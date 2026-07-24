//! SQLite-first content repository.
//!
//! The connection uses WAL for file-backed databases, foreign keys, a busy
//! timeout, and short transactions. External work must never run while a
//! repository transaction is held.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
    str::FromStr,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use chrono::{DateTime, Utc};
use osb_kernel::{
    AI_PROPOSAL_AUDIT_SCHEMA_VERSION, Ai2AiEnvelope, AiProposalAuditRecord, CONTENT_SCHEMA_VERSION,
    ContentRepository, DocumentSnapshot, DocumentStatus, NewDocument, ProposedRevision,
    PublicAuthorship, PublicAuthorshipKind, RepositoryError, RevisionActor, RevisionActorKind,
    RevisionSnapshot, content_hash_with_ai_summary,
};
use rusqlite::{
    Connection, MAIN_DB, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

/// Latest schema version required by both mutable and delivery-only runtimes.
pub const DATABASE_SCHEMA_VERSION: u64 = 10;

/// Public home projections cap each independent document pool at this size.
/// The Series/category pool reserves one item for every active, non-empty
/// Series before assigning remaining capacity in Series order.
const HOME_FEED_MAX_SECTION_ITEMS: usize = 500;

pub struct SqliteRepository {
    connection: Mutex<Connection>,
}

/// An authenticated community member as stored by the control plane.
///
/// `email` and `password_phc` are deliberately excluded from serialization so
/// accidentally returning this record from an HTTP handler cannot expose
/// credentials. They remain readable by the authentication layer.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRecord {
    pub id: Uuid,
    #[serde(skip_serializing)]
    pub email: String,
    pub handle: String,
    pub display_name: String,
    #[serde(skip_serializing)]
    pub password_phc: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl fmt::Debug for UserRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserRecord")
            .field("id", &self.id)
            .field("handle", &self.handle)
            .field("display_name", &self.display_name)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish_non_exhaustive()
    }
}

/// Backwards-friendly descriptive alias for callers that prefer domain names.
pub type CommunityUser = UserRecord;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub auth_epoch: u64,
    pub auth_method: SessionAuthMethod,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub type CommunitySession = SessionRecord;

/// The one human administration mechanism selected for this installation.
///
/// This value is persisted with the primary owner binding so two replicas with
/// contradictory configuration cannot silently take turns issuing sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminAuthMode {
    AccessKey,
    External,
    Disabled,
}

impl AdminAuthMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccessKey => "access_key",
            Self::External => "external",
            Self::Disabled => "disabled",
        }
    }

    const fn session_method(self) -> Option<SessionAuthMethod> {
        match self {
            Self::AccessKey => Some(SessionAuthMethod::AccessKey),
            Self::External => Some(SessionAuthMethod::External),
            Self::Disabled => None,
        }
    }
}

impl FromStr for AdminAuthMode {
    type Err = RepositoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "access_key" => Ok(Self::AccessKey),
            "external" => Ok(Self::External),
            "disabled" => Ok(Self::Disabled),
            other => Err(RepositoryError::Storage(format!(
                "unknown admin authentication mode {other}"
            ))),
        }
    }
}

/// Provenance attached to a revocable browser session. `Legacy` is retained so
/// the pre-v6 local-account API remains source-compatible while new owner
/// authentication converges on the same session table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAuthMethod {
    Legacy,
    AccessKey,
    External,
}

impl SessionAuthMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::AccessKey => "access_key",
            Self::External => "external",
        }
    }
}

impl FromStr for SessionAuthMethod {
    type Err = RepositoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "legacy" => Ok(Self::Legacy),
            "access_key" => Ok(Self::AccessKey),
            "external" => Ok(Self::External),
            other => Err(RepositoryError::Storage(format!(
                "unknown session authentication method {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminControlPlaneRecord {
    pub primary_site_id: Uuid,
    pub owner_user_id: Uuid,
    pub auth_mode: AdminAuthMode,
    pub auth_epoch: u64,
    pub setup_complete: bool,
    pub binding_fingerprint: [u8; 32],
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Human-readable values used only when a fresh database has no primary site.
/// The persistent owner identity is generated internally and deliberately has
/// no usable local password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryOwnerBootstrap {
    pub site_id: Uuid,
    pub site_handle: String,
    pub site_title: String,
    pub site_description: Option<String>,
    pub owner_display_name: String,
    pub theme_profile: ThemeProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentityRecord {
    pub adapter: String,
    pub issuer: String,
    pub subject_hash: [u8; 32],
    pub user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryOwnerSession {
    pub session: SessionRecord,
    pub user: UserRecord,
    pub site: SiteRecord,
}

/// One typed target in the installation-wide home curation order.
///
/// Post targets reference documents rather than revisions so a deliberate
/// republish updates the visible card. Series targets keep the collection as
/// the home unit and resolve its exact currently-published members at read
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HomePinTarget {
    Post { id: Uuid },
    Series { id: Uuid },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HomePinRecord {
    pub slot: u8,
    pub target: HomePinTarget,
    pub pinned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HomeCategorySectionRecords {
    pub category: CategoryRecord,
    pub items: Vec<DocumentSnapshot>,
}

/// One first-class ordered series projected with its currently published
/// members. A series extends a category so existing public routes and
/// revision-scoped placement remain authoritative.
#[derive(Debug, Clone, PartialEq)]
pub struct HomeSeriesSectionRecords {
    pub series: SeriesRecord,
    pub items: Vec<DocumentSnapshot>,
}

/// One first-class public home unit. Presentation may collapse series while a
/// standalone post remains a single, directly-readable card.
#[derive(Debug, Clone, PartialEq)]
pub enum HomeUnitRecords {
    Post(DocumentSnapshot),
    Series(HomeSeriesSectionRecords),
}

#[derive(Debug, Clone, PartialEq)]
pub struct HomeFeedRecords {
    pub units: Vec<HomeUnitRecords>,
    /// Compatibility projection for clients predating typed home units.
    pub pinned: Vec<DocumentSnapshot>,
    /// Compatibility projection for clients predating typed home units.
    pub recent: Vec<DocumentSnapshot>,
    /// Compatibility projection for clients predating typed home units.
    pub category_sections: Vec<HomeCategorySectionRecords>,
    /// Compatibility projection for clients predating typed home units.
    pub series_sections: Vec<HomeSeriesSectionRecords>,
}

/// Operator intent for SQLite's local WAL durability/latency trade-off.
/// Even `Fast` keeps `synchronous=NORMAL`; unsafe `OFF` is never exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteDurabilityProfile {
    Durable,
    Balanced,
    Fast,
}

/// The complete built-in theme allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeProfile {
    Paper,
    Ink,
    Forest,
    Terminal,
}

impl ThemeProfile {
    pub const ALL: [Self; 4] = [Self::Paper, Self::Ink, Self::Forest, Self::Terminal];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Ink => "ink",
            Self::Forest => "forest",
            Self::Terminal => "terminal",
        }
    }
}

impl fmt::Display for ThemeProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ThemeProfile {
    type Err = RepositoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "paper" => Ok(Self::Paper),
            "ink" => Ok(Self::Ink),
            "forest" => Ok(Self::Forest),
            "terminal" => Ok(Self::Terminal),
            _ => Err(RepositoryError::Validation(
                "theme profile must be one of paper, ink, forest, or terminal".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteRecord {
    pub id: Uuid,
    pub handle: String,
    pub title: String,
    pub description: Option<String>,
    pub owner_user_id: Uuid,
    pub theme_profile: ThemeProfile,
    pub theme_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_css: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Closed, content-free site metadata used by bounded control-plane listings.
///
/// Keep this projection separate from [`SiteRecord`]: callers that only need
/// navigation metadata must not load owner identity, descriptions, theme
/// revisions, or custom CSS for every row in a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteMetadataRecord {
    pub id: Uuid,
    pub handle: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub type CommunitySite = SiteRecord;

/// Lifecycle state for a category landing page.
///
/// Archiving is deliberately non-destructive: already-published revisions keep
/// their category placement and routes, while new assignments and publications
/// into the category are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CategoryStatus {
    Active,
    Archived,
}

impl CategoryStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }
}

impl FromStr for CategoryStatus {
    type Err = RepositoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "archived" => Ok(Self::Archived),
            other => Err(RepositoryError::Storage(format!(
                "unknown category status {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryRecord {
    pub id: Uuid,
    pub site_id: Uuid,
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
    pub theme_profile: Option<ThemeProfile>,
    pub status: CategoryStatus,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Closed category metadata for paginated navigation projections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryMetadataRecord {
    pub id: Uuid,
    pub site_id: Uuid,
    pub slug: String,
    pub title: String,
    pub status: CategoryStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Metadata for a document's exact current revision and placement.
///
/// Source Markdown, embeds, authorship, hashes, and AI provenance are omitted
/// deliberately so a tree page cannot accidentally hydrate private content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentDocumentMetadataRecord {
    pub id: Uuid,
    pub site_id: Uuid,
    pub status: DocumentStatus,
    pub current_revision_id: Uuid,
    pub published_revision_id: Option<Uuid>,
    pub title: String,
    pub slug: String,
    pub revision_number: u64,
    pub category_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Closed revision metadata for paginated history navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionMetadataRecord {
    pub id: Uuid,
    pub document_id: Uuid,
    pub revision_number: u64,
    pub slug: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCategoryInput {
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
    pub theme_profile: Option<ThemeProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCategoryInput {
    pub title: String,
    pub description: Option<String>,
    pub theme_profile: Option<ThemeProfile>,
}

/// Input for creating a series and its immutable-slug backing category in one
/// transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSeriesInput {
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
    pub theme_profile: Option<ThemeProfile>,
}

/// A first-class ordered collection backed one-to-one by a category.
///
/// Category metadata is joined into this closed projection instead of being
/// duplicated in the `series` table. This preserves the existing category
/// route, archive lifecycle, and theme behavior for promoted categories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesRecord {
    pub id: Uuid,
    pub site_id: Uuid,
    pub category_id: Uuid,
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
    pub theme_profile: Option<ThemeProfile>,
    pub status: CategoryStatus,
    pub home_position: u64,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Stable ordering metadata for one document in one series.
///
/// Historical rows are retained when a later revision moves the document.
/// Public reads always join through the exact published revision's category,
/// so retaining them makes publishing an older revision safe without leaking
/// draft placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesItemRecord {
    pub series_id: Uuid,
    pub site_id: Uuid,
    pub document_id: Uuid,
    pub position: u64,
    pub added_at: DateTime<Utc>,
}

/// Category placement belongs to an immutable revision rather than a mutable
/// document. This lets delivery keep showing the published category while a
/// newer Studio revision is moved elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionCategoryPlacement {
    pub revision_id: Uuid,
    pub document_id: Uuid,
    pub site_id: Uuid,
    pub category_id: Option<Uuid>,
    pub assigned_by_user_id: Option<Uuid>,
    pub assigned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteMembershipRole {
    Owner,
    Editor,
    Writer,
}

impl SiteMembershipRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Writer => "writer",
        }
    }

    pub const fn can_write(self) -> bool {
        matches!(self, Self::Owner | Self::Editor | Self::Writer)
    }

    pub const fn is_owner(self) -> bool {
        matches!(self, Self::Owner)
    }
}

impl fmt::Display for SiteMembershipRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SiteMembershipRole {
    type Err = RepositoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "owner" => Ok(Self::Owner),
            "editor" => Ok(Self::Editor),
            "writer" => Ok(Self::Writer),
            other => Err(RepositoryError::Storage(format!(
                "unknown site membership role {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteMembershipRecord {
    pub site_id: Uuid,
    pub user_id: Uuid,
    pub role: SiteMembershipRole,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentStatus {
    Pending,
    Approved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentRecord {
    pub id: Uuid,
    pub site_id: Uuid,
    pub document_id: Uuid,
    pub author_user_id: Uuid,
    pub source_markdown: String,
    pub status: CommentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub type CommunityComment = CommentRecord;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteExport {
    pub schema_version: String,
    pub site_id: Uuid,
    #[serde(default)]
    pub categories: Vec<CategoryRecord>,
    #[serde(default)]
    pub series: Vec<SeriesRecord>,
    #[serde(default)]
    pub series_items: Vec<SeriesItemRecord>,
    pub documents: Vec<ExportedDocument>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedDocument {
    pub current: DocumentSnapshot,
    pub revisions: Vec<RevisionSnapshot>,
    #[serde(default)]
    pub revision_category_placements: Vec<RevisionCategoryPlacement>,
    pub ai_proposals: Vec<AiProposalAuditRecord>,
    pub routes: Vec<ExportedRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedRoute {
    pub path: String,
    pub canonical: bool,
    pub created_at: DateTime<Utc>,
}

/// One category declaration in an offline import batch.
///
/// Existing active categories are reused only when their portable metadata is
/// identical. This prevents a retry from silently changing a live navigation
/// tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineImportCategory {
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
}

/// A historical route that should permanently resolve to the imported post's
/// canonical route. `created_at` is retained in exports and backup restores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineImportAlias {
    pub path: String,
    pub created_at: DateTime<Utc>,
}

/// Fully materialized post input for an atomic offline import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineImportPost {
    pub source_id: String,
    pub title: String,
    pub slug: String,
    pub source_markdown: String,
    pub created_at: DateTime<Utc>,
    pub author_id: String,
    pub author_display_name: String,
    pub primary_category: String,
    pub human_reviewed: bool,
    pub aliases: Vec<OfflineImportAlias>,
}

/// A batch is committed or rolled back as a unit. `source` namespaces stable
/// source IDs so retry safety is independent from filenames and post slugs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineImportBatch {
    pub source: String,
    pub owner_display_name: String,
    pub categories: Vec<OfflineImportCategory>,
    pub posts: Vec<OfflineImportPost>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineImportPostStatus {
    Imported,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineImportPostOutcome {
    pub source_id: String,
    pub canonical_path: String,
    pub status: OfflineImportPostStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineImportReport {
    pub dry_run: bool,
    pub owner_display_name_updated: bool,
    pub categories_created: usize,
    pub categories_reused: usize,
    pub posts_imported: usize,
    pub posts_unchanged: usize,
    pub aliases_created: usize,
    pub posts: Vec<OfflineImportPostOutcome>,
}

impl SqliteRepository {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let connection = Connection::open(path).map_err(storage_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(storage_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(storage_error)?;
        let repository = Self {
            connection: Mutex::new(connection),
        };
        repository.migrate()?;
        Ok(repository)
    }

    pub fn open_in_memory() -> Result<Self, RepositoryError> {
        let connection = Connection::open_in_memory().map_err(storage_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(storage_error)?;
        let repository = Self {
            connection: Mutex::new(connection),
        };
        repository.migrate()?;
        Ok(repository)
    }

    /// Opens an already-migrated database without creating files, selecting a
    /// journal mode, or running migrations. This is the delivery-node path for
    /// read-only database mounts.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let canonical = path.as_ref().canonicalize().map_err(storage_error)?;
        let mut wal_name = canonical.as_os_str().to_os_string();
        wal_name.push("-wal");
        let wal_path = std::path::PathBuf::from(wal_name);
        if wal_path
            .metadata()
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false)
        {
            return Err(RepositoryError::Storage(format!(
                "delivery-only database has an uncheckpointed WAL: {}",
                wal_path.display()
            )));
        }
        let mut uri = Url::from_file_path(&canonical).map_err(|_| {
            RepositoryError::Storage("database path cannot be represented as a file URI".into())
        })?;
        uri.query_pairs_mut()
            .append_pair("mode", "ro")
            .append_pair("immutable", "1");
        let connection = Connection::open_with_flags(
            uri.as_str(),
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(storage_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(storage_error)?;
        // Fail startup clearly when a delivery node is pointed at an old DB;
        // it must never self-migrate a read-only deployment artifact.
        let migrated = connection
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = ?1",
                [DATABASE_SCHEMA_VERSION as i64],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if !migrated {
            return Err(RepositoryError::Storage(format!(
                "delivery-only database must be migrated through schema version {DATABASE_SCHEMA_VERSION}"
            )));
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn migrate(&self) -> Result<(), RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        transaction
            .execute_batch(MIGRATION_1)
            .map_err(storage_error)?;
        transaction
            .execute_batch(MIGRATION_2)
            .map_err(storage_error)?;
        transaction
            .execute_batch(MIGRATION_3)
            .map_err(storage_error)?;
        transaction
            .execute_batch(MIGRATION_4)
            .map_err(storage_error)?;
        let has_migration_5 = transaction
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = 5",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if !has_migration_5 {
            transaction
                .execute_batch(MIGRATION_5)
                .map_err(storage_error)?;
        }
        let has_migration_6 = transaction
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = 6",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if !has_migration_6 {
            transaction
                .execute_batch(MIGRATION_6)
                .map_err(storage_error)?;
        }
        let has_migration_7 = transaction
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = 7",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if !has_migration_7 {
            transaction
                .execute_batch(MIGRATION_7)
                .map_err(storage_error)?;
        }
        let has_migration_8 = transaction
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = 8",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if !has_migration_8 {
            transaction
                .execute_batch(MIGRATION_8)
                .map_err(storage_error)?;
        }
        let has_migration_9 = transaction
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = 9",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if !has_migration_9 {
            transaction
                .execute_batch(MIGRATION_9)
                .map_err(storage_error)?;
        }
        let has_migration_10 = transaction
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = 10",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if !has_migration_10 {
            transaction
                .execute_batch(MIGRATION_10)
                .map_err(storage_error)?;
        }
        transaction.commit().map_err(storage_error)
    }

    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<(), RepositoryError> {
        let connection = self.lock()?;
        let mut destination = Connection::open(destination).map_err(storage_error)?;
        let backup =
            rusqlite::backup::Backup::new(&connection, &mut destination).map_err(storage_error)?;
        backup
            .run_to_completion(16, Duration::from_millis(10), None)
            .map_err(storage_error)
    }

    pub fn apply_durability_profile(
        &self,
        profile: SqliteDurabilityProfile,
    ) -> Result<(), RepositoryError> {
        let connection = self.lock()?;
        let (synchronous, auto_checkpoint_pages) = match profile {
            SqliteDurabilityProfile::Durable => ("FULL", 256_i64),
            SqliteDurabilityProfile::Balanced => ("NORMAL", 1_000_i64),
            SqliteDurabilityProfile::Fast => ("NORMAL", 4_000_i64),
        };
        connection
            .pragma_update(None, "synchronous", synchronous)
            .map_err(storage_error)?;
        connection
            .pragma_update(None, "wal_autocheckpoint", auto_checkpoint_pages)
            .map_err(storage_error)?;
        Ok(())
    }

    pub fn export_site(&self, site_id: Uuid) -> Result<SiteExport, RepositoryError> {
        let connection = self.lock()?;
        let categories = {
            let mut statement = connection
                .prepare(
                    "SELECT id, site_id, slug, title, description, theme_profile, status,
                            created_by_user_id, created_at, updated_at
                     FROM categories
                     WHERE site_id = ?1
                     ORDER BY created_at, id",
                )
                .map_err(storage_error)?;
            statement
                .query_map(params![site_id.to_string()], stored_category_row)
                .map_err(storage_error)?
                .map(|row| row.map_err(storage_error).and_then(parse_category_row))
                .collect::<Result<Vec<_>, _>>()?
        };
        let series = list_series_with_connection(&connection, site_id, true, 500)?;
        let series_items = {
            let mut statement = connection
                .prepare(
                    "SELECT item.series_id, item.site_id, item.document_id,
                            item.position, item.added_at
                     FROM series_items item
                     JOIN series
                       ON series.id = item.series_id AND series.site_id = item.site_id
                     WHERE item.site_id = ?1
                     ORDER BY series.home_position, item.series_id,
                              item.position, item.document_id",
                )
                .map_err(storage_error)?;
            statement
                .query_map(params![site_id.to_string()], stored_series_item_row)
                .map_err(storage_error)?
                .map(|row| row.map_err(storage_error).and_then(parse_series_item_row))
                .collect::<Result<Vec<_>, _>>()?
        };
        let document_ids = {
            let mut statement = connection
                .prepare("SELECT id FROM documents WHERE site_id = ?1 ORDER BY created_at")
                .map_err(storage_error)?;
            statement
                .query_map(params![site_id.to_string()], |row| row.get::<_, String>(0))
                .map_err(storage_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage_error)?
        };
        let mut documents = Vec::with_capacity(document_ids.len());
        for document_id in document_ids {
            let document_id = parse_uuid(&document_id)?;
            let current = load_document(&connection, document_id, RevisionSelector::Current)?;
            let revisions = {
                let mut statement = connection
                    .prepare(
                        "SELECT snapshot_json FROM revisions
                         WHERE document_id = ?1 ORDER BY revision_number",
                    )
                    .map_err(storage_error)?;
                statement
                    .query_map(params![document_id.to_string()], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(storage_error)?
                    .map(|row| {
                        row.map_err(storage_error).and_then(|json| {
                            serde_json::from_str::<RevisionSnapshot>(&json).map_err(storage_error)
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            let routes = {
                let mut statement = connection
                    .prepare(
                        "SELECT path, is_canonical, created_at FROM routes
                         WHERE document_id = ?1 ORDER BY created_at, path",
                    )
                    .map_err(storage_error)?;
                statement
                    .query_map(params![document_id.to_string()], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(storage_error)?
                    .map(|row| {
                        let (path, canonical, created_at) = row.map_err(storage_error)?;
                        Ok(ExportedRoute {
                            path,
                            canonical,
                            created_at: parse_datetime(&created_at)?,
                        })
                    })
                    .collect::<Result<Vec<_>, RepositoryError>>()?
            };
            let revision_category_placements = revisions
                .iter()
                .map(|revision| {
                    load_revision_category_placement(&connection, site_id, document_id, revision.id)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let ai_proposals = load_ai_proposals(
                &connection,
                document_id,
                usize::MAX,
                AuditOrder::OldestFirst,
            )?;
            documents.push(ExportedDocument {
                current,
                revisions,
                revision_category_placements,
                ai_proposals,
                routes,
            });
        }
        Ok(SiteExport {
            schema_version: "open-soverign-blog-export/4".into(),
            site_id,
            categories,
            series,
            series_items,
            documents,
        })
    }

    pub fn create_user(
        &self,
        email: &str,
        handle: &str,
        display_name: &str,
        password_phc: &str,
    ) -> Result<UserRecord, RepositoryError> {
        let email = normalize_email(email)?;
        let handle = normalize_handle(handle, "user handle")?;
        let display_name = validate_required_text(display_name, "display name", 100)?;
        validate_password_phc(password_phc)?;

        let connection = self.lock()?;
        let id = Uuid::now_v7();
        let now = Utc::now();
        connection
            .execute(
                "INSERT INTO users (
                    id, email, handle, display_name, password_phc, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![
                    id.to_string(),
                    email,
                    handle,
                    display_name,
                    password_phc,
                    now.to_rfc3339(),
                ],
            )
            .map_err(map_community_constraint_error)?;
        load_user_by_id(&connection, id)
    }

    pub fn get_user_by_id(&self, id: Uuid) -> Result<UserRecord, RepositoryError> {
        let connection = self.lock()?;
        load_user_by_id(&connection, id)
    }

    pub fn get_user_by_handle(&self, handle: &str) -> Result<UserRecord, RepositoryError> {
        let handle = normalize_handle(handle, "user handle")?;
        let connection = self.lock()?;
        load_user_by_column(&connection, "handle", &handle)
    }

    pub fn find_user_by_email(&self, email: &str) -> Result<UserRecord, RepositoryError> {
        let email = normalize_email(email)?;
        let connection = self.lock()?;
        load_user_by_column(&connection, "email", &email)
    }

    /// Atomically provisions the primary owner and site on an empty database,
    /// then persists the immutable authentication binding for this deployment.
    /// Existing sites are never guessed or re-parented by this operation.
    pub fn provision_primary_owner_site(
        &self,
        bootstrap: &PrimaryOwnerBootstrap,
        auth_mode: AdminAuthMode,
        binding_fingerprint: &[u8],
    ) -> Result<AdminControlPlaneRecord, RepositoryError> {
        validate_fingerprint(binding_fingerprint)?;
        let site_handle = normalize_handle(&bootstrap.site_handle, "site handle")?;
        let site_title = validate_required_text(&bootstrap.site_title, "site title", 200)?;
        let site_description = validate_optional_text(
            bootstrap.site_description.as_deref(),
            "site description",
            2_000,
        )?;
        let owner_display_name =
            validate_required_text(&bootstrap.owner_display_name, "owner display name", 100)?;

        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        if let Some(existing) = load_admin_control_plane_optional(&transaction)? {
            validate_control_plane_binding(
                &existing,
                bootstrap.site_id,
                auth_mode,
                binding_fingerprint,
            )?;
            ensure_site_owner(
                &transaction,
                existing.owner_user_id,
                existing.primary_site_id,
            )?;
            transaction.commit().map_err(storage_error)?;
            return Ok(existing);
        }

        let (site, setup_complete) = match load_site_by_id(&transaction, bootstrap.site_id, None) {
            Ok(site) => (site, true),
            Err(RepositoryError::NotFound) => {
                let site_count: i64 = transaction
                    .query_row("SELECT COUNT(*) FROM sites", [], |row| row.get(0))
                    .map_err(storage_error)?;
                if site_count != 0 {
                    return Err(RepositoryError::Validation(
                        "refusing to guess a primary owner because another site already exists"
                            .into(),
                    ));
                }

                let compact = bootstrap.site_id.simple().to_string();
                let owner_handle = format!("owner-{compact}");
                let owner_email = format!("{owner_handle}@localhost");
                let now = Utc::now();
                transaction
                    .execute(
                        "INSERT INTO users (
                            id, email, handle, display_name, password_phc, created_at, updated_at
                         ) VALUES (?1, ?2, ?3, ?4,
                                   '$argon2id$disabled-for-primary-owner', ?5, ?5)",
                        params![
                            bootstrap.site_id.to_string(),
                            owner_email,
                            owner_handle,
                            owner_display_name,
                            now.to_rfc3339(),
                        ],
                    )
                    .map_err(map_community_constraint_error)?;
                transaction
                    .execute(
                        "INSERT INTO sites (
                            id, handle, title, description, current_theme_revision,
                            created_at, updated_at
                         ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
                        params![
                            bootstrap.site_id.to_string(),
                            site_handle,
                            site_title,
                            site_description,
                            now.to_rfc3339(),
                        ],
                    )
                    .map_err(map_community_constraint_error)?;
                transaction
                    .execute(
                        "INSERT INTO site_memberships (site_id, user_id, role, created_at)
                         VALUES (?1, ?1, 'owner', ?2)",
                        params![bootstrap.site_id.to_string(), now.to_rfc3339()],
                    )
                    .map_err(map_community_constraint_error)?;
                transaction
                    .execute(
                        "INSERT INTO site_theme_revisions (
                            site_id, revision, profile, custom_css, created_by_user_id, created_at
                         ) VALUES (?1, 1, ?2, NULL, ?1, ?3)",
                        params![
                            bootstrap.site_id.to_string(),
                            bootstrap.theme_profile.as_str(),
                            now.to_rfc3339(),
                        ],
                    )
                    .map_err(map_community_constraint_error)?;
                (
                    load_site_by_id(&transaction, bootstrap.site_id, None)?,
                    false,
                )
            }
            Err(error) => return Err(error),
        };
        let record = insert_admin_control_plane(
            &transaction,
            site.id,
            site.owner_user_id,
            auth_mode,
            binding_fingerprint,
            setup_complete,
        )?;
        transaction.commit().map_err(storage_error)?;
        Ok(record)
    }

    /// Reconciles an already-provisioned primary site with this replica's
    /// configuration. A differing mode or fingerprint is a hard error rather
    /// than an implicit credential rotation.
    pub fn reconcile_admin_control_plane(
        &self,
        primary_site_id: Uuid,
        auth_mode: AdminAuthMode,
        binding_fingerprint: &[u8],
    ) -> Result<AdminControlPlaneRecord, RepositoryError> {
        validate_fingerprint(binding_fingerprint)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        if let Some(existing) = load_admin_control_plane_optional(&transaction)? {
            validate_control_plane_binding(
                &existing,
                primary_site_id,
                auth_mode,
                binding_fingerprint,
            )?;
            ensure_site_owner(
                &transaction,
                existing.owner_user_id,
                existing.primary_site_id,
            )?;
            transaction.commit().map_err(storage_error)?;
            return Ok(existing);
        }
        let site = load_site_by_id(&transaction, primary_site_id, None)?;
        let record = insert_admin_control_plane(
            &transaction,
            site.id,
            site.owner_user_id,
            auth_mode,
            binding_fingerprint,
            true,
        )?;
        transaction.commit().map_err(storage_error)?;
        Ok(record)
    }

    /// Explicitly rotates the configured administrator authentication binding.
    ///
    /// A matching target is a no-op so multiple replicas may safely start with
    /// the same one-shot rotation flag. A real change advances the epoch,
    /// revokes every non-member session, and removes external identity bindings
    /// in the same immediate transaction. The primary site and owner are never
    /// changed by credential rotation.
    pub fn rotate_admin_control_plane(
        &self,
        primary_site_id: Uuid,
        auth_mode: AdminAuthMode,
        binding_fingerprint: &[u8],
    ) -> Result<AdminControlPlaneRecord, RepositoryError> {
        validate_fingerprint(binding_fingerprint)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let existing = load_admin_control_plane(&transaction)?;
        if existing.primary_site_id != primary_site_id {
            return Err(RepositoryError::Validation(
                "administrator authentication rotation cannot change the primary site".into(),
            ));
        }
        ensure_site_owner(
            &transaction,
            existing.owner_user_id,
            existing.primary_site_id,
        )?;
        if existing.auth_mode == auth_mode
            && existing.binding_fingerprint.as_slice() == binding_fingerprint
        {
            transaction.commit().map_err(storage_error)?;
            return Ok(existing);
        }

        let next_epoch = existing.auth_epoch.checked_add(1).ok_or_else(|| {
            RepositoryError::Validation("administrator authentication epoch is exhausted".into())
        })?;
        let next_epoch = i64::try_from(next_epoch).map_err(|_| {
            RepositoryError::Validation("administrator authentication epoch is too large".into())
        })?;
        let now = Utc::now().to_rfc3339();
        let updated = transaction
            .execute(
                "UPDATE admin_control_plane
                 SET auth_mode = ?1, auth_epoch = ?2, binding_fingerprint = ?3,
                     updated_at = ?4
                 WHERE singleton = 1 AND auth_epoch = ?5",
                params![
                    auth_mode.as_str(),
                    next_epoch,
                    binding_fingerprint,
                    now,
                    i64::try_from(existing.auth_epoch).map_err(|_| {
                        RepositoryError::Storage(
                            "stored administrator authentication epoch is too large".into(),
                        )
                    })?,
                ],
            )
            .map_err(storage_error)?;
        if updated != 1 {
            return Err(RepositoryError::Storage(
                "administrator authentication binding changed concurrently".into(),
            ));
        }
        transaction
            .execute(
                "UPDATE sessions
                 SET revoked_at = COALESCE(revoked_at, ?1)
                 WHERE auth_method != 'legacy'",
                params![now],
            )
            .map_err(storage_error)?;
        transaction
            .execute("DELETE FROM external_identities", [])
            .map_err(storage_error)?;
        let rotated = load_admin_control_plane(&transaction)?;
        transaction.commit().map_err(storage_error)?;
        Ok(rotated)
    }

    pub fn get_admin_control_plane(&self) -> Result<AdminControlPlaneRecord, RepositoryError> {
        let connection = self.lock()?;
        load_admin_control_plane(&connection)
    }

    /// Completes the one-time metadata and theme selection for a freshly
    /// provisioned primary owner site.
    pub fn complete_primary_owner_setup(
        &self,
        owner_user_id: Uuid,
        handle: &str,
        title: &str,
        description: Option<&str>,
        theme_profile: ThemeProfile,
    ) -> Result<SiteRecord, RepositoryError> {
        let handle = normalize_handle(handle, "site handle")?;
        let title = validate_required_text(title, "site title", 200)?;
        let description = validate_optional_text(description, "site description", 2_000)?;

        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let control = load_admin_control_plane(&transaction)?;
        if control.owner_user_id != owner_user_id {
            return Err(RepositoryError::NotFound);
        }
        if control.setup_complete {
            return Err(RepositoryError::Validation(
                "primary owner setup is already complete".into(),
            ));
        }
        ensure_site_owner(&transaction, control.owner_user_id, control.primary_site_id)?;

        let now = Utc::now();
        transaction
            .execute(
                "UPDATE sites
                 SET handle = ?1, title = ?2, description = ?3, updated_at = ?4
                 WHERE id = ?5",
                params![
                    handle,
                    title,
                    description,
                    now.to_rfc3339(),
                    control.primary_site_id.to_string(),
                ],
            )
            .map_err(map_community_constraint_error)?;
        append_site_appearance_revision(
            &transaction,
            control.owner_user_id,
            control.primary_site_id,
            theme_profile,
            None,
        )?;
        let updated = transaction
            .execute(
                "UPDATE admin_control_plane
                 SET setup_complete = 1, updated_at = ?1
                 WHERE singleton = 1 AND setup_complete = 0",
                params![Utc::now().to_rfc3339()],
            )
            .map_err(storage_error)?;
        if updated != 1 {
            return Err(RepositoryError::Validation(
                "primary owner setup is already complete".into(),
            ));
        }
        let site = load_site_by_id(
            &transaction,
            control.primary_site_id,
            Some(control.owner_user_id),
        )?;
        transaction.commit().map_err(storage_error)?;
        Ok(site)
    }

    /// Idempotently binds a cryptographically verified external subject to the
    /// already-selected primary owner. Authorization remains in memberships.
    pub fn bind_external_identity(
        &self,
        adapter: &str,
        issuer: &str,
        subject_hash: &[u8],
        binding_fingerprint: &[u8],
    ) -> Result<ExternalIdentityRecord, RepositoryError> {
        let adapter = validate_external_adapter(adapter)?;
        let issuer = validate_external_issuer(issuer)?;
        validate_subject_hash(subject_hash)?;
        validate_fingerprint(binding_fingerprint)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let control = load_admin_control_plane(&transaction)?;
        if control.auth_mode != AdminAuthMode::External
            || control.binding_fingerprint.as_slice() != binding_fingerprint
        {
            return Err(RepositoryError::Validation(
                "external identity binding does not match the active administrator authentication binding"
                    .into(),
            ));
        }
        ensure_site_owner(&transaction, control.owner_user_id, control.primary_site_id)?;
        let now = Utc::now();
        if let Some(existing) =
            load_external_identity_optional(&transaction, &adapter, &issuer, subject_hash)?
        {
            if existing.user_id != control.owner_user_id {
                return Err(RepositoryError::Validation(
                    "external identity is already bound to a different user".into(),
                ));
            }
            transaction
                .execute(
                    "UPDATE external_identities SET last_seen_at = ?1
                     WHERE adapter = ?2 AND issuer = ?3 AND subject_hash = ?4",
                    params![now.to_rfc3339(), adapter, issuer, subject_hash],
                )
                .map_err(storage_error)?;
        } else {
            transaction
                .execute(
                    "INSERT INTO external_identities (
                        adapter, issuer, subject_hash, user_id, created_at, last_seen_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    params![
                        adapter,
                        issuer,
                        subject_hash,
                        control.owner_user_id.to_string(),
                        now.to_rfc3339(),
                    ],
                )
                .map_err(map_community_constraint_error)?;
        }
        let record = load_external_identity(&transaction, &adapter, &issuer, subject_hash)?;
        transaction.commit().map_err(storage_error)?;
        Ok(record)
    }

    pub fn get_external_identity(
        &self,
        adapter: &str,
        issuer: &str,
        subject_hash: &[u8],
    ) -> Result<ExternalIdentityRecord, RepositoryError> {
        let adapter = validate_external_adapter(adapter)?;
        let issuer = validate_external_issuer(issuer)?;
        validate_subject_hash(subject_hash)?;
        let connection = self.lock()?;
        load_external_identity(&connection, &adapter, &issuer, subject_hash)
    }

    /// Issues an opaque browser session for the configured primary owner after
    /// the caller has completed access-key or external verification.
    pub fn create_primary_owner_session(
        &self,
        token_hash: &[u8],
        expires_at: DateTime<Utc>,
        auth_method: SessionAuthMethod,
        binding_fingerprint: &[u8],
    ) -> Result<SessionRecord, RepositoryError> {
        validate_token_hash(token_hash)?;
        validate_session_expiry(expires_at)?;
        validate_fingerprint(binding_fingerprint)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let control = load_admin_control_plane(&transaction)?;
        if control.auth_mode.session_method() != Some(auth_method)
            || control.binding_fingerprint.as_slice() != binding_fingerprint
        {
            return Err(RepositoryError::Validation(
                "session issuance does not match the active administrator authentication binding"
                    .into(),
            ));
        }
        ensure_site_owner(&transaction, control.owner_user_id, control.primary_site_id)?;
        let session = insert_session(
            &transaction,
            control.owner_user_id,
            token_hash,
            expires_at,
            control.auth_epoch,
            auth_method,
        )?;
        transaction.commit().map_err(storage_error)?;
        Ok(session)
    }

    pub fn get_primary_owner_session(
        &self,
        token_hash: &[u8],
    ) -> Result<PrimaryOwnerSession, RepositoryError> {
        validate_token_hash(token_hash)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(storage_error)?;
        let session = load_active_session_by_hash(&transaction, token_hash)?;
        let control = load_admin_control_plane(&transaction)?;
        if session.user_id != control.owner_user_id
            || session.auth_epoch != control.auth_epoch
            || control.auth_mode.session_method() != Some(session.auth_method)
        {
            return Err(RepositoryError::NotFound);
        }
        let owner_session = PrimaryOwnerSession {
            user: load_user_by_id(&transaction, control.owner_user_id)?,
            site: load_site_by_id(&transaction, control.primary_site_id, None)?,
            session,
        };
        transaction.commit().map_err(storage_error)?;
        Ok(owner_session)
    }

    /// Stores only a 32-byte SHA-256 digest of the opaque browser credential.
    pub fn create_session(
        &self,
        user_id: Uuid,
        token_hash: &[u8],
        expires_at: DateTime<Utc>,
    ) -> Result<SessionRecord, RepositoryError> {
        validate_token_hash(token_hash)?;
        validate_session_expiry(expires_at)?;
        let connection = self.lock()?;
        insert_session(
            &connection,
            user_id,
            token_hash,
            expires_at,
            0,
            SessionAuthMethod::Legacy,
        )
    }

    /// Returns only a currently valid session. Expired and revoked credentials
    /// are indistinguishable from unknown credentials.
    pub fn get_session(&self, token_hash: &[u8]) -> Result<SessionRecord, RepositoryError> {
        validate_token_hash(token_hash)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(storage_error)?;
        let session = load_active_session_by_hash(&transaction, token_hash)?;
        if session.auth_method != SessionAuthMethod::Legacy {
            let control = load_admin_control_plane(&transaction)?;
            if session.user_id != control.owner_user_id
                || session.auth_epoch != control.auth_epoch
                || control.auth_mode.session_method() != Some(session.auth_method)
            {
                return Err(RepositoryError::NotFound);
            }
        }
        transaction.commit().map_err(storage_error)?;
        Ok(session)
    }

    pub fn revoke_session(&self, token_hash: &[u8]) -> Result<bool, RepositoryError> {
        validate_token_hash(token_hash)?;
        let connection = self.lock()?;
        let changed = connection
            .execute(
                "UPDATE sessions SET revoked_at = ?1
                 WHERE token_hash = ?2 AND revoked_at IS NULL",
                params![Utc::now().to_rfc3339(), token_hash],
            )
            .map_err(storage_error)?;
        Ok(changed > 0)
    }

    pub fn create_site(
        &self,
        owner_user_id: Uuid,
        handle: &str,
        title: &str,
        description: Option<&str>,
        theme_profile: ThemeProfile,
    ) -> Result<SiteRecord, RepositoryError> {
        let handle = normalize_handle(handle, "site handle")?;
        let title = validate_required_text(title, "site title", 200)?;
        let description = validate_optional_text(description, "site description", 2_000)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_user_exists(&transaction, owner_user_id)?;
        let already_owns_site = transaction
            .query_row(
                "SELECT 1 FROM site_memberships WHERE user_id = ?1 AND role = 'owner' LIMIT 1",
                params![owner_user_id.to_string()],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if already_owns_site {
            return Err(RepositoryError::Validation(
                "an account can own one site in this deployment".into(),
            ));
        }
        let id = Uuid::now_v7();
        let now = Utc::now();
        transaction
            .execute(
                "INSERT INTO sites (
                    id, handle, title, description, current_theme_revision, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
                params![id.to_string(), handle, title, description, now.to_rfc3339(),],
            )
            .map_err(map_community_constraint_error)?;
        transaction
            .execute(
                "INSERT INTO site_memberships (site_id, user_id, role, created_at)
                 VALUES (?1, ?2, 'owner', ?3)",
                params![id.to_string(), owner_user_id.to_string(), now.to_rfc3339()],
            )
            .map_err(map_community_constraint_error)?;
        transaction
            .execute(
                "INSERT INTO site_theme_revisions (
                    site_id, revision, profile, created_by_user_id, created_at
                 ) VALUES (?1, 1, ?2, ?3, ?4)",
                params![
                    id.to_string(),
                    theme_profile.as_str(),
                    owner_user_id.to_string(),
                    now.to_rfc3339(),
                ],
            )
            .map_err(map_community_constraint_error)?;
        transaction.commit().map_err(storage_error)?;
        load_site_by_id(&connection, id, None)
    }

    /// Provisions the non-loginable public profile used by the retained
    /// single-owner API. This keeps legacy writes visible in the community feed
    /// immediately, including on a fresh community database.
    pub fn ensure_legacy_site(&self, site_id: Uuid) -> Result<SiteRecord, RepositoryError> {
        match self.get_site_by_id(site_id) {
            Ok(site) => return Ok(site),
            Err(RepositoryError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let compact = site_id.simple().to_string();
        let handle = format!("legacy-{compact}");
        let email = format!("{handle}@localhost");
        let now = Utc::now();
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO users (
                    id, email, handle, display_name, password_phc, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, 'Legacy owner',
                           '$argon2id$disabled-for-legacy-readonly-owner', ?4, ?4)",
                params![site_id.to_string(), email, handle, now.to_rfc3339()],
            )
            .map_err(map_community_constraint_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO sites (
                    id, handle, title, description, current_theme_revision, created_at, updated_at
                 ) VALUES (?1, ?2, 'Legacy blog',
                           'Content retained from the single-site deployment profile.',
                           1, ?3, ?3)",
                params![site_id.to_string(), handle, now.to_rfc3339()],
            )
            .map_err(map_community_constraint_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO site_memberships (site_id, user_id, role, created_at)
                 VALUES (?1, ?1, 'owner', ?2)",
                params![site_id.to_string(), now.to_rfc3339()],
            )
            .map_err(map_community_constraint_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO site_theme_revisions (
                    site_id, revision, profile, created_by_user_id, created_at
                 ) VALUES (?1, 1, 'paper', ?1, ?2)",
                params![site_id.to_string(), now.to_rfc3339()],
            )
            .map_err(map_community_constraint_error)?;
        transaction.commit().map_err(storage_error)?;
        load_site_by_id(&connection, site_id, None)
    }

    pub fn list_sites(&self, limit: usize) -> Result<Vec<SiteRecord>, RepositoryError> {
        let connection = self.lock()?;
        load_sites(&connection, None, limit)
    }

    /// Lists only the closed site navigation projection with true SQL
    /// offset/limit pagination. No installation-wide hard cap is applied.
    pub fn list_site_metadata_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<SiteMetadataRecord>, RepositoryError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT id, handle, title, created_at, updated_at
                 FROM sites
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?1 OFFSET ?2",
            )
            .map_err(storage_error)?;
        statement
            .query_map(
                params![page_parameter(limit)?, page_parameter(offset)?],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(storage_error)?
            .map(|row| {
                let (id, handle, title, created_at, updated_at) = row.map_err(storage_error)?;
                Ok(SiteMetadataRecord {
                    id: parse_uuid(&id)?,
                    handle,
                    title,
                    created_at: parse_datetime(&created_at)?,
                    updated_at: parse_datetime(&updated_at)?,
                })
            })
            .collect()
    }

    pub fn get_site_by_id(&self, id: Uuid) -> Result<SiteRecord, RepositoryError> {
        let connection = self.lock()?;
        load_site_by_id(&connection, id, None)
    }

    pub fn get_site_by_handle(&self, handle: &str) -> Result<SiteRecord, RepositoryError> {
        let handle = normalize_handle(handle, "site handle")?;
        let connection = self.lock()?;
        let id: Option<String> = connection
            .query_row(
                "SELECT id FROM sites WHERE handle = ?1 COLLATE NOCASE",
                params![handle],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        load_site_by_id(
            &connection,
            parse_uuid(&id.ok_or(RepositoryError::NotFound)?)?,
            None,
        )
    }

    pub fn list_owned_sites(
        &self,
        owner_user_id: Uuid,
        limit: usize,
    ) -> Result<Vec<SiteRecord>, RepositoryError> {
        let connection = self.lock()?;
        load_sites(&connection, Some(owner_user_id), limit)
    }

    pub fn get_owned_site(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
    ) -> Result<SiteRecord, RepositoryError> {
        let connection = self.lock()?;
        load_site_by_id(&connection, site_id, Some(owner_user_id))
    }

    /// Lists sites where the account has any persisted Studio membership.
    /// Owned sites are ordered first so the no-site-selector Studio remains
    /// deterministic for accounts that also collaborate elsewhere.
    pub fn list_accessible_sites(
        &self,
        user_id: Uuid,
        limit: usize,
    ) -> Result<Vec<SiteRecord>, RepositoryError> {
        let connection = self.lock()?;
        let site_ids = {
            let mut statement = connection
                .prepare(
                    "SELECT site_id FROM site_memberships
                     WHERE user_id = ?1
                     ORDER BY CASE role WHEN 'owner' THEN 0 ELSE 1 END,
                              created_at DESC, site_id DESC
                     LIMIT ?2",
                )
                .map_err(storage_error)?;
            statement
                .query_map(params![user_id.to_string(), limit.min(500) as i64], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(storage_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage_error)?
        };
        site_ids
            .into_iter()
            .map(|site_id| load_site_by_id(&connection, parse_uuid(&site_id)?, None))
            .collect()
    }

    pub fn get_site_membership(
        &self,
        user_id: Uuid,
        site_id: Uuid,
    ) -> Result<SiteMembershipRecord, RepositoryError> {
        let connection = self.lock()?;
        load_site_membership(&connection, site_id, user_id)
    }

    pub fn list_site_memberships(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        limit: usize,
    ) -> Result<Vec<SiteMembershipRecord>, RepositoryError> {
        let connection = self.lock()?;
        ensure_site_owner(&connection, owner_user_id, site_id)?;
        let mut statement = connection
            .prepare(
                "SELECT site_id, user_id, role, created_at
                 FROM site_memberships WHERE site_id = ?1
                 ORDER BY CASE role WHEN 'owner' THEN 0 WHEN 'editor' THEN 1 ELSE 2 END,
                          created_at, user_id LIMIT ?2",
            )
            .map_err(storage_error)?;
        statement
            .query_map(
                params![site_id.to_string(), limit.min(500) as i64],
                stored_site_membership_row,
            )
            .map_err(storage_error)?
            .map(|row| {
                row.map_err(storage_error)
                    .and_then(parse_site_membership_row)
            })
            .collect()
    }

    /// Creates a writer/editor membership for an already registered local
    /// account. Owner membership is provisioned only by `create_site`.
    pub fn add_site_collaborator(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        collaborator_email: &str,
        role: SiteMembershipRole,
    ) -> Result<SiteMembershipRecord, RepositoryError> {
        if role.is_owner() {
            return Err(RepositoryError::Validation(
                "owner membership cannot be invited or reassigned".into(),
            ));
        }
        let email = normalize_email(collaborator_email)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        let collaborator_id: String = transaction
            .query_row(
                "SELECT id FROM users WHERE email = ?1 COLLATE NOCASE",
                params![email],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?
            .ok_or(RepositoryError::NotFound)?;
        let collaborator_id = parse_uuid(&collaborator_id)?;
        let now = Utc::now();
        transaction
            .execute(
                "INSERT INTO site_memberships (site_id, user_id, role, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    site_id.to_string(),
                    collaborator_id.to_string(),
                    role.as_str(),
                    now.to_rfc3339(),
                ],
            )
            .map_err(|error| {
                if error.to_string().contains("site_memberships.site_id") {
                    RepositoryError::Validation("account is already a member of this site".into())
                } else {
                    map_community_constraint_error(error)
                }
            })?;
        transaction.commit().map_err(storage_error)?;
        load_site_membership(&connection, site_id, collaborator_id)
    }

    pub fn remove_site_collaborator(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        collaborator_user_id: Uuid,
    ) -> Result<SiteMembershipRecord, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        let membership = load_site_membership(&transaction, site_id, collaborator_user_id)?;
        if membership.role.is_owner() {
            return Err(RepositoryError::Validation(
                "owner membership cannot be removed or demoted".into(),
            ));
        }
        let removed = transaction
            .execute(
                "DELETE FROM site_memberships WHERE site_id = ?1 AND user_id = ?2",
                params![site_id.to_string(), collaborator_user_id.to_string()],
            )
            .map_err(storage_error)?;
        if removed != 1 {
            return Err(RepositoryError::NotFound);
        }
        transaction.commit().map_err(storage_error)?;
        Ok(membership)
    }

    pub fn owns_site(&self, user_id: Uuid, site_id: Uuid) -> Result<bool, RepositoryError> {
        let connection = self.lock()?;
        owns_site_in_connection(&connection, user_id, site_id)
    }

    /// Appends an immutable appearance revision and atomically selects it.
    pub fn change_site_theme(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        theme_profile: ThemeProfile,
    ) -> Result<SiteRecord, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        // Read and preserve CSS inside the same write transaction that appends
        // the new theme. Otherwise a concurrent CSS save can be overwritten by
        // a stale value observed before this operation acquired its write lock.
        let custom_css: Option<String> = transaction
            .query_row(
                "SELECT theme.custom_css
                 FROM sites site
                 JOIN site_theme_revisions theme
                   ON theme.site_id = site.id
                  AND theme.revision = site.current_theme_revision
                 WHERE site.id = ?1",
                params![site_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        append_site_appearance_revision(
            &transaction,
            owner_user_id,
            site_id,
            theme_profile,
            custom_css,
        )?;
        transaction.commit().map_err(storage_error)?;
        load_site_by_id(&connection, site_id, Some(owner_user_id))
    }

    /// Appends an immutable site appearance revision and atomically selects it.
    pub fn change_site_appearance(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        theme_profile: ThemeProfile,
        custom_css: Option<&str>,
    ) -> Result<SiteRecord, RepositoryError> {
        let custom_css = validate_custom_css(custom_css)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        append_site_appearance_revision(
            &transaction,
            owner_user_id,
            site_id,
            theme_profile,
            custom_css,
        )?;
        transaction.commit().map_err(storage_error)?;
        load_site_by_id(&connection, site_id, Some(owner_user_id))
    }

    /// Creates a site-scoped category. Slugs are immutable after creation so
    /// public category URLs cannot be silently repurposed.
    pub fn create_category(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        input: CreateCategoryInput,
    ) -> Result<CategoryRecord, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        let category = create_category_in_transaction(&transaction, owner_user_id, site_id, input)?;
        transaction.commit().map_err(storage_error)?;
        Ok(category)
    }

    /// Imports a prevalidated set of Markdown posts without requiring a
    /// network authentication surface.
    ///
    /// The complete batch is atomic. A stable `(site, source, source_id)` key
    /// makes exact retries no-ops; reusing that key with different content,
    /// category placement, authorship, timestamps, or aliases fails closed.
    /// Dry runs execute the same constraints and SQL before rolling back.
    pub fn import_offline_batch(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        batch: OfflineImportBatch,
        dry_run: bool,
    ) -> Result<OfflineImportReport, RepositoryError> {
        self.import_offline_batch_with_reserved_roots(
            owner_user_id,
            site_id,
            batch,
            &["blog"],
            dry_run,
        )
    }

    /// Variant used by local maintenance after reading the effective article
    /// base path from trusted deployment configuration. Existing source IDs
    /// remain immutable while later batches may reconcile owner metadata and
    /// append new category declarations and posts.
    pub fn import_offline_batch_with_reserved_roots(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        batch: OfflineImportBatch,
        reserved_route_roots: &[&str],
        dry_run: bool,
    ) -> Result<OfflineImportReport, RepositoryError> {
        validate_offline_import_batch(site_id, &batch, reserved_route_roots)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;

        let mut report = OfflineImportReport {
            dry_run,
            owner_display_name_updated: false,
            categories_created: 0,
            categories_reused: 0,
            posts_imported: 0,
            posts_unchanged: 0,
            aliases_created: 0,
            posts: Vec::with_capacity(batch.posts.len()),
        };
        let import_started_at = Utc::now();
        let owner_display_name = validate_required_text(
            &batch.owner_display_name,
            "offline import owner display name",
            200,
        )?;
        let current_owner_display_name: String = transaction
            .query_row(
                "SELECT display_name FROM users WHERE id = ?1",
                params![owner_user_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?
            .ok_or(RepositoryError::NotFound)?;
        if current_owner_display_name != owner_display_name {
            transaction
                .execute(
                    "UPDATE users SET display_name = ?1, updated_at = ?2 WHERE id = ?3",
                    params![
                        owner_display_name,
                        import_started_at.to_rfc3339(),
                        owner_user_id.to_string(),
                    ],
                )
                .map_err(map_community_constraint_error)?;
            report.owner_display_name_updated = true;
        }

        for category in &batch.categories {
            let slug = normalize_new_category_slug(&category.slug)?;
            let title = validate_required_text(&category.title, "category title", 200)?;
            let description = validate_optional_text(
                category.description.as_deref(),
                "category description",
                2_000,
            )?;
            match load_category_by_slug(&transaction, site_id, &slug) {
                Ok(existing) => {
                    if existing.status != CategoryStatus::Active
                        || existing.title != title
                        || existing.description != description
                    {
                        return Err(RepositoryError::Validation(format!(
                            "offline import category '{slug}' conflicts with existing metadata"
                        )));
                    }
                    report.categories_reused += 1;
                }
                Err(RepositoryError::NotFound) => {
                    ensure_category_landing_available(&transaction, site_id, &slug)?;
                    transaction
                        .execute(
                            "INSERT INTO categories (
                                id, site_id, slug, title, description, theme_profile, status,
                                created_by_user_id, created_at, updated_at
                             ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'active', ?6, ?7, ?7)",
                            params![
                                Uuid::now_v7().to_string(),
                                site_id.to_string(),
                                slug,
                                title,
                                description,
                                owner_user_id.to_string(),
                                import_started_at.to_rfc3339(),
                            ],
                        )
                        .map_err(map_category_constraint_error)?;
                    report.categories_created += 1;
                }
                Err(error) => return Err(error),
            }
        }

        for post in &batch.posts {
            let category_slug = normalize_new_category_slug(&post.primary_category)?;
            let category = load_category_by_slug(&transaction, site_id, &category_slug)?;
            if category.status != CategoryStatus::Active {
                return Err(RepositoryError::Validation(format!(
                    "offline import post '{}' targets archived category '{category_slug}'",
                    post.source_id
                )));
            }
            let canonical_path = category_route_path(Some(&category), &post.slug);
            let idempotency_key =
                offline_import_idempotency_key(site_id, &batch.source, &post.source_id)?;
            let existing = transaction
                .query_row(
                    "SELECT document_id, id FROM revisions WHERE idempotency_key = ?1",
                    params![idempotency_key],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(storage_error)?;
            if let Some((document_id, revision_id)) = existing {
                ensure_offline_import_retry_matches(
                    &transaction,
                    site_id,
                    &batch.source,
                    post,
                    &category,
                    parse_uuid(&document_id)?,
                    parse_uuid(&revision_id)?,
                )?;
                report.posts_unchanged += 1;
                report.posts.push(OfflineImportPostOutcome {
                    source_id: post.source_id.clone(),
                    canonical_path,
                    status: OfflineImportPostStatus::Unchanged,
                });
                continue;
            }

            let document = create_document_in_transaction(
                &transaction,
                offline_import_document(site_id, &batch.source, post),
                post.created_at,
                Some((owner_user_id, category.id)),
                Some(&idempotency_key),
            )?;
            publish_in_transaction_at(
                &transaction,
                document.id,
                document.current_revision_id,
                post.created_at,
            )?;
            for alias in &post.aliases {
                ensure_document_route_available(&transaction, site_id, document.id, &alias.path)?;
                if !alias.path.contains('/') {
                    ensure_root_slug_not_category(&transaction, site_id, &alias.path)?;
                }
                transaction
                    .execute(
                        "INSERT INTO routes (site_id, path, document_id, is_canonical, created_at)
                         VALUES (?1, ?2, ?3, 0, ?4)",
                        params![
                            site_id.to_string(),
                            alias.path,
                            document.id.to_string(),
                            alias.created_at.to_rfc3339(),
                        ],
                    )
                    .map_err(map_constraint_error)?;
                report.aliases_created += 1;
            }
            report.posts_imported += 1;
            report.posts.push(OfflineImportPostOutcome {
                source_id: post.source_id.clone(),
                canonical_path,
                status: OfflineImportPostStatus::Imported,
            });
        }

        if dry_run {
            transaction.rollback().map_err(storage_error)?;
        } else {
            transaction.commit().map_err(storage_error)?;
        }
        Ok(report)
    }

    pub fn list_categories(
        &self,
        site_id: Uuid,
        include_archived: bool,
        limit: usize,
    ) -> Result<Vec<CategoryRecord>, RepositoryError> {
        let connection = self.lock()?;
        load_site_by_id(&connection, site_id, None)?;
        let mut statement = connection
            .prepare(
                "SELECT id, site_id, slug, title, description, theme_profile, status,
                        created_by_user_id, created_at, updated_at
                 FROM categories
                 WHERE site_id = ?1 AND (?2 OR status = 'active')
                 ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, title, slug
                 LIMIT ?3",
            )
            .map_err(storage_error)?;
        statement
            .query_map(
                params![site_id.to_string(), include_archived, limit.min(500) as i64],
                stored_category_row,
            )
            .map_err(storage_error)?
            .map(|row| row.map_err(storage_error).and_then(parse_category_row))
            .collect()
    }

    /// Lists only closed category navigation metadata with true SQL
    /// offset/limit pagination. The site existence check prevents an empty
    /// result from making an unknown tenant indistinguishable from a real,
    /// empty site at repository boundaries that require fail-closed scoping.
    pub fn list_category_metadata_page(
        &self,
        site_id: Uuid,
        include_archived: bool,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<CategoryMetadataRecord>, RepositoryError> {
        let connection = self.lock()?;
        ensure_site_exists(&connection, site_id)?;
        let mut statement = connection
            .prepare(
                "SELECT id, site_id, slug, title, status, created_at, updated_at
                 FROM categories
                 WHERE site_id = ?1 AND (?2 OR status = 'active')
                 ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, title, slug
                 LIMIT ?3 OFFSET ?4",
            )
            .map_err(storage_error)?;
        statement
            .query_map(
                params![
                    site_id.to_string(),
                    include_archived,
                    page_parameter(limit)?,
                    page_parameter(offset)?,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .map_err(storage_error)?
            .map(|row| {
                let (id, site_id, slug, title, status, created_at, updated_at) =
                    row.map_err(storage_error)?;
                Ok(CategoryMetadataRecord {
                    id: parse_uuid(&id)?,
                    site_id: parse_uuid(&site_id)?,
                    slug,
                    title,
                    status: CategoryStatus::from_str(&status)?,
                    created_at: parse_datetime(&created_at)?,
                    updated_at: parse_datetime(&updated_at)?,
                })
            })
            .collect()
    }

    pub fn get_category_by_id(
        &self,
        site_id: Uuid,
        category_id: Uuid,
    ) -> Result<CategoryRecord, RepositoryError> {
        let connection = self.lock()?;
        load_category_by_id(&connection, site_id, category_id)
    }

    pub fn get_category_by_slug(
        &self,
        site_id: Uuid,
        slug: &str,
    ) -> Result<CategoryRecord, RepositoryError> {
        let slug = normalize_category_slug(slug)?;
        let connection = self.lock()?;
        load_category_by_slug(&connection, site_id, &slug)
    }

    pub fn update_category(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        category_id: Uuid,
        input: UpdateCategoryInput,
    ) -> Result<CategoryRecord, RepositoryError> {
        let title = validate_required_text(&input.title, "category title", 200)?;
        let description =
            validate_optional_text(input.description.as_deref(), "category description", 2_000)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        load_category_by_id(&transaction, site_id, category_id)?;
        transaction
            .execute(
                "UPDATE categories
                 SET title = ?1, description = ?2, theme_profile = ?3, updated_at = ?4
                 WHERE id = ?5 AND site_id = ?6",
                params![
                    title,
                    description,
                    input.theme_profile.map(ThemeProfile::as_str),
                    Utc::now().to_rfc3339(),
                    category_id.to_string(),
                    site_id.to_string(),
                ],
            )
            .map_err(map_category_constraint_error)?;
        transaction.commit().map_err(storage_error)?;
        load_category_by_id(&connection, site_id, category_id)
    }

    pub fn archive_category(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        category_id: Uuid,
    ) -> Result<CategoryRecord, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        let category = load_category_by_id(&transaction, site_id, category_id)?;
        if category.status == CategoryStatus::Active {
            transaction
                .execute(
                    "UPDATE categories SET status = 'archived', updated_at = ?1
                     WHERE id = ?2 AND site_id = ?3",
                    params![
                        Utc::now().to_rfc3339(),
                        category_id.to_string(),
                        site_id.to_string(),
                    ],
                )
                .map_err(storage_error)?;
        }
        canonicalize_home_pins_for_changed_site_in_transaction(&transaction, site_id)?;
        transaction.commit().map_err(storage_error)?;
        load_category_by_id(&connection, site_id, category_id)
    }

    /// Atomically creates a first-class series and its backing category.
    ///
    /// The category continues to own the immutable public slug, presentation,
    /// archive lifecycle, and revision-scoped route placement.
    pub fn create_series(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        input: CreateSeriesInput,
    ) -> Result<SeriesRecord, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        let category = create_category_in_transaction(
            &transaction,
            owner_user_id,
            site_id,
            CreateCategoryInput {
                slug: input.slug,
                title: input.title,
                description: input.description,
                theme_profile: input.theme_profile,
            },
        )?;
        let series = create_series_for_category_in_transaction(
            &transaction,
            owner_user_id,
            site_id,
            category.id,
        )?;
        transaction.commit().map_err(storage_error)?;
        Ok(series)
    }

    /// Explicitly promotes an existing category to a series without changing
    /// its metadata, status, routes, or any revision placement.
    ///
    /// Exact retries are idempotent. The initial series order is the original
    /// document creation order and contains only currently published members.
    pub fn promote_category_to_series(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        category_id: Uuid,
    ) -> Result<SeriesRecord, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        load_category_by_id(&transaction, site_id, category_id)?;
        let series = match load_series_by_category_id(&transaction, site_id, category_id) {
            Ok(series) => series,
            Err(RepositoryError::NotFound) => create_series_for_category_in_transaction(
                &transaction,
                owner_user_id,
                site_id,
                category_id,
            )?,
            Err(error) => return Err(error),
        };
        canonicalize_home_pins_for_changed_site_in_transaction(&transaction, site_id)?;
        transaction.commit().map_err(storage_error)?;
        Ok(series)
    }

    pub fn list_series(
        &self,
        site_id: Uuid,
        include_archived: bool,
        limit: usize,
    ) -> Result<Vec<SeriesRecord>, RepositoryError> {
        let connection = self.lock()?;
        ensure_site_exists(&connection, site_id)?;
        list_series_with_connection(&connection, site_id, include_archived, limit)
    }

    pub fn get_series_by_id(
        &self,
        site_id: Uuid,
        series_id: Uuid,
    ) -> Result<SeriesRecord, RepositoryError> {
        let connection = self.lock()?;
        load_series_by_id(&connection, site_id, series_id)
    }

    pub fn get_series_by_slug(
        &self,
        site_id: Uuid,
        slug: &str,
    ) -> Result<SeriesRecord, RepositoryError> {
        let slug = normalize_category_slug(slug)?;
        let connection = self.lock()?;
        load_series_by_slug(&connection, site_id, &slug)
    }

    pub fn get_series_by_category_id(
        &self,
        site_id: Uuid,
        category_id: Uuid,
    ) -> Result<SeriesRecord, RepositoryError> {
        let connection = self.lock()?;
        load_series_by_category_id(&connection, site_id, category_id)
    }

    /// Updates the mutable presentation owned by the backing category without
    /// requiring a caller that only knows the series ID to cross resource
    /// boundaries itself.
    pub fn update_series(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        series_id: Uuid,
        input: UpdateCategoryInput,
    ) -> Result<SeriesRecord, RepositoryError> {
        let title = validate_required_text(&input.title, "series title", 200)?;
        let description =
            validate_optional_text(input.description.as_deref(), "series description", 2_000)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        let series = load_series_by_id(&transaction, site_id, series_id)?;
        let now = Utc::now().to_rfc3339();
        transaction
            .execute(
                "UPDATE categories
                 SET title = ?1, description = ?2, theme_profile = ?3, updated_at = ?4
                 WHERE id = ?5 AND site_id = ?6",
                params![
                    title,
                    description,
                    input.theme_profile.map(ThemeProfile::as_str),
                    now,
                    series.category_id.to_string(),
                    site_id.to_string(),
                ],
            )
            .map_err(map_category_constraint_error)?;
        transaction
            .execute(
                "UPDATE series SET updated_at = ?1 WHERE id = ?2 AND site_id = ?3",
                params![now, series_id.to_string(), site_id.to_string()],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)?;
        load_series_by_id(&connection, site_id, series_id)
    }

    /// Archives a series through its backing category. Existing published
    /// routes and ordered membership stay readable, while category admission
    /// continues to reject future assignments and publications.
    pub fn archive_series(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        series_id: Uuid,
    ) -> Result<SeriesRecord, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        let series = load_series_by_id(&transaction, site_id, series_id)?;
        if series.status == CategoryStatus::Active {
            let now = Utc::now().to_rfc3339();
            transaction
                .execute(
                    "UPDATE categories SET status = 'archived', updated_at = ?1
                     WHERE id = ?2 AND site_id = ?3",
                    params![now, series.category_id.to_string(), site_id.to_string()],
                )
                .map_err(storage_error)?;
            transaction
                .execute(
                    "UPDATE series SET updated_at = ?1
                     WHERE id = ?2 AND site_id = ?3",
                    params![now, series_id.to_string(), site_id.to_string()],
                )
                .map_err(storage_error)?;
        }
        canonicalize_home_pins_for_changed_site_in_transaction(&transaction, site_id)?;
        transaction.commit().map_err(storage_error)?;
        load_series_by_id(&connection, site_id, series_id)
    }

    /// Lists the public members in explicit series order. Draft-only
    /// documents and draft placement changes never participate.
    pub fn list_published_in_series(
        &self,
        site_id: Uuid,
        series_id: Uuid,
        limit: usize,
    ) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
        let connection = self.lock()?;
        load_series_by_id(&connection, site_id, series_id)?;
        list_published_in_series_with_connection(&connection, site_id, series_id, limit)
    }

    /// Atomically replaces the complete order of the currently published
    /// series membership.
    ///
    /// The request must contain exactly the current public member set, at most
    /// 500 unique document IDs. This prevents a stale reorder request from
    /// silently adding, dropping, or cross-tenant moving content.
    pub fn replace_series_order(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        series_id: Uuid,
        document_ids: &[Uuid],
    ) -> Result<Vec<SeriesItemRecord>, RepositoryError> {
        if document_ids.len() > 500
            || document_ids.iter().copied().collect::<BTreeSet<_>>().len() != document_ids.len()
        {
            return Err(RepositoryError::Validation(
                "series order must contain at most 500 unique document ids".into(),
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        load_series_by_id(&transaction, site_id, series_id)?;
        let current =
            list_published_series_items_with_connection(&transaction, site_id, series_id, 501)?;
        let current_ids = current
            .iter()
            .map(|item| item.document_id)
            .collect::<BTreeSet<_>>();
        let requested_ids = document_ids.iter().copied().collect::<BTreeSet<_>>();
        if current.len() != document_ids.len() || current_ids != requested_ids {
            return Err(RepositoryError::RevisionConflict);
        }
        for (index, document_id) in document_ids.iter().enumerate() {
            let position = series_position(index)?;
            let changed = transaction
                .execute(
                    "UPDATE series_items SET position = ?1
                     WHERE series_id = ?2 AND site_id = ?3 AND document_id = ?4",
                    params![
                        position,
                        series_id.to_string(),
                        site_id.to_string(),
                        document_id.to_string(),
                    ],
                )
                .map_err(storage_error)?;
            if changed != 1 {
                return Err(RepositoryError::RevisionConflict);
            }
        }
        transaction
            .execute(
                "UPDATE series SET updated_at = ?1 WHERE id = ?2 AND site_id = ?3",
                params![
                    Utc::now().to_rfc3339(),
                    series_id.to_string(),
                    site_id.to_string()
                ],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)?;
        list_published_series_items_with_connection(&connection, site_id, series_id, 500)
    }

    /// Assigns the current, unpublished revision to a category. `None` moves
    /// it back to the site root. A revision that is already public is immutable;
    /// append a new revision before moving it.
    pub fn assign_revision_category_in_writable_site(
        &self,
        actor_user_id: Uuid,
        site_id: Uuid,
        document_id: Uuid,
        revision_id: Uuid,
        category_id: Option<Uuid>,
    ) -> Result<RevisionCategoryPlacement, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_writer(&transaction, actor_user_id, site_id)?;
        let placement = assign_revision_category_in_transaction(
            &transaction,
            actor_user_id,
            site_id,
            document_id,
            revision_id,
            category_id,
            Utc::now(),
        )?;
        transaction.commit().map_err(storage_error)?;
        Ok(placement)
    }

    pub fn get_revision_category_placement(
        &self,
        site_id: Uuid,
        document_id: Uuid,
        revision_id: Uuid,
    ) -> Result<RevisionCategoryPlacement, RepositoryError> {
        let connection = self.lock()?;
        ensure_document_in_site(&connection, site_id, document_id)?;
        load_revision_category_placement(&connection, site_id, document_id, revision_id)
    }

    pub fn get_current_category(
        &self,
        site_id: Uuid,
        document_id: Uuid,
    ) -> Result<Option<CategoryRecord>, RepositoryError> {
        let connection = self.lock()?;
        load_document_category(&connection, site_id, document_id, RevisionSelector::Current)
    }

    pub fn get_published_category(
        &self,
        site_id: Uuid,
        document_id: Uuid,
    ) -> Result<Option<CategoryRecord>, RepositoryError> {
        let connection = self.lock()?;
        load_document_category(
            &connection,
            site_id,
            document_id,
            RevisionSelector::Published,
        )
    }

    /// Reports whether an exact persisted path is a historical route for a
    /// currently published document. Canonical storage paths are deliberately
    /// excluded: an uncategorized post stored as `references`, for example,
    /// is exposed below the configured article base and does not occupy the
    /// application's root `/references` page.
    pub fn published_noncanonical_route_exists(
        &self,
        site_id: Uuid,
        path: &str,
    ) -> Result<bool, RepositoryError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT EXISTS (
                   SELECT 1
                   FROM routes route
                   JOIN documents document
                     ON document.id = route.document_id
                    AND document.site_id = route.site_id
                   WHERE route.site_id = ?1
                     AND route.path = ?2
                     AND route.is_canonical = 0
                     AND document.published_revision_id IS NOT NULL
                     AND document.status != 'archived'
                 )",
                params![site_id.to_string(), path],
                |row| row.get(0),
            )
            .map_err(storage_error)
    }

    /// Resolves a backwards-compatible leaf slug only when it identifies one
    /// currently published document in the site. Category routes deliberately
    /// allow the same leaf below different category roots, so callers must keep
    /// zero and multiple matches indistinguishable.
    pub fn get_unique_published_by_leaf_slug(
        &self,
        site_id: Uuid,
        leaf_slug: &str,
    ) -> Result<Option<DocumentSnapshot>, RepositoryError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT document.id
                 FROM documents document
                 JOIN revisions published ON published.id = document.published_revision_id
                 WHERE document.site_id = ?1
                   AND published.slug = ?2
                   AND document.published_revision_id IS NOT NULL
                   AND document.status != 'archived'
                 ORDER BY document.id
                 LIMIT 2",
            )
            .map_err(storage_error)?;
        let ids = statement
            .query_map(params![site_id.to_string(), leaf_slug], |row| {
                row.get::<_, String>(0)
            })
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        if ids.len() != 1 {
            return Ok(None);
        }
        load_document(
            &connection,
            parse_uuid(&ids[0])?,
            RevisionSelector::Published,
        )
        .map(Some)
    }

    pub fn list_published_in_category(
        &self,
        site_id: Uuid,
        category_id: Uuid,
        limit: usize,
    ) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
        let connection = self.lock()?;
        load_category_by_id(&connection, site_id, category_id)?;
        list_published_in_category_with_connection(
            &connection,
            site_id,
            category_id,
            limit,
            CategoryPostOrder::NewestFirst,
        )
    }

    pub fn create_document_in_owned_site(
        &self,
        owner_user_id: Uuid,
        input: NewDocument,
    ) -> Result<DocumentSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, input.site_id)?;
        let document = create_document_in_transaction(&transaction, input, Utc::now(), None, None)?;
        transaction.commit().map_err(storage_error)?;
        Ok(document)
    }

    pub fn create_document_in_writable_site(
        &self,
        actor_user_id: Uuid,
        input: NewDocument,
    ) -> Result<DocumentSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        ensure_site_writer(&transaction, actor_user_id, input.site_id)?;
        let document = create_document_in_transaction(&transaction, input, Utc::now(), None, None)?;
        transaction.commit().map_err(storage_error)?;
        Ok(document)
    }

    /// Atomically creates the first revision and its optional category
    /// placement. This avoids exposing a transient root-slug draft to another
    /// writer and permits the same post slug in different categories.
    pub fn create_document_in_writable_site_with_category(
        &self,
        actor_user_id: Uuid,
        input: NewDocument,
        category_id: Option<Uuid>,
    ) -> Result<DocumentSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;
        let site_id = input.site_id;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_writer(&transaction, actor_user_id, site_id)?;
        let initial_category = category_id.map(|category_id| (actor_user_id, category_id));
        let document = create_document_in_transaction(
            &transaction,
            input,
            Utc::now(),
            initial_category,
            None,
        )?;
        transaction.commit().map_err(storage_error)?;
        Ok(document)
    }

    pub fn get_document_in_owned_site(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        document_id: Uuid,
    ) -> Result<DocumentSnapshot, RepositoryError> {
        let connection = self.lock()?;
        ensure_site_owner(&connection, owner_user_id, site_id)?;
        ensure_document_in_site(&connection, site_id, document_id)?;
        load_document(&connection, document_id, RevisionSelector::Current)
    }

    pub fn get_document_in_writable_site(
        &self,
        actor_user_id: Uuid,
        site_id: Uuid,
        document_id: Uuid,
    ) -> Result<DocumentSnapshot, RepositoryError> {
        let connection = self.lock()?;
        ensure_site_writer(&connection, actor_user_id, site_id)?;
        ensure_document_in_site(&connection, site_id, document_id)?;
        load_document(&connection, document_id, RevisionSelector::Current)
    }

    pub fn list_documents_in_owned_site(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        limit: usize,
    ) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
        let connection = self.lock()?;
        ensure_site_owner(&connection, owner_user_id, site_id)?;
        list_documents_with_selector(&connection, Some(site_id), limit, RevisionSelector::Current)
    }

    pub fn list_documents_in_writable_site(
        &self,
        actor_user_id: Uuid,
        site_id: Uuid,
        limit: usize,
    ) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
        let connection = self.lock()?;
        ensure_site_writer(&connection, actor_user_id, site_id)?;
        list_documents_with_selector(&connection, Some(site_id), limit, RevisionSelector::Current)
    }

    /// Lists closed metadata for documents whose exact current revision is in
    /// `category_id`. Passing `None` selects only uncategorized current
    /// revisions; it does not include documents based on an older published
    /// revision's placement.
    pub fn list_current_document_metadata_page(
        &self,
        site_id: Uuid,
        category_id: Option<Uuid>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<CurrentDocumentMetadataRecord>, RepositoryError> {
        let connection = self.lock()?;
        ensure_site_exists(&connection, site_id)?;
        if let Some(category_id) = category_id {
            ensure_category_in_site(&connection, site_id, category_id)?;
        }
        let category_id = category_id.map(|id| id.to_string());
        let mut statement = connection
            .prepare(
                "SELECT document.id, document.site_id, document.status,
                        document.current_revision_id, document.published_revision_id,
                        json_extract(current.snapshot_json, '$.title'), current.slug,
                        current.revision_number, placement.category_id,
                        document.created_at, document.updated_at
                 FROM documents document
                 JOIN revisions current
                   ON current.id = document.current_revision_id
                  AND current.document_id = document.id
                 JOIN revision_categories placement
                   ON placement.revision_id = document.current_revision_id
                  AND placement.document_id = document.id
                  AND placement.site_id = document.site_id
                 WHERE document.site_id = ?1
                   AND ((?2 IS NULL AND placement.category_id IS NULL)
                        OR placement.category_id = ?2)
                 ORDER BY document.updated_at DESC, document.id DESC
                 LIMIT ?3 OFFSET ?4",
            )
            .map_err(storage_error)?;
        statement
            .query_map(
                params![
                    site_id.to_string(),
                    category_id,
                    page_parameter(limit)?,
                    page_parameter(offset)?,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                    ))
                },
            )
            .map_err(storage_error)?
            .map(|row| {
                let (
                    id,
                    site_id,
                    status,
                    current_revision_id,
                    published_revision_id,
                    title,
                    slug,
                    revision_number,
                    category_id,
                    created_at,
                    updated_at,
                ) = row.map_err(storage_error)?;
                Ok(CurrentDocumentMetadataRecord {
                    id: parse_uuid(&id)?,
                    site_id: parse_uuid(&site_id)?,
                    status: parse_status(&status)?,
                    current_revision_id: parse_uuid(&current_revision_id)?,
                    published_revision_id: published_revision_id
                        .map(|id| parse_uuid(&id))
                        .transpose()?,
                    title,
                    slug,
                    revision_number: u64::try_from(revision_number).map_err(storage_error)?,
                    category_id: category_id.map(|id| parse_uuid(&id)).transpose()?,
                    created_at: parse_datetime(&created_at)?,
                    updated_at: parse_datetime(&updated_at)?,
                })
            })
            .collect()
    }

    /// Lists closed revision metadata with true SQL offset/limit pagination.
    pub fn list_revision_metadata_page(
        &self,
        document_id: Uuid,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<RevisionMetadataRecord>, RepositoryError> {
        let connection = self.lock()?;
        ensure_document_exists(&connection, document_id)?;
        let mut statement = connection
            .prepare(
                "SELECT id, document_id, revision_number, slug, created_at
                 FROM revisions
                 WHERE document_id = ?1
                 ORDER BY revision_number DESC
                 LIMIT ?2 OFFSET ?3",
            )
            .map_err(storage_error)?;
        statement
            .query_map(
                params![
                    document_id.to_string(),
                    page_parameter(limit)?,
                    page_parameter(offset)?,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(storage_error)?
            .map(|row| {
                let (id, document_id, revision_number, slug, created_at) =
                    row.map_err(storage_error)?;
                Ok(RevisionMetadataRecord {
                    id: parse_uuid(&id)?,
                    document_id: parse_uuid(&document_id)?,
                    revision_number: u64::try_from(revision_number).map_err(storage_error)?,
                    slug,
                    created_at: parse_datetime(&created_at)?,
                })
            })
            .collect()
    }

    pub fn revise_document_in_owned_site(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        input: ProposedRevision,
    ) -> Result<RevisionSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        ensure_document_in_site(&transaction, site_id, input.document_id)?;
        let revision = append_revision_in_transaction(&transaction, input, Utc::now())?;
        transaction.commit().map_err(storage_error)?;
        Ok(revision)
    }

    pub fn revise_document_in_writable_site(
        &self,
        actor_user_id: Uuid,
        site_id: Uuid,
        input: ProposedRevision,
    ) -> Result<RevisionSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        ensure_site_writer(&transaction, actor_user_id, site_id)?;
        ensure_document_in_site(&transaction, site_id, input.document_id)?;
        let revision = append_revision_in_transaction(&transaction, input, Utc::now())?;
        transaction.commit().map_err(storage_error)?;
        Ok(revision)
    }

    /// Appends and optionally moves a revision in one transaction.
    ///
    /// `None` inherits the parent placement through the migration-v8 trigger,
    /// `Some(None)` explicitly moves to the site root, and
    /// `Some(Some(category_id))` moves to that active site category.
    pub fn revise_document_in_writable_site_with_category(
        &self,
        actor_user_id: Uuid,
        site_id: Uuid,
        input: ProposedRevision,
        category_selection: Option<Option<Uuid>>,
    ) -> Result<RevisionSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;
        let document_id = input.document_id;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_writer(&transaction, actor_user_id, site_id)?;
        ensure_document_in_site(&transaction, site_id, document_id)?;
        let revision = append_revision_in_transaction_with_category(
            &transaction,
            input,
            Utc::now(),
            category_selection.map(|category_id| (actor_user_id, category_id)),
        )?;
        transaction.commit().map_err(storage_error)?;
        Ok(revision)
    }

    pub fn publish_document_in_owned_site(
        &self,
        owner_user_id: Uuid,
        site_id: Uuid,
        document_id: Uuid,
        revision_id: Uuid,
    ) -> Result<DocumentSnapshot, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        ensure_document_in_site(&transaction, site_id, document_id)?;
        publish_in_transaction(&transaction, document_id, revision_id)?;
        transaction.commit().map_err(storage_error)?;
        load_document(&connection, document_id, RevisionSelector::Published)
    }

    pub fn list_published_across_sites(
        &self,
        limit: usize,
    ) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
        let connection = self.lock()?;
        list_documents_with_selector(&connection, None, limit, RevisionSelector::Published)
    }

    pub fn get_published_document_by_id(
        &self,
        document_id: Uuid,
    ) -> Result<DocumentSnapshot, RepositoryError> {
        let connection = self.lock()?;
        load_document(&connection, document_id, RevisionSelector::Published)
    }

    /// Authenticated comments are immediately approved in the initial
    /// community profile so a successful submission is visible at once.
    pub fn create_comment(
        &self,
        author_user_id: Uuid,
        site_id: Uuid,
        document_id: Uuid,
        source_markdown: &str,
    ) -> Result<CommentRecord, RepositoryError> {
        let source_markdown = validate_comment_markdown(source_markdown)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        ensure_user_exists(&transaction, author_user_id)?;
        ensure_published_document_in_site(&transaction, site_id, document_id)?;
        let id = Uuid::now_v7();
        let now = Utc::now();
        transaction
            .execute(
                "INSERT INTO comments (
                    id, site_id, document_id, author_user_id, source_markdown,
                    status, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'approved', ?6, ?6)",
                params![
                    id.to_string(),
                    site_id.to_string(),
                    document_id.to_string(),
                    author_user_id.to_string(),
                    source_markdown,
                    now.to_rfc3339(),
                ],
            )
            .map_err(map_community_constraint_error)?;
        transaction.commit().map_err(storage_error)?;
        load_comment_by_id(&connection, id, false)
    }

    pub fn list_approved_comments(
        &self,
        site_id: Uuid,
        document_id: Uuid,
        limit: usize,
    ) -> Result<Vec<CommentRecord>, RepositoryError> {
        let connection = self.lock()?;
        ensure_published_document_in_site(&connection, site_id, document_id)?;
        let mut statement = connection
            .prepare(
                "SELECT id, site_id, document_id, author_user_id, source_markdown,
                        status, created_at, updated_at
                 FROM comments
                 WHERE site_id = ?1 AND document_id = ?2 AND status = 'approved'
                 ORDER BY created_at ASC, id ASC LIMIT ?3",
            )
            .map_err(storage_error)?;
        statement
            .query_map(
                params![
                    site_id.to_string(),
                    document_id.to_string(),
                    limit.min(1_000) as i64,
                ],
                stored_comment_row,
            )
            .map_err(storage_error)?
            .map(|row| row.map_err(storage_error).and_then(parse_comment_row))
            .collect()
    }

    pub fn count_approved_comments(
        &self,
        site_id: Uuid,
        document_id: Uuid,
    ) -> Result<usize, RepositoryError> {
        let connection = self.lock()?;
        ensure_published_document_in_site(&connection, site_id, document_id)?;
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM comments
                 WHERE site_id = ?1 AND document_id = ?2 AND status = 'approved'",
                params![site_id.to_string(), document_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        usize::try_from(count).map_err(storage_error)
    }

    pub fn get_comment(&self, comment_id: Uuid) -> Result<CommentRecord, RepositoryError> {
        let connection = self.lock()?;
        load_comment_by_id(&connection, comment_id, true)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, RepositoryError> {
        self.connection
            .lock()
            .map_err(|_| RepositoryError::Storage("SQLite connection lock was poisoned".into()))
    }
}

impl SqliteRepository {
    /// Atomically replaces the complete, ordered global home curation set.
    /// The caller is responsible for authenticating an installation
    /// administrator; repository validation keeps the invariant true for every
    /// adapter and future CLI using the same port.
    pub fn replace_home_pins(
        &self,
        administrator_user_id: Uuid,
        targets: &[HomePinTarget],
    ) -> Result<Vec<HomePinRecord>, RepositoryError> {
        if targets.len() > 3 || targets.iter().collect::<BTreeSet<_>>().len() != targets.len() {
            return Err(RepositoryError::Validation(
                "home pins must contain at most three unique targets".into(),
            ));
        }

        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let control = load_admin_control_plane(&transaction)?;
        if control.owner_user_id != administrator_user_id {
            return Err(RepositoryError::NotFound);
        }
        validate_home_pin_targets(&transaction, control.primary_site_id, targets)?;
        let pins =
            replace_home_pin_targets_in_transaction(&transaction, administrator_user_id, targets)?;
        transaction.commit().map_err(storage_error)?;
        Ok(pins)
    }

    /// Compatibility adapter for the document-only v1 pin request. A document
    /// whose published revision belongs to an active primary-site series is
    /// normalized to that series target. Multiple legacy documents from the
    /// same series collapse to the first requested slot.
    pub fn replace_legacy_home_document_pins(
        &self,
        administrator_user_id: Uuid,
        document_ids: &[Uuid],
    ) -> Result<Vec<HomePinRecord>, RepositoryError> {
        if document_ids.len() > 3
            || document_ids.iter().collect::<BTreeSet<_>>().len() != document_ids.len()
        {
            return Err(RepositoryError::Validation(
                "legacy home pins must contain at most three unique document ids".into(),
            ));
        }

        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let control = load_admin_control_plane(&transaction)?;
        if control.owner_user_id != administrator_user_id {
            return Err(RepositoryError::NotFound);
        }
        let mut seen = BTreeSet::new();
        let mut targets = Vec::with_capacity(document_ids.len());
        for document_id in document_ids {
            let document = load_document(&transaction, *document_id, RevisionSelector::Published)
                .map_err(|error| match error {
                RepositoryError::NotFound => RepositoryError::Validation(
                    "only currently published documents can be pinned".into(),
                ),
                other => other,
            })?;
            if document.status == DocumentStatus::Archived {
                return Err(RepositoryError::Validation(
                    "only currently published documents can be pinned".into(),
                ));
            }
            let target = match active_home_series_for_published_document(
                &transaction,
                control.primary_site_id,
                *document_id,
            )? {
                Some(series) => HomePinTarget::Series { id: series.id },
                None => HomePinTarget::Post { id: *document_id },
            };
            if seen.insert(target) {
                targets.push(target);
            }
        }
        validate_home_pin_targets(&transaction, control.primary_site_id, &targets)?;
        let pins =
            replace_home_pin_targets_in_transaction(&transaction, administrator_user_id, &targets)?;
        transaction.commit().map_err(storage_error)?;
        Ok(pins)
    }

    pub fn list_home_pins(&self) -> Result<Vec<HomePinRecord>, RepositoryError> {
        let mut connection = self.lock()?;
        if connection.is_readonly(MAIN_DB).map_err(storage_error)? {
            let primary_site_id = load_admin_control_plane(&connection)?.primary_site_id;
            return canonical_home_pin_records(&connection, primary_site_id);
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let primary_site_id = load_admin_control_plane(&transaction)?.primary_site_id;
        let pins = canonicalize_home_pins_in_transaction(&transaction, primary_site_id)?;
        transaction.commit().map_err(storage_error)?;
        Ok(pins)
    }

    /// Returns a coherent public home snapshot of peer Series and standalone
    /// post units, with the canonical pins moved to the front.
    ///
    /// Series/category document payloads share a hard 500-item bound. Every
    /// active, non-empty Series first receives one reserved item, even when the
    /// caller's requested limit is smaller than the Series count. Remaining
    /// capacity is assigned in explicit Series order, followed by legacy
    /// category sections. This keeps every Series discoverable without letting
    /// an early large Series consume the entire bounded projection.
    pub fn home_feed(
        &self,
        primary_site_id: Uuid,
        recent_limit: usize,
    ) -> Result<HomeFeedRecords, RepositoryError> {
        let connection = self.lock()?;
        load_site_by_id(&connection, primary_site_id, None)?;
        let control = load_admin_control_plane(&connection)?;
        if control.primary_site_id != primary_site_id {
            return Err(RepositoryError::NotFound);
        }
        // Public home is a true read: it projects canonical targets in memory
        // and never acquires a SQLite write lock. Durable cleanup happens in
        // publish/promote/archive transactions and authenticated pin reads.
        let pins = canonical_home_pin_records(&connection, primary_site_id)?;

        let all_series = list_series_with_connection(&connection, primary_site_id, false, 500)?;
        let mut nonempty_series = Vec::with_capacity(all_series.len());
        for series in all_series {
            if !list_published_in_series_with_connection(
                &connection,
                primary_site_id,
                series.id,
                1,
            )?
            .is_empty()
            {
                nonempty_series.push(series);
            }
        }
        let section_budget = recent_limit
            .min(HOME_FEED_MAX_SECTION_ITEMS)
            .max(nonempty_series.len());
        let series_count = nonempty_series.len();
        let mut remaining = section_budget;
        let mut series_sections = Vec::with_capacity(series_count);
        for (index, series) in nonempty_series.into_iter().enumerate() {
            let later_series = series_count.saturating_sub(index + 1);
            let item_limit = remaining
                .saturating_sub(later_series)
                .max(1)
                .min(HOME_FEED_MAX_SECTION_ITEMS);
            let items = list_published_in_series_with_connection(
                &connection,
                primary_site_id,
                series.id,
                item_limit,
            )?;
            debug_assert!(!items.is_empty());
            remaining = remaining.saturating_sub(items.len());
            series_sections.push(HomeSeriesSectionRecords { series, items });
        }

        let mut units = Vec::new();
        let mut pinned = Vec::with_capacity(pins.len());
        let mut pinned_ids = BTreeSet::new();
        let mut pinned_series_ids = BTreeSet::new();
        for pin in pins {
            match pin.target {
                HomePinTarget::Post { id } => {
                    match load_document(&connection, id, RevisionSelector::Published) {
                        Ok(document) if document.status != DocumentStatus::Archived => {
                            pinned_ids.insert(document.id);
                            pinned.push(document.clone());
                            units.push(HomeUnitRecords::Post(document));
                        }
                        Ok(_) | Err(RepositoryError::NotFound) => {}
                        Err(error) => return Err(error),
                    }
                }
                HomePinTarget::Series { id } => {
                    if !pinned_series_ids.insert(id) {
                        continue;
                    }
                    let Some(section) = series_sections
                        .iter()
                        .find(|section| section.series.id == id)
                        .cloned()
                    else {
                        continue;
                    };
                    if let Some(representative) = section.items.first() {
                        pinned_ids.insert(representative.id);
                        pinned.push(representative.clone());
                    }
                    units.push(HomeUnitRecords::Series(section));
                }
            }
        }
        for section in &series_sections {
            if !pinned_series_ids.contains(&section.series.id) {
                units.push(HomeUnitRecords::Series(section.clone()));
            }
        }

        let recent = list_home_standalone_published_with_connection(
            &connection,
            primary_site_id,
            &pinned_ids,
            recent_limit.min(HOME_FEED_MAX_SECTION_ITEMS),
        )?;

        let categories = {
            let mut statement = connection
                .prepare(
                    "SELECT id, site_id, slug, title, description, theme_profile, status,
                            created_by_user_id, created_at, updated_at
                     FROM categories
                     WHERE site_id = ?1 AND status = 'active'
                       AND NOT EXISTS (
                         SELECT 1 FROM series
                         WHERE series.site_id = categories.site_id
                           AND series.category_id = categories.id
                       )
                     ORDER BY created_at ASC, id ASC
                     LIMIT 500",
                )
                .map_err(storage_error)?;
            statement
                .query_map(params![primary_site_id.to_string()], stored_category_row)
                .map_err(storage_error)?
                .map(|row| row.map_err(storage_error).and_then(parse_category_row))
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut category_sections = Vec::new();
        for category in categories {
            if remaining == 0 {
                break;
            }
            let fetch_limit = remaining
                .saturating_add(pinned_ids.len())
                .min(HOME_FEED_MAX_SECTION_ITEMS);
            let items = list_published_in_category_with_connection(
                &connection,
                primary_site_id,
                category.id,
                fetch_limit,
                CategoryPostOrder::OldestFirst,
            )?
            .into_iter()
            .filter(|document| !pinned_ids.contains(&document.id))
            .take(remaining)
            .collect::<Vec<_>>();
            if items.is_empty() {
                continue;
            }
            remaining = remaining.saturating_sub(items.len());
            category_sections.push(HomeCategorySectionRecords { category, items });
        }
        for document in &recent {
            if active_home_series_for_published_document(&connection, primary_site_id, document.id)?
                .is_none()
            {
                units.push(HomeUnitRecords::Post(document.clone()));
            }
        }
        Ok(HomeFeedRecords {
            units,
            pinned,
            recent,
            category_sections,
            series_sections,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredHomePin {
    slot: u8,
    target: HomePinTarget,
    pinned_by_user_id: Uuid,
    pinned_at: DateTime<Utc>,
}

impl StoredHomePin {
    fn public_record(&self) -> HomePinRecord {
        HomePinRecord {
            slot: self.slot,
            target: self.target,
            pinned_at: self.pinned_at,
        }
    }
}

fn load_home_pins(connection: &Connection) -> Result<Vec<HomePinRecord>, RepositoryError> {
    load_stored_home_pins(connection)
        .map(|pins| pins.into_iter().map(|pin| pin.public_record()).collect())
}

fn load_stored_home_pins(connection: &Connection) -> Result<Vec<StoredHomePin>, RepositoryError> {
    let mut statement = connection
        .prepare(
            "SELECT slot, target_kind, document_id, series_id,
                    pinned_by_user_id, pinned_at
             FROM home_pins ORDER BY slot ASC",
        )
        .map_err(storage_error)?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(storage_error)?
        .map(|row| {
            let (slot, target_kind, document_id, series_id, pinned_by_user_id, pinned_at) =
                row.map_err(storage_error)?;
            let target = match (
                target_kind.as_str(),
                document_id.as_deref(),
                series_id.as_deref(),
            ) {
                ("post", Some(document_id), None) => HomePinTarget::Post {
                    id: parse_uuid(document_id)?,
                },
                ("series", None, Some(series_id)) => HomePinTarget::Series {
                    id: parse_uuid(series_id)?,
                },
                _ => {
                    return Err(RepositoryError::Storage(
                        "home pin target violates its typed storage invariant".into(),
                    ));
                }
            };
            Ok(StoredHomePin {
                slot: u8::try_from(slot).map_err(storage_error)?,
                target,
                pinned_by_user_id: parse_uuid(&pinned_by_user_id)?,
                pinned_at: parse_datetime(&pinned_at)?,
            })
        })
        .collect()
}

/// Canonicalizes stored pin targets against current published placement.
///
/// A post that has entered an active primary-site Series becomes that Series
/// target. Targets that now resolve to the same Series retain the earliest
/// slot's audit metadata, and all survivors are compacted back to slots 1..=3.
/// Archived/unpublished targets are removed so admin reads and public home
/// reads observe the same durable set.
fn canonicalize_home_pins_in_transaction(
    transaction: &Transaction<'_>,
    primary_site_id: Uuid,
) -> Result<Vec<HomePinRecord>, RepositoryError> {
    let stored = load_stored_home_pins(transaction)?;
    let canonical = canonical_home_pins_from_stored(transaction, primary_site_id, &stored)?;
    let changed = stored.len() != canonical.len()
        || stored
            .iter()
            .zip(&canonical)
            .any(|(old, new)| old.slot != new.slot || old.target != new.target);
    if changed {
        transaction
            .execute("DELETE FROM home_pins", [])
            .map_err(storage_error)?;
        for pin in &canonical {
            let (target_kind, document_id, series_id) = match pin.target {
                HomePinTarget::Post { id } => ("post", Some(id.to_string()), None),
                HomePinTarget::Series { id } => ("series", None, Some(id.to_string())),
            };
            transaction
                .execute(
                    "INSERT INTO home_pins (
                        slot, target_kind, document_id, series_id,
                        pinned_by_user_id, pinned_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        i64::from(pin.slot),
                        target_kind,
                        document_id,
                        series_id,
                        pin.pinned_by_user_id.to_string(),
                        pin.pinned_at.to_rfc3339(),
                    ],
                )
                .map_err(map_constraint_error)?;
        }
    }
    Ok(canonical
        .into_iter()
        .map(|pin| pin.public_record())
        .collect())
}

fn canonical_home_pin_records(
    connection: &Connection,
    primary_site_id: Uuid,
) -> Result<Vec<HomePinRecord>, RepositoryError> {
    let stored = load_stored_home_pins(connection)?;
    canonical_home_pins_from_stored(connection, primary_site_id, &stored)
        .map(|pins| pins.into_iter().map(|pin| pin.public_record()).collect())
}

fn canonical_home_pins_from_stored(
    connection: &Connection,
    primary_site_id: Uuid,
    stored: &[StoredHomePin],
) -> Result<Vec<StoredHomePin>, RepositoryError> {
    let mut seen = BTreeSet::new();
    let mut canonical = Vec::with_capacity(stored.len());
    for pin in stored {
        let Some(target) = canonical_home_pin_target(connection, primary_site_id, pin.target)?
        else {
            continue;
        };
        if seen.insert(target) {
            canonical.push(StoredHomePin {
                slot: u8::try_from(canonical.len() + 1).map_err(storage_error)?,
                target,
                pinned_by_user_id: pin.pinned_by_user_id,
                pinned_at: pin.pinned_at,
            });
        }
    }
    Ok(canonical)
}

fn canonical_home_pin_target(
    connection: &Connection,
    primary_site_id: Uuid,
    target: HomePinTarget,
) -> Result<Option<HomePinTarget>, RepositoryError> {
    match target {
        HomePinTarget::Post { id } => {
            let document = match load_document(connection, id, RevisionSelector::Published) {
                Ok(document) if document.status != DocumentStatus::Archived => document,
                Ok(_) | Err(RepositoryError::NotFound) => return Ok(None),
                Err(error) => return Err(error),
            };
            Ok(
                active_home_series_for_published_document(
                    connection,
                    primary_site_id,
                    document.id,
                )?
                .map_or(Some(HomePinTarget::Post { id }), |series| {
                    Some(HomePinTarget::Series { id: series.id })
                }),
            )
        }
        HomePinTarget::Series { id } => {
            let series = match load_series_by_id(connection, primary_site_id, id) {
                Ok(series) if series.status == CategoryStatus::Active => series,
                Ok(_) | Err(RepositoryError::NotFound) => return Ok(None),
                Err(error) => return Err(error),
            };
            Ok((!list_published_in_series_with_connection(
                connection,
                primary_site_id,
                series.id,
                1,
            )?
            .is_empty())
            .then_some(HomePinTarget::Series { id: series.id }))
        }
    }
}

fn canonicalize_home_pins_for_changed_site_in_transaction(
    transaction: &Transaction<'_>,
    changed_site_id: Uuid,
) -> Result<(), RepositoryError> {
    let primary_site_id = transaction
        .query_row(
            "SELECT primary_site_id FROM admin_control_plane WHERE singleton = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(storage_error)?
        .map(|id| parse_uuid(&id))
        .transpose()?;
    if primary_site_id == Some(changed_site_id) {
        canonicalize_home_pins_in_transaction(transaction, changed_site_id)?;
    }
    Ok(())
}

fn validate_home_pin_targets(
    connection: &Connection,
    primary_site_id: Uuid,
    targets: &[HomePinTarget],
) -> Result<(), RepositoryError> {
    for target in targets {
        match target {
            HomePinTarget::Post { id } => {
                let document = load_document(connection, *id, RevisionSelector::Published)
                    .map_err(|error| match error {
                        RepositoryError::NotFound => RepositoryError::Validation(
                            "only currently published standalone posts can be pinned".into(),
                        ),
                        other => other,
                    })?;
                if document.status == DocumentStatus::Archived {
                    return Err(RepositoryError::Validation(
                        "only currently published standalone posts can be pinned".into(),
                    ));
                }
                if active_home_series_for_published_document(connection, primary_site_id, *id)?
                    .is_some()
                {
                    return Err(RepositoryError::Validation(
                        "a post in an active series must be pinned through its series target"
                            .into(),
                    ));
                }
            }
            HomePinTarget::Series { id } => {
                let series = load_series_by_id(connection, primary_site_id, *id).map_err(
                    |error| match error {
                        RepositoryError::NotFound => RepositoryError::Validation(
                            "only active primary-site series can be pinned".into(),
                        ),
                        other => other,
                    },
                )?;
                if series.status != CategoryStatus::Active
                    || list_published_in_series_with_connection(
                        connection,
                        primary_site_id,
                        series.id,
                        1,
                    )?
                    .is_empty()
                {
                    return Err(RepositoryError::Validation(
                        "only active primary-site series with published posts can be pinned".into(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn replace_home_pin_targets_in_transaction(
    transaction: &Transaction<'_>,
    administrator_user_id: Uuid,
    targets: &[HomePinTarget],
) -> Result<Vec<HomePinRecord>, RepositoryError> {
    transaction
        .execute("DELETE FROM home_pins", [])
        .map_err(storage_error)?;
    let now = Utc::now();
    for (index, target) in targets.iter().enumerate() {
        let (target_kind, document_id, series_id) = match target {
            HomePinTarget::Post { id } => ("post", Some(id.to_string()), None),
            HomePinTarget::Series { id } => ("series", None, Some(id.to_string())),
        };
        transaction
            .execute(
                "INSERT INTO home_pins (
                    slot, target_kind, document_id, series_id,
                    pinned_by_user_id, pinned_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    (index + 1) as i64,
                    target_kind,
                    document_id,
                    series_id,
                    administrator_user_id.to_string(),
                    now.to_rfc3339(),
                ],
            )
            .map_err(map_constraint_error)?;
    }
    load_home_pins(transaction)
}

fn active_home_series_for_published_document(
    connection: &Connection,
    primary_site_id: Uuid,
    document_id: Uuid,
) -> Result<Option<SeriesRecord>, RepositoryError> {
    let series_id = connection
        .query_row(
            "SELECT series.id
             FROM documents document
             JOIN revision_categories placement
               ON placement.revision_id = document.published_revision_id
              AND placement.document_id = document.id
              AND placement.site_id = document.site_id
             JOIN series
               ON series.category_id = placement.category_id
              AND series.site_id = placement.site_id
             JOIN categories category
               ON category.id = series.category_id
              AND category.site_id = series.site_id
             WHERE document.id = ?1
               AND document.site_id = ?2
               AND document.published_revision_id IS NOT NULL
               AND document.status != 'archived'
               AND category.status = 'active'",
            params![document_id.to_string(), primary_site_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(storage_error)?;
    series_id
        .map(|series_id| load_series_by_id(connection, primary_site_id, parse_uuid(&series_id)?))
        .transpose()
}

impl ContentRepository for SqliteRepository {
    fn create_document(&self, input: NewDocument) -> Result<DocumentSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        let document = create_document_in_transaction(&transaction, input, Utc::now(), None, None)?;
        transaction.commit().map_err(storage_error)?;
        Ok(document)
    }

    fn get_document(&self, id: Uuid) -> Result<DocumentSnapshot, RepositoryError> {
        let connection = self.lock()?;
        load_document(&connection, id, RevisionSelector::Current)
    }

    fn get_published_by_slug(
        &self,
        site_id: Uuid,
        slug: &str,
    ) -> Result<DocumentSnapshot, RepositoryError> {
        let connection = self.lock()?;
        let document_id: Option<String> = connection
            .query_row(
                "SELECT route.document_id
                 FROM routes route
                 JOIN documents document
                   ON document.id = route.document_id
                  AND document.site_id = route.site_id
                 WHERE route.site_id = ?1 AND route.path = ?2",
                params![site_id.to_string(), slug],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let document_id = document_id.ok_or(RepositoryError::NotFound)?;
        load_document(
            &connection,
            parse_uuid(&document_id)?,
            RevisionSelector::Published,
        )
    }

    fn list_published(
        &self,
        site_id: Uuid,
        limit: usize,
    ) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT d.id FROM documents d
                 JOIN revisions published ON published.id = d.published_revision_id
                 WHERE d.site_id = ?1
                   AND d.published_revision_id IS NOT NULL
                   AND d.status != 'archived'
                 ORDER BY published.created_at DESC, d.id DESC LIMIT ?2",
            )
            .map_err(storage_error)?;
        let ids = statement
            .query_map(params![site_id.to_string(), limit.min(500) as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        ids.into_iter()
            .map(|id| load_document(&connection, parse_uuid(&id)?, RevisionSelector::Published))
            .collect()
    }

    fn list_documents(
        &self,
        site_id: Uuid,
        limit: usize,
    ) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT id FROM documents
                 WHERE site_id = ?1
                 ORDER BY updated_at DESC, id DESC LIMIT ?2",
            )
            .map_err(storage_error)?;
        let ids = statement
            .query_map(params![site_id.to_string(), limit.min(500) as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        ids.into_iter()
            .map(|id| load_document(&connection, parse_uuid(&id)?, RevisionSelector::Current))
            .collect()
    }

    fn list_revisions(
        &self,
        document_id: Uuid,
        limit: usize,
    ) -> Result<Vec<RevisionSnapshot>, RepositoryError> {
        let connection = self.lock()?;
        let exists = connection
            .query_row(
                "SELECT 1 FROM documents WHERE id = ?1",
                params![document_id.to_string()],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if !exists {
            return Err(RepositoryError::NotFound);
        }
        let mut statement = connection
            .prepare(
                "SELECT snapshot_json FROM revisions
                 WHERE document_id = ?1
                 ORDER BY revision_number DESC LIMIT ?2",
            )
            .map_err(storage_error)?;
        statement
            .query_map(
                params![document_id.to_string(), limit.min(1_000) as i64],
                |row| row.get::<_, String>(0),
            )
            .map_err(storage_error)?
            .map(|row| {
                row.map_err(storage_error)
                    .and_then(|json| serde_json::from_str(&json).map_err(storage_error))
            })
            .collect()
    }

    fn append_revision(
        &self,
        input: ProposedRevision,
    ) -> Result<RevisionSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        let now = Utc::now();
        let revision = append_revision_in_transaction(&transaction, input, now)?;
        transaction.commit().map_err(storage_error)?;
        Ok(revision)
    }

    fn append_ai_proposal(
        &self,
        envelope: Ai2AiEnvelope,
    ) -> Result<RevisionSnapshot, RepositoryError> {
        envelope
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;

        // The envelope is the authoritative identity and idempotency boundary.
        // Preserve it byte-for-structure in the audit record while deriving the
        // accepted revision actor exactly as the prior HTTP flow did.
        let mut proposal = envelope.proposal.clone();
        proposal.actor.kind = RevisionActorKind::Agent;
        proposal.actor.id.clone_from(&envelope.actor.id);
        proposal.actor.display_name = None;
        proposal.authorship = PublicAuthorship {
            kind: PublicAuthorshipKind::AiGenerated,
            generator: Some(
                envelope
                    .actor
                    .model
                    .clone()
                    .or_else(|| envelope.actor.provider.clone())
                    .unwrap_or_else(|| "ai2ai".into()),
            ),
            human_reviewed: false,
        };
        proposal.idempotency_key = Some(envelope.idempotency_key.clone());
        proposal
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        let received_at = Utc::now();
        let revision = append_revision_in_transaction(&transaction, proposal, received_at)?;
        let record = AiProposalAuditRecord {
            schema_version: AI_PROPOSAL_AUDIT_SCHEMA_VERSION.into(),
            document_id: revision.document_id,
            accepted_revision_id: revision.id,
            received_at,
            envelope,
        };
        insert_ai_proposal(&transaction, &record)?;
        transaction.commit().map_err(storage_error)?;
        Ok(revision)
    }

    fn list_ai_proposals(
        &self,
        document_id: Uuid,
        limit: usize,
    ) -> Result<Vec<AiProposalAuditRecord>, RepositoryError> {
        let connection = self.lock()?;
        ensure_document_exists(&connection, document_id)?;
        load_ai_proposals(&connection, document_id, limit, AuditOrder::NewestFirst)
    }

    fn publish(
        &self,
        document_id: Uuid,
        revision_id: Uuid,
    ) -> Result<DocumentSnapshot, RepositoryError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        publish_in_transaction(&transaction, document_id, revision_id)?;
        transaction.commit().map_err(storage_error)?;

        load_document(&connection, document_id, RevisionSelector::Published)
    }
}

fn create_document_in_transaction(
    transaction: &Transaction<'_>,
    input: NewDocument,
    now: DateTime<Utc>,
    initial_category: Option<(Uuid, Uuid)>,
    idempotency_key: Option<&str>,
) -> Result<DocumentSnapshot, RepositoryError> {
    let document_id = Uuid::now_v7();
    let revision_id = Uuid::now_v7();
    let site_id = input.site_id;
    let revision = with_computed_hash(RevisionSnapshot {
        schema_version: CONTENT_SCHEMA_VERSION.into(),
        id: revision_id,
        document_id,
        revision_number: 1,
        parent_revision_id: None,
        title: input.title,
        slug: input.slug,
        source_markdown: input.source_markdown,
        embeds: input.embeds,
        intent: input.intent,
        ontology: input.ontology,
        ai_summary: input.ai_summary,
        authorship: input.authorship,
        actor: input.actor,
        content_hash: String::new(),
        created_at: now,
    });

    let initial_category_record = initial_category
        .map(|(_, category_id)| load_category_by_id(transaction, site_id, category_id))
        .transpose()?;
    if initial_category_record
        .as_ref()
        .is_some_and(|category| category.status != CategoryStatus::Active)
    {
        return Err(RepositoryError::Validation(
            "archived categories cannot receive revisions".into(),
        ));
    }
    if initial_category_record.is_none() {
        ensure_root_slug_not_category(transaction, site_id, &revision.slug)?;
    }
    let initial_route_path = category_route_path(initial_category_record.as_ref(), &revision.slug);
    ensure_document_route_available(transaction, site_id, document_id, &initial_route_path)?;

    transaction
        .execute(
            "INSERT INTO documents (
                id, site_id, status, current_revision_id, published_revision_id,
                current_slug, created_at, updated_at
             ) VALUES (?1, ?2, 'draft', ?3, NULL, ?4, ?5, ?5)",
            params![
                document_id.to_string(),
                site_id.to_string(),
                revision_id.to_string(),
                initial_route_path,
                now.to_rfc3339(),
            ],
        )
        .map_err(map_constraint_error)?;
    insert_revision(transaction, &revision, idempotency_key)?;
    if let Some((actor_user_id, category_id)) = initial_category {
        assign_revision_category_in_transaction(
            transaction,
            actor_user_id,
            site_id,
            document_id,
            revision_id,
            Some(category_id),
            now,
        )?;
    }

    Ok(DocumentSnapshot {
        schema_version: CONTENT_SCHEMA_VERSION.into(),
        id: document_id,
        site_id,
        status: DocumentStatus::Draft,
        current_revision_id: revision_id,
        published_revision_id: None,
        revision,
        created_at: now,
        updated_at: now,
    })
}

fn assign_revision_category_in_transaction(
    connection: &Connection,
    actor_user_id: Uuid,
    site_id: Uuid,
    document_id: Uuid,
    revision_id: Uuid,
    category_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<RevisionCategoryPlacement, RepositoryError> {
    let (current_revision_id, published_revision_id): (String, Option<String>) = connection
        .query_row(
            "SELECT current_revision_id, published_revision_id
             FROM documents WHERE id = ?1 AND site_id = ?2",
            params![document_id.to_string(), site_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    if current_revision_id != revision_id.to_string() {
        return Err(RepositoryError::RevisionConflict);
    }
    if published_revision_id.as_deref() == Some(current_revision_id.as_str()) {
        return Err(RepositoryError::Validation(
            "published revision placement is immutable; create a new revision before moving it"
                .into(),
        ));
    }
    let category = category_id
        .map(|id| load_category_by_id(connection, site_id, id))
        .transpose()?;
    if category
        .as_ref()
        .is_some_and(|category| category.status != CategoryStatus::Active)
    {
        return Err(RepositoryError::Validation(
            "archived categories cannot receive revisions".into(),
        ));
    }
    let revision_slug: String = connection
        .query_row(
            "SELECT slug FROM revisions WHERE id = ?1 AND document_id = ?2",
            params![revision_id.to_string(), document_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    let route_path = category_route_path(category.as_ref(), &revision_slug);
    ensure_document_route_available(connection, site_id, document_id, &route_path)?;
    if category.is_none() {
        ensure_root_slug_not_category(connection, site_id, &revision_slug)?;
    }
    let changed = connection
        .execute(
            "UPDATE revision_categories
             SET category_id = ?1, assigned_by_user_id = ?2, assigned_at = ?3
             WHERE revision_id = ?4 AND document_id = ?5 AND site_id = ?6",
            params![
                category_id.map(|id| id.to_string()),
                actor_user_id.to_string(),
                now.to_rfc3339(),
                revision_id.to_string(),
                document_id.to_string(),
                site_id.to_string(),
            ],
        )
        .map_err(map_category_constraint_error)?;
    if changed != 1 {
        return Err(RepositoryError::NotFound);
    }
    connection
        .execute(
            "UPDATE documents SET current_slug = ?1, updated_at = ?2
             WHERE id = ?3 AND site_id = ?4 AND current_revision_id = ?5",
            params![
                route_path,
                now.to_rfc3339(),
                document_id.to_string(),
                site_id.to_string(),
                revision_id.to_string(),
            ],
        )
        .map_err(map_constraint_error)?;
    load_revision_category_placement(connection, site_id, document_id, revision_id)
}

fn publish_in_transaction(
    transaction: &Transaction<'_>,
    document_id: Uuid,
    revision_id: Uuid,
) -> Result<(), RepositoryError> {
    publish_in_transaction_at(transaction, document_id, revision_id, Utc::now())
}

fn publish_in_transaction_at(
    transaction: &Transaction<'_>,
    document_id: Uuid,
    revision_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), RepositoryError> {
    let revision_json: Option<String> = transaction
        .query_row(
            "SELECT snapshot_json FROM revisions WHERE id = ?1 AND document_id = ?2",
            params![revision_id.to_string(), document_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    let revision: RevisionSnapshot =
        serde_json::from_str(&revision_json.ok_or(RepositoryError::NotFound)?)
            .map_err(storage_error)?;
    let site_id: String = transaction
        .query_row(
            "SELECT site_id FROM documents WHERE id = ?1",
            params![document_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    let site_uuid = parse_uuid(&site_id)?;
    let route_path = revision_category_route_path(
        transaction,
        site_uuid,
        document_id,
        revision_id,
        &revision.slug,
        true,
    )?;
    ensure_document_route_available(transaction, site_uuid, document_id, &route_path)?;
    transaction
        .execute(
            "UPDATE routes SET is_canonical = 0 WHERE document_id = ?1",
            params![document_id.to_string()],
        )
        .map_err(storage_error)?;
    transaction
        .execute(
            "INSERT INTO routes (site_id, path, document_id, is_canonical, created_at)
             VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(site_id, path) DO UPDATE SET
               document_id = CASE
                 WHEN routes.document_id = excluded.document_id THEN excluded.document_id
                 ELSE routes.document_id
               END,
               is_canonical = CASE
                 WHEN routes.document_id = excluded.document_id THEN 1
                 ELSE routes.is_canonical
               END",
            params![
                site_id,
                route_path,
                document_id.to_string(),
                now.to_rfc3339()
            ],
        )
        .map_err(map_constraint_error)?;
    let routed_document: String = transaction
        .query_row(
            "SELECT document_id FROM routes WHERE site_id = ?1 AND path = ?2",
            params![site_id, route_path],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    if routed_document != document_id.to_string() {
        return Err(RepositoryError::DuplicateSlug);
    }
    ensure_series_item_for_revision_in_transaction(
        transaction,
        site_uuid,
        document_id,
        revision_id,
        now,
    )?;
    transaction
        .execute(
            "UPDATE documents SET status = 'published', published_revision_id = ?1,
             updated_at = ?2 WHERE id = ?3",
            params![
                revision_id.to_string(),
                now.to_rfc3339(),
                document_id.to_string()
            ],
        )
        .map_err(storage_error)?;
    canonicalize_home_pins_for_changed_site_in_transaction(transaction, site_uuid)?;
    Ok(())
}

fn append_revision_in_transaction(
    transaction: &Transaction<'_>,
    input: ProposedRevision,
    now: DateTime<Utc>,
) -> Result<RevisionSnapshot, RepositoryError> {
    append_revision_in_transaction_with_category(transaction, input, now, None)
}

fn append_revision_in_transaction_with_category(
    transaction: &Transaction<'_>,
    input: ProposedRevision,
    now: DateTime<Utc>,
    category_assignment: Option<(Uuid, Option<Uuid>)>,
) -> Result<RevisionSnapshot, RepositoryError> {
    input
        .validate()
        .map_err(|error| RepositoryError::Validation(error.to_string()))?;
    let ProposedRevision {
        document_id,
        base_revision_id,
        title,
        slug,
        source_markdown,
        embeds,
        intent,
        ontology,
        ai_summary,
        authorship,
        actor,
        idempotency_key,
    } = input;
    let current: Option<(String, i64, String)> = transaction
        .query_row(
            "SELECT d.current_revision_id, r.revision_number, d.site_id
             FROM documents d
             JOIN revisions r ON r.id = d.current_revision_id
             WHERE d.id = ?1",
            params![document_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(storage_error)?;
    let (current_revision_id, revision_number, site_id) =
        current.ok_or(RepositoryError::NotFound)?;
    if current_revision_id != base_revision_id.to_string() {
        return Err(RepositoryError::RevisionConflict);
    }

    let revision = with_computed_hash(RevisionSnapshot {
        schema_version: CONTENT_SCHEMA_VERSION.into(),
        id: Uuid::now_v7(),
        document_id,
        revision_number: (revision_number + 1) as u64,
        parent_revision_id: Some(base_revision_id),
        title,
        slug,
        source_markdown,
        embeds,
        intent,
        ontology,
        ai_summary,
        authorship,
        actor,
        content_hash: String::new(),
        created_at: now,
    });
    insert_revision(transaction, &revision, idempotency_key.as_deref())?;
    let site_id = parse_uuid(&site_id)?;
    if let Some((actor_user_id, category_id)) = category_assignment {
        let category = category_id
            .map(|id| load_category_by_id(transaction, site_id, id))
            .transpose()?;
        if category
            .as_ref()
            .is_some_and(|category| category.status != CategoryStatus::Active)
        {
            return Err(RepositoryError::Validation(
                "archived categories cannot receive revisions".into(),
            ));
        }
        let changed = transaction
            .execute(
                "UPDATE revision_categories
                 SET category_id = ?1, assigned_by_user_id = ?2, assigned_at = ?3
                 WHERE revision_id = ?4 AND document_id = ?5 AND site_id = ?6",
                params![
                    category_id.map(|id| id.to_string()),
                    actor_user_id.to_string(),
                    now.to_rfc3339(),
                    revision.id.to_string(),
                    revision.document_id.to_string(),
                    site_id.to_string(),
                ],
            )
            .map_err(map_category_constraint_error)?;
        if changed != 1 {
            return Err(RepositoryError::NotFound);
        }
    }
    let route_path = revision_category_route_path(
        transaction,
        site_id,
        revision.document_id,
        revision.id,
        &revision.slug,
        false,
    )?;
    ensure_document_route_available(transaction, site_id, revision.document_id, &route_path)?;
    transaction
        .execute(
            "UPDATE documents
             SET current_revision_id = ?1,
                 current_slug = ?2,
                 status = CASE
                   WHEN published_revision_id IS NULL THEN 'draft'
                   ELSE 'published'
                 END,
                 updated_at = ?3
             WHERE id = ?4",
            params![
                revision.id.to_string(),
                route_path,
                revision.created_at.to_rfc3339(),
                revision.document_id.to_string(),
            ],
        )
        .map_err(map_constraint_error)?;
    Ok(revision)
}

fn with_computed_hash(mut revision: RevisionSnapshot) -> RevisionSnapshot {
    revision.content_hash = content_hash_with_ai_summary(
        &revision.title,
        &revision.slug,
        &revision.source_markdown,
        &revision.embeds,
        revision.intent.as_ref(),
        revision.ontology.as_ref(),
        revision.ai_summary.as_ref(),
    );
    revision
}

fn insert_revision(
    transaction: &Transaction<'_>,
    revision: &RevisionSnapshot,
    idempotency_key: Option<&str>,
) -> Result<(), RepositoryError> {
    transaction
        .execute(
            "INSERT INTO revisions (
                id, document_id, revision_number, parent_revision_id, slug,
                snapshot_json, idempotency_key, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                revision.id.to_string(),
                revision.document_id.to_string(),
                revision.revision_number as i64,
                revision.parent_revision_id.map(|id| id.to_string()),
                revision.slug,
                serde_json::to_string(revision).map_err(storage_error)?,
                idempotency_key,
                revision.created_at.to_rfc3339(),
            ],
        )
        .map_err(|error| {
            let text = error.to_string();
            if text.contains("idempotency_key") {
                RepositoryError::DuplicateIdempotencyKey
            } else {
                map_constraint_error(error)
            }
        })?;
    Ok(())
}

fn insert_ai_proposal(
    transaction: &Transaction<'_>,
    record: &AiProposalAuditRecord,
) -> Result<(), RepositoryError> {
    transaction
        .execute(
            "INSERT INTO ai_proposal_audits (
                schema_version, accepted_revision_id, document_id, message_id,
                idempotency_key, received_at, envelope_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.schema_version,
                record.accepted_revision_id.to_string(),
                record.document_id.to_string(),
                record.envelope.message_id.to_string(),
                record.envelope.idempotency_key,
                record.received_at.to_rfc3339(),
                serde_json::to_string(&record.envelope).map_err(storage_error)?,
            ],
        )
        .map_err(map_ai_proposal_constraint_error)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum AuditOrder {
    NewestFirst,
    OldestFirst,
}

fn load_ai_proposals(
    connection: &Connection,
    document_id: Uuid,
    limit: usize,
    order: AuditOrder,
) -> Result<Vec<AiProposalAuditRecord>, RepositoryError> {
    let direction = match order {
        AuditOrder::NewestFirst => "DESC",
        AuditOrder::OldestFirst => "ASC",
    };
    let sql = format!(
        "SELECT schema_version, accepted_revision_id, document_id, message_id,
                idempotency_key, received_at, envelope_json
         FROM ai_proposal_audits
         WHERE document_id = ?1
         ORDER BY received_at {direction}, accepted_revision_id {direction}
         LIMIT ?2"
    );
    let sql_limit = if limit == usize::MAX {
        i64::MAX
    } else {
        limit.min(1_000) as i64
    };
    let mut statement = connection.prepare(&sql).map_err(storage_error)?;
    statement
        .query_map(params![document_id.to_string(), sql_limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(storage_error)?
        .map(|row| {
            let (
                schema_version,
                accepted_revision_id,
                stored_document_id,
                message_id,
                idempotency_key,
                received_at,
                envelope_json,
            ) = row.map_err(storage_error)?;
            let envelope: Ai2AiEnvelope =
                serde_json::from_str(&envelope_json).map_err(storage_error)?;
            let stored_document_id = parse_uuid(&stored_document_id)?;
            if stored_document_id != document_id
                || envelope.proposal.document_id != document_id
                || envelope.message_id.to_string() != message_id
                || envelope.idempotency_key != idempotency_key
            {
                return Err(RepositoryError::Storage(
                    "AI proposal audit columns do not match the stored envelope".into(),
                ));
            }
            Ok(AiProposalAuditRecord {
                schema_version,
                document_id,
                accepted_revision_id: parse_uuid(&accepted_revision_id)?,
                received_at: parse_datetime(&received_at)?,
                envelope,
            })
        })
        .collect()
}

#[derive(Clone, Copy)]
enum RevisionSelector {
    Current,
    Published,
}

#[derive(Clone, Copy)]
enum CategoryPostOrder {
    NewestFirst,
    OldestFirst,
}

type StoredDocumentRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
);

fn load_document(
    connection: &Connection,
    document_id: Uuid,
    selector: RevisionSelector,
) -> Result<DocumentSnapshot, RepositoryError> {
    let (revision_column, updated_column, visibility_clause) = match selector {
        RevisionSelector::Current => ("d.current_revision_id", "d.updated_at", ""),
        RevisionSelector::Published => (
            "d.published_revision_id",
            "r.created_at",
            "AND d.published_revision_id IS NOT NULL AND d.status != 'archived'",
        ),
    };
    let sql = format!(
        "SELECT d.site_id, d.status, d.current_revision_id, d.published_revision_id,
                d.created_at, {updated_column}, r.snapshot_json
         FROM documents d
         JOIN revisions r ON r.id = {revision_column} AND r.document_id = d.id
         WHERE d.id = ?1 {visibility_clause}"
    );
    let raw: Option<StoredDocumentRow> = connection
        .query_row(&sql, params![document_id.to_string()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .optional()
        .map_err(storage_error)?;
    let (site_id, status, current_revision_id, published_revision_id, created_at, updated_at, json) =
        raw.ok_or(RepositoryError::NotFound)?;
    Ok(DocumentSnapshot {
        schema_version: CONTENT_SCHEMA_VERSION.into(),
        id: document_id,
        site_id: parse_uuid(&site_id)?,
        status: parse_status(&status)?,
        current_revision_id: parse_uuid(&current_revision_id)?,
        published_revision_id: published_revision_id
            .map(|value| parse_uuid(&value))
            .transpose()?,
        revision: serde_json::from_str(&json).map_err(storage_error)?,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    })
}

fn list_documents_with_selector(
    connection: &Connection,
    site_id: Option<Uuid>,
    limit: usize,
    selector: RevisionSelector,
) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
    let site_id = site_id.map(|id| id.to_string());
    let sql = match selector {
        RevisionSelector::Current => {
            "SELECT d.id
             FROM documents d
             JOIN sites community_site ON community_site.id = d.site_id
             WHERE (?1 IS NULL OR d.site_id = ?1)
             ORDER BY d.updated_at DESC, d.id DESC LIMIT ?2"
        }
        RevisionSelector::Published => {
            "SELECT d.id
             FROM documents d
             JOIN sites community_site ON community_site.id = d.site_id
             JOIN revisions published ON published.id = d.published_revision_id
             WHERE (?1 IS NULL OR d.site_id = ?1)
               AND d.published_revision_id IS NOT NULL
               AND d.status != 'archived'
             ORDER BY published.created_at DESC, d.id DESC LIMIT ?2"
        }
    };
    let mut statement = connection.prepare(sql).map_err(storage_error)?;
    let ids = statement
        .query_map(params![site_id, limit.min(500) as i64], |row| {
            row.get::<_, String>(0)
        })
        .map_err(storage_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_error)?;
    ids.into_iter()
        .map(|id| load_document(connection, parse_uuid(&id)?, selector))
        .collect()
}

/// Lists newest installation-wide community posts without first consuming the
/// bound with primary-site Series members. Secondary-site posts intentionally
/// remain peer standalone units because only the primary site owns the ordered
/// home Series projection. At most three pinned representatives are
/// over-fetched and removed so the returned standalone pool can still fill its
/// independent public-home limit.
fn list_home_standalone_published_with_connection(
    connection: &Connection,
    primary_site_id: Uuid,
    excluded_ids: &BTreeSet<Uuid>,
    limit: usize,
) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
    let limit = limit.min(HOME_FEED_MAX_SECTION_ITEMS);
    let fetch_limit = limit.saturating_add(excluded_ids.len());
    let mut statement = connection
        .prepare(
            "SELECT document.id
             FROM documents document
             JOIN sites community_site ON community_site.id = document.site_id
             JOIN revisions published ON published.id = document.published_revision_id
             WHERE document.published_revision_id IS NOT NULL
               AND document.status != 'archived'
               AND NOT EXISTS (
                 SELECT 1
                 FROM revision_categories placement
                 JOIN series
                   ON series.category_id = placement.category_id
                  AND series.site_id = placement.site_id
                 JOIN categories category
                   ON category.id = series.category_id
                  AND category.site_id = series.site_id
                 WHERE placement.revision_id = document.published_revision_id
                   AND placement.document_id = document.id
                   AND placement.site_id = document.site_id
                   AND series.site_id = ?1
                   AND category.status = 'active'
               )
             ORDER BY published.created_at DESC, document.id DESC
             LIMIT ?2",
        )
        .map_err(storage_error)?;
    let ids = statement
        .query_map(
            params![primary_site_id.to_string(), page_parameter(fetch_limit)?],
            |row| row.get::<_, String>(0),
        )
        .map_err(storage_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_error)?;
    ids.into_iter()
        .map(|id| parse_uuid(&id))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|id| !excluded_ids.contains(id))
        .take(limit)
        .map(|id| load_document(connection, id, RevisionSelector::Published))
        .collect()
}

fn list_published_in_category_with_connection(
    connection: &Connection,
    site_id: Uuid,
    category_id: Uuid,
    limit: usize,
    order: CategoryPostOrder,
) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
    let ordering = match order {
        CategoryPostOrder::NewestFirst => "published.created_at DESC, document.id DESC",
        CategoryPostOrder::OldestFirst => "document.created_at ASC, document.id ASC",
    };
    let sql = format!(
        "SELECT document.id
         FROM documents document
         JOIN revision_categories placement
           ON placement.revision_id = document.published_revision_id
          AND placement.document_id = document.id
          AND placement.site_id = document.site_id
         JOIN revisions published ON published.id = document.published_revision_id
         WHERE document.site_id = ?1 AND placement.category_id = ?2
           AND document.published_revision_id IS NOT NULL
           AND document.status != 'archived'
         ORDER BY {ordering} LIMIT ?3"
    );
    let mut statement = connection.prepare(&sql).map_err(storage_error)?;
    let ids = statement
        .query_map(
            params![
                site_id.to_string(),
                category_id.to_string(),
                limit.min(500) as i64
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(storage_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_error)?;
    ids.into_iter()
        .map(|id| load_document(connection, parse_uuid(&id)?, RevisionSelector::Published))
        .collect()
}

type StoredSeriesItemRow = (String, String, String, i64, String);

fn stored_series_item_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSeriesItemRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn parse_series_item_row(raw: StoredSeriesItemRow) -> Result<SeriesItemRecord, RepositoryError> {
    let (series_id, site_id, document_id, position, added_at) = raw;
    Ok(SeriesItemRecord {
        series_id: parse_uuid(&series_id)?,
        site_id: parse_uuid(&site_id)?,
        document_id: parse_uuid(&document_id)?,
        position: u64::try_from(position)
            .map_err(|error| RepositoryError::Storage(error.to_string()))?,
        added_at: parse_datetime(&added_at)?,
    })
}

fn list_published_series_items_with_connection(
    connection: &Connection,
    site_id: Uuid,
    series_id: Uuid,
    limit: usize,
) -> Result<Vec<SeriesItemRecord>, RepositoryError> {
    let mut statement = connection
        .prepare(
            "SELECT item.series_id, item.site_id, item.document_id,
                    item.position, item.added_at
             FROM series_items item
             JOIN series
               ON series.id = item.series_id AND series.site_id = item.site_id
             JOIN documents document
               ON document.id = item.document_id AND document.site_id = item.site_id
             JOIN revision_categories placement
               ON placement.revision_id = document.published_revision_id
              AND placement.document_id = document.id
              AND placement.site_id = document.site_id
             WHERE item.site_id = ?1 AND item.series_id = ?2
               AND placement.category_id = series.category_id
               AND document.published_revision_id IS NOT NULL
               AND document.status != 'archived'
             ORDER BY item.position ASC, item.document_id ASC
             LIMIT ?3",
        )
        .map_err(storage_error)?;
    statement
        .query_map(
            params![
                site_id.to_string(),
                series_id.to_string(),
                limit.min(501) as i64
            ],
            stored_series_item_row,
        )
        .map_err(storage_error)?
        .map(|row| row.map_err(storage_error).and_then(parse_series_item_row))
        .collect()
}

fn list_published_in_series_with_connection(
    connection: &Connection,
    site_id: Uuid,
    series_id: Uuid,
    limit: usize,
) -> Result<Vec<DocumentSnapshot>, RepositoryError> {
    list_published_series_items_with_connection(connection, site_id, series_id, limit)?
        .into_iter()
        .map(|item| load_document(connection, item.document_id, RevisionSelector::Published))
        .collect()
}

fn ensure_series_item_for_revision_in_transaction(
    transaction: &Transaction<'_>,
    site_id: Uuid,
    document_id: Uuid,
    revision_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), RepositoryError> {
    let series_id: Option<String> = transaction
        .query_row(
            "SELECT series.id
             FROM revision_categories placement
             JOIN series
               ON series.category_id = placement.category_id
              AND series.site_id = placement.site_id
             WHERE placement.revision_id = ?1
               AND placement.document_id = ?2
               AND placement.site_id = ?3",
            params![
                revision_id.to_string(),
                document_id.to_string(),
                site_id.to_string()
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    let Some(series_id) = series_id else {
        return Ok(());
    };
    let already_present = transaction
        .query_row(
            "SELECT 1 FROM series_items
             WHERE series_id = ?1 AND site_id = ?2 AND document_id = ?3",
            params![series_id, site_id.to_string(), document_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some();
    if already_present {
        return Ok(());
    }
    let published_count: i64 = transaction
        .query_row(
            "SELECT COUNT(*)
             FROM series_items item
             JOIN series
               ON series.id = item.series_id AND series.site_id = item.site_id
             JOIN documents document
               ON document.id = item.document_id AND document.site_id = item.site_id
             JOIN revision_categories placement
               ON placement.revision_id = document.published_revision_id
              AND placement.document_id = document.id
              AND placement.site_id = document.site_id
             WHERE item.series_id = ?1 AND item.site_id = ?2
               AND placement.category_id = series.category_id
               AND document.published_revision_id IS NOT NULL
               AND document.status != 'archived'",
            params![series_id, site_id.to_string()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    if published_count >= 500 {
        return Err(RepositoryError::Validation(
            "a series cannot contain more than 500 published documents".into(),
        ));
    }
    let next_position: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(position), 0) + ?1
             FROM series_items WHERE series_id = ?2 AND site_id = ?3",
            params![SERIES_POSITION_STEP as i64, series_id, site_id.to_string()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    if next_position <= 0 {
        return Err(RepositoryError::Validation(
            "series position overflow".into(),
        ));
    }
    transaction
        .execute(
            "INSERT INTO series_items (
                series_id, site_id, document_id, position, added_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                series_id,
                site_id.to_string(),
                document_id.to_string(),
                next_position,
                now.to_rfc3339(),
            ],
        )
        .map_err(map_series_constraint_error)?;
    Ok(())
}

type StoredUserRow = (String, String, String, String, String, String, String);

fn stored_user_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredUserRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn parse_user_row(raw: StoredUserRow) -> Result<UserRecord, RepositoryError> {
    let (id, email, handle, display_name, password_phc, created_at, updated_at) = raw;
    Ok(UserRecord {
        id: parse_uuid(&id)?,
        email,
        handle,
        display_name,
        password_phc,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    })
}

fn load_user_by_id(connection: &Connection, id: Uuid) -> Result<UserRecord, RepositoryError> {
    let raw = connection
        .query_row(
            "SELECT id, email, handle, display_name, password_phc, created_at, updated_at
             FROM users WHERE id = ?1",
            params![id.to_string()],
            stored_user_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    parse_user_row(raw)
}

fn load_user_by_column(
    connection: &Connection,
    column: &str,
    value: &str,
) -> Result<UserRecord, RepositoryError> {
    debug_assert!(matches!(column, "email" | "handle"));
    let sql = format!(
        "SELECT id, email, handle, display_name, password_phc, created_at, updated_at
         FROM users WHERE {column} = ?1 COLLATE NOCASE"
    );
    let raw = connection
        .query_row(&sql, params![value], stored_user_row)
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    parse_user_row(raw)
}

type StoredSessionRow = (String, String, i64, String, String, String, Option<String>);

fn stored_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSessionRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn parse_session_row(raw: StoredSessionRow) -> Result<SessionRecord, RepositoryError> {
    let (id, user_id, auth_epoch, auth_method, expires_at, created_at, revoked_at) = raw;
    Ok(SessionRecord {
        id: parse_uuid(&id)?,
        user_id: parse_uuid(&user_id)?,
        auth_epoch: u64::try_from(auth_epoch)
            .map_err(|_| RepositoryError::Storage("session auth epoch is invalid".into()))?,
        auth_method: SessionAuthMethod::from_str(&auth_method)?,
        expires_at: parse_datetime(&expires_at)?,
        created_at: parse_datetime(&created_at)?,
        revoked_at: revoked_at.as_deref().map(parse_datetime).transpose()?,
    })
}

fn load_session_by_id(connection: &Connection, id: Uuid) -> Result<SessionRecord, RepositoryError> {
    let raw = connection
        .query_row(
            "SELECT id, user_id, auth_epoch, auth_method, expires_at, created_at, revoked_at
             FROM sessions WHERE id = ?1",
            params![id.to_string()],
            stored_session_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    parse_session_row(raw)
}

fn insert_session(
    connection: &Connection,
    user_id: Uuid,
    token_hash: &[u8],
    expires_at: DateTime<Utc>,
    auth_epoch: u64,
    auth_method: SessionAuthMethod,
) -> Result<SessionRecord, RepositoryError> {
    validate_token_hash(token_hash)?;
    validate_session_expiry(expires_at)?;
    let auth_epoch = i64::try_from(auth_epoch)
        .map_err(|_| RepositoryError::Validation("session auth epoch is too large".into()))?;
    let id = Uuid::now_v7();
    let now = Utc::now();
    connection
        .execute(
            "INSERT INTO sessions (
                id, token_hash, user_id, auth_epoch, auth_method,
                expires_at, created_at, revoked_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                id.to_string(),
                token_hash,
                user_id.to_string(),
                auth_epoch,
                auth_method.as_str(),
                expires_at.to_rfc3339(),
                now.to_rfc3339(),
            ],
        )
        .map_err(map_community_constraint_error)?;
    load_session_by_id(connection, id)
}

fn load_active_session_by_hash(
    connection: &Connection,
    token_hash: &[u8],
) -> Result<SessionRecord, RepositoryError> {
    let raw: Option<StoredSessionRow> = connection
        .query_row(
            "SELECT id, user_id, auth_epoch, auth_method, expires_at, created_at, revoked_at
             FROM sessions
             WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![token_hash],
            stored_session_row,
        )
        .optional()
        .map_err(storage_error)?;
    let session = raw
        .map(parse_session_row)
        .transpose()?
        .ok_or(RepositoryError::NotFound)?;
    if session.expires_at <= Utc::now() {
        Err(RepositoryError::NotFound)
    } else {
        Ok(session)
    }
}

type StoredAdminControlPlaneRow = (String, String, String, i64, bool, Vec<u8>, String, String);

fn stored_admin_control_plane_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredAdminControlPlaneRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn parse_admin_control_plane_row(
    raw: StoredAdminControlPlaneRow,
) -> Result<AdminControlPlaneRecord, RepositoryError> {
    let (
        primary_site_id,
        owner_user_id,
        auth_mode,
        auth_epoch,
        setup_complete,
        binding_fingerprint,
        created_at,
        updated_at,
    ) = raw;
    Ok(AdminControlPlaneRecord {
        primary_site_id: parse_uuid(&primary_site_id)?,
        owner_user_id: parse_uuid(&owner_user_id)?,
        auth_mode: AdminAuthMode::from_str(&auth_mode)?,
        auth_epoch: u64::try_from(auth_epoch)
            .map_err(|_| RepositoryError::Storage("admin auth epoch is invalid".into()))?,
        setup_complete,
        binding_fingerprint: fixed_hash(&binding_fingerprint, "admin binding fingerprint")?,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    })
}

fn load_admin_control_plane_optional(
    connection: &Connection,
) -> Result<Option<AdminControlPlaneRecord>, RepositoryError> {
    connection
        .query_row(
            "SELECT primary_site_id, owner_user_id, auth_mode, auth_epoch,
                    setup_complete, binding_fingerprint, created_at, updated_at
             FROM admin_control_plane WHERE singleton = 1",
            [],
            stored_admin_control_plane_row,
        )
        .optional()
        .map_err(storage_error)?
        .map(parse_admin_control_plane_row)
        .transpose()
}

fn load_admin_control_plane(
    connection: &Connection,
) -> Result<AdminControlPlaneRecord, RepositoryError> {
    load_admin_control_plane_optional(connection)?.ok_or(RepositoryError::NotFound)
}

fn insert_admin_control_plane(
    connection: &Connection,
    primary_site_id: Uuid,
    owner_user_id: Uuid,
    auth_mode: AdminAuthMode,
    binding_fingerprint: &[u8],
    setup_complete: bool,
) -> Result<AdminControlPlaneRecord, RepositoryError> {
    validate_fingerprint(binding_fingerprint)?;
    ensure_site_owner(connection, owner_user_id, primary_site_id)?;
    let now = Utc::now();
    connection
        .execute(
            "INSERT INTO admin_control_plane (
                singleton, primary_site_id, owner_user_id, auth_mode, auth_epoch,
                setup_complete, binding_fingerprint, created_at, updated_at
             ) VALUES (1, ?1, ?2, ?3, 1, ?4, ?5, ?6, ?6)",
            params![
                primary_site_id.to_string(),
                owner_user_id.to_string(),
                auth_mode.as_str(),
                setup_complete,
                binding_fingerprint,
                now.to_rfc3339(),
            ],
        )
        .map_err(map_community_constraint_error)?;
    load_admin_control_plane(connection)
}

fn validate_control_plane_binding(
    existing: &AdminControlPlaneRecord,
    primary_site_id: Uuid,
    auth_mode: AdminAuthMode,
    binding_fingerprint: &[u8],
) -> Result<(), RepositoryError> {
    if existing.primary_site_id == primary_site_id
        && existing.auth_mode == auth_mode
        && existing.binding_fingerprint.as_slice() == binding_fingerprint
    {
        Ok(())
    } else {
        Err(RepositoryError::Validation(
            "admin control-plane binding differs from persisted state; use an explicit authentication migration or rotation"
                .into(),
        ))
    }
}

type StoredExternalIdentityRow = (String, String, Vec<u8>, String, String, String);

fn stored_external_identity_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredExternalIdentityRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn parse_external_identity_row(
    raw: StoredExternalIdentityRow,
) -> Result<ExternalIdentityRecord, RepositoryError> {
    let (adapter, issuer, subject_hash, user_id, created_at, last_seen_at) = raw;
    Ok(ExternalIdentityRecord {
        adapter,
        issuer,
        subject_hash: fixed_hash(&subject_hash, "external subject hash")?,
        user_id: parse_uuid(&user_id)?,
        created_at: parse_datetime(&created_at)?,
        last_seen_at: parse_datetime(&last_seen_at)?,
    })
}

fn load_external_identity_optional(
    connection: &Connection,
    adapter: &str,
    issuer: &str,
    subject_hash: &[u8],
) -> Result<Option<ExternalIdentityRecord>, RepositoryError> {
    connection
        .query_row(
            "SELECT adapter, issuer, subject_hash, user_id, created_at, last_seen_at
             FROM external_identities
             WHERE adapter = ?1 AND issuer = ?2 AND subject_hash = ?3",
            params![adapter, issuer, subject_hash],
            stored_external_identity_row,
        )
        .optional()
        .map_err(storage_error)?
        .map(parse_external_identity_row)
        .transpose()
}

fn load_external_identity(
    connection: &Connection,
    adapter: &str,
    issuer: &str,
    subject_hash: &[u8],
) -> Result<ExternalIdentityRecord, RepositoryError> {
    load_external_identity_optional(connection, adapter, issuer, subject_hash)?
        .ok_or(RepositoryError::NotFound)
}

type StoredSiteRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
    Option<String>,
    String,
    String,
);

fn stored_site_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSiteRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn parse_site_row(raw: StoredSiteRow) -> Result<SiteRecord, RepositoryError> {
    let (
        id,
        handle,
        title,
        description,
        owner_user_id,
        profile,
        theme_revision,
        custom_css,
        created_at,
        updated_at,
    ) = raw;
    Ok(SiteRecord {
        id: parse_uuid(&id)?,
        handle,
        title,
        description,
        owner_user_id: parse_uuid(&owner_user_id)?,
        theme_profile: ThemeProfile::from_str(&profile)?,
        theme_revision: u64::try_from(theme_revision)
            .map_err(|error| RepositoryError::Storage(error.to_string()))?,
        custom_css,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    })
}

fn load_site_by_id(
    connection: &Connection,
    id: Uuid,
    owner_user_id: Option<Uuid>,
) -> Result<SiteRecord, RepositoryError> {
    let owner_user_id = owner_user_id.map(|id| id.to_string());
    let raw = connection
        .query_row(
            "SELECT s.id, s.handle, s.title, s.description, membership.user_id,
                    theme.profile, theme.revision, theme.custom_css,
                    s.created_at, s.updated_at
             FROM sites s
             JOIN site_memberships membership
               ON membership.site_id = s.id AND membership.role = 'owner'
             JOIN site_theme_revisions theme
               ON theme.site_id = s.id AND theme.revision = s.current_theme_revision
             WHERE s.id = ?1 AND (?2 IS NULL OR membership.user_id = ?2)",
            params![id.to_string(), owner_user_id],
            stored_site_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    parse_site_row(raw)
}

fn load_sites(
    connection: &Connection,
    owner_user_id: Option<Uuid>,
    limit: usize,
) -> Result<Vec<SiteRecord>, RepositoryError> {
    let owner_user_id = owner_user_id.map(|id| id.to_string());
    let mut statement = connection
        .prepare(
            "SELECT s.id, s.handle, s.title, s.description, membership.user_id,
                    theme.profile, theme.revision, theme.custom_css,
                    s.created_at, s.updated_at
             FROM sites s
             JOIN site_memberships membership
               ON membership.site_id = s.id AND membership.role = 'owner'
             JOIN site_theme_revisions theme
               ON theme.site_id = s.id AND theme.revision = s.current_theme_revision
             WHERE (?1 IS NULL OR membership.user_id = ?1)
             ORDER BY s.created_at DESC, s.id DESC LIMIT ?2",
        )
        .map_err(storage_error)?;
    statement
        .query_map(
            params![owner_user_id, limit.min(500) as i64],
            stored_site_row,
        )
        .map_err(storage_error)?
        .map(|row| row.map_err(storage_error).and_then(parse_site_row))
        .collect()
}

fn create_category_in_transaction(
    transaction: &Transaction<'_>,
    owner_user_id: Uuid,
    site_id: Uuid,
    input: CreateCategoryInput,
) -> Result<CategoryRecord, RepositoryError> {
    let slug = normalize_new_category_slug(&input.slug)?;
    let title = validate_required_text(&input.title, "category title", 200)?;
    let description =
        validate_optional_text(input.description.as_deref(), "category description", 2_000)?;
    ensure_category_landing_available(transaction, site_id, &slug)?;
    let id = Uuid::now_v7();
    let now = Utc::now();
    transaction
        .execute(
            "INSERT INTO categories (
                id, site_id, slug, title, description, theme_profile, status,
                created_by_user_id, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?8)",
            params![
                id.to_string(),
                site_id.to_string(),
                slug,
                title,
                description,
                input.theme_profile.map(ThemeProfile::as_str),
                owner_user_id.to_string(),
                now.to_rfc3339(),
            ],
        )
        .map_err(map_category_constraint_error)?;
    load_category_by_id(transaction, site_id, id)
}

type StoredCategoryRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    String,
);

fn stored_category_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCategoryRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn parse_category_row(raw: StoredCategoryRow) -> Result<CategoryRecord, RepositoryError> {
    let (
        id,
        site_id,
        slug,
        title,
        description,
        theme_profile,
        status,
        created_by_user_id,
        created_at,
        updated_at,
    ) = raw;
    Ok(CategoryRecord {
        id: parse_uuid(&id)?,
        site_id: parse_uuid(&site_id)?,
        slug,
        title,
        description,
        theme_profile: theme_profile
            .as_deref()
            .map(ThemeProfile::from_str)
            .transpose()?,
        status: CategoryStatus::from_str(&status)?,
        created_by_user_id: parse_uuid(&created_by_user_id)?,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    })
}

fn load_category_by_id(
    connection: &Connection,
    site_id: Uuid,
    category_id: Uuid,
) -> Result<CategoryRecord, RepositoryError> {
    connection
        .query_row(
            "SELECT id, site_id, slug, title, description, theme_profile, status,
                    created_by_user_id, created_at, updated_at
             FROM categories WHERE id = ?1 AND site_id = ?2",
            params![category_id.to_string(), site_id.to_string()],
            stored_category_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)
        .and_then(parse_category_row)
}

fn load_category_by_slug(
    connection: &Connection,
    site_id: Uuid,
    slug: &str,
) -> Result<CategoryRecord, RepositoryError> {
    connection
        .query_row(
            "SELECT id, site_id, slug, title, description, theme_profile, status,
                    created_by_user_id, created_at, updated_at
             FROM categories WHERE site_id = ?1 AND slug = ?2 COLLATE NOCASE",
            params![site_id.to_string(), slug],
            stored_category_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)
        .and_then(parse_category_row)
}

const SERIES_POSITION_STEP: u64 = 1_024;

type StoredSeriesRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    String,
    String,
    String,
);

fn stored_series_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSeriesRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

fn parse_series_row(raw: StoredSeriesRow) -> Result<SeriesRecord, RepositoryError> {
    let (
        id,
        site_id,
        category_id,
        slug,
        title,
        description,
        theme_profile,
        status,
        home_position,
        created_by_user_id,
        created_at,
        updated_at,
    ) = raw;
    Ok(SeriesRecord {
        id: parse_uuid(&id)?,
        site_id: parse_uuid(&site_id)?,
        category_id: parse_uuid(&category_id)?,
        slug,
        title,
        description,
        theme_profile: theme_profile
            .as_deref()
            .map(ThemeProfile::from_str)
            .transpose()?,
        status: CategoryStatus::from_str(&status)?,
        home_position: u64::try_from(home_position)
            .map_err(|error| RepositoryError::Storage(error.to_string()))?,
        created_by_user_id: parse_uuid(&created_by_user_id)?,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    })
}

fn series_select() -> &'static str {
    "SELECT series.id, series.site_id, series.category_id,
            category.slug, category.title, category.description,
            category.theme_profile, category.status, series.home_position,
            series.created_by_user_id, series.created_at,
            CASE
              WHEN category.updated_at > series.updated_at THEN category.updated_at
              ELSE series.updated_at
            END
     FROM series
     JOIN categories category
       ON category.id = series.category_id AND category.site_id = series.site_id"
}

fn load_series_by_id(
    connection: &Connection,
    site_id: Uuid,
    series_id: Uuid,
) -> Result<SeriesRecord, RepositoryError> {
    connection
        .query_row(
            &format!(
                "{} WHERE series.id = ?1 AND series.site_id = ?2",
                series_select()
            ),
            params![series_id.to_string(), site_id.to_string()],
            stored_series_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)
        .and_then(parse_series_row)
}

fn load_series_by_category_id(
    connection: &Connection,
    site_id: Uuid,
    category_id: Uuid,
) -> Result<SeriesRecord, RepositoryError> {
    connection
        .query_row(
            &format!(
                "{} WHERE series.category_id = ?1 AND series.site_id = ?2",
                series_select()
            ),
            params![category_id.to_string(), site_id.to_string()],
            stored_series_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)
        .and_then(parse_series_row)
}

fn load_series_by_slug(
    connection: &Connection,
    site_id: Uuid,
    slug: &str,
) -> Result<SeriesRecord, RepositoryError> {
    connection
        .query_row(
            &format!(
                "{} WHERE series.site_id = ?1 AND category.slug = ?2 COLLATE NOCASE",
                series_select()
            ),
            params![site_id.to_string(), slug],
            stored_series_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)
        .and_then(parse_series_row)
}

fn list_series_with_connection(
    connection: &Connection,
    site_id: Uuid,
    include_archived: bool,
    limit: usize,
) -> Result<Vec<SeriesRecord>, RepositoryError> {
    let sql = format!(
        "{} WHERE series.site_id = ?1 AND (?2 OR category.status = 'active')
         ORDER BY CASE category.status WHEN 'active' THEN 0 ELSE 1 END,
                  series.home_position, series.id
         LIMIT ?3",
        series_select()
    );
    let mut statement = connection.prepare(&sql).map_err(storage_error)?;
    statement
        .query_map(
            params![site_id.to_string(), include_archived, limit.min(500) as i64],
            stored_series_row,
        )
        .map_err(storage_error)?
        .map(|row| row.map_err(storage_error).and_then(parse_series_row))
        .collect()
}

fn create_series_for_category_in_transaction(
    transaction: &Transaction<'_>,
    owner_user_id: Uuid,
    site_id: Uuid,
    category_id: Uuid,
) -> Result<SeriesRecord, RepositoryError> {
    load_category_by_id(transaction, site_id, category_id)?;
    let series_count: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM series WHERE site_id = ?1",
            params![site_id.to_string()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    if series_count >= 500 {
        return Err(RepositoryError::Validation(
            "a site cannot contain more than 500 series".into(),
        ));
    }
    let next_home_position: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(home_position), 0) + ?1
             FROM series WHERE site_id = ?2",
            params![SERIES_POSITION_STEP as i64, site_id.to_string()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    if next_home_position <= 0 {
        return Err(RepositoryError::Validation(
            "series home position overflow".into(),
        ));
    }
    let series_id = Uuid::now_v7();
    let now = Utc::now();
    transaction
        .execute(
            "INSERT INTO series (
                id, site_id, category_id, home_position, created_by_user_id,
                created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                series_id.to_string(),
                site_id.to_string(),
                category_id.to_string(),
                next_home_position,
                owner_user_id.to_string(),
                now.to_rfc3339(),
            ],
        )
        .map_err(map_series_constraint_error)?;
    backfill_series_items_in_transaction(transaction, site_id, series_id, category_id, now)?;
    load_series_by_id(transaction, site_id, series_id)
}

fn backfill_series_items_in_transaction(
    transaction: &Transaction<'_>,
    site_id: Uuid,
    series_id: Uuid,
    category_id: Uuid,
    added_at: DateTime<Utc>,
) -> Result<(), RepositoryError> {
    let document_ids = {
        let mut statement = transaction
            .prepare(
                "SELECT document.id
                 FROM documents document
                 JOIN revision_categories placement
                   ON placement.revision_id = document.published_revision_id
                  AND placement.document_id = document.id
                  AND placement.site_id = document.site_id
                 WHERE document.site_id = ?1 AND placement.category_id = ?2
                   AND document.published_revision_id IS NOT NULL
                   AND document.status != 'archived'
                 ORDER BY document.created_at ASC, document.id ASC
                 LIMIT 501",
            )
            .map_err(storage_error)?;
        statement
            .query_map(
                params![site_id.to_string(), category_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?
    };
    if document_ids.len() > 500 {
        return Err(RepositoryError::Validation(
            "a series cannot contain more than 500 published documents".into(),
        ));
    }
    for (index, document_id) in document_ids.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO series_items (
                    series_id, site_id, document_id, position, added_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    series_id.to_string(),
                    site_id.to_string(),
                    document_id,
                    series_position(index)?,
                    added_at.to_rfc3339(),
                ],
            )
            .map_err(map_series_constraint_error)?;
    }
    Ok(())
}

fn series_position(index: usize) -> Result<i64, RepositoryError> {
    let ordinal = u64::try_from(index)
        .map_err(|error| RepositoryError::Validation(error.to_string()))?
        .checked_add(1)
        .and_then(|value| value.checked_mul(SERIES_POSITION_STEP))
        .ok_or_else(|| RepositoryError::Validation("series position overflow".into()))?;
    i64::try_from(ordinal).map_err(|error| RepositoryError::Validation(error.to_string()))
}

type StoredRevisionCategoryPlacementRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
);

fn load_revision_category_placement(
    connection: &Connection,
    site_id: Uuid,
    document_id: Uuid,
    revision_id: Uuid,
) -> Result<RevisionCategoryPlacement, RepositoryError> {
    let raw: Option<StoredRevisionCategoryPlacementRow> = connection
        .query_row(
            "SELECT revision_id, document_id, site_id, category_id,
                    assigned_by_user_id, assigned_at
             FROM revision_categories
             WHERE revision_id = ?1 AND document_id = ?2 AND site_id = ?3",
            params![
                revision_id.to_string(),
                document_id.to_string(),
                site_id.to_string()
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(storage_error)?;
    let (revision_id, document_id, site_id, category_id, assigned_by_user_id, assigned_at) =
        raw.ok_or(RepositoryError::NotFound)?;
    Ok(RevisionCategoryPlacement {
        revision_id: parse_uuid(&revision_id)?,
        document_id: parse_uuid(&document_id)?,
        site_id: parse_uuid(&site_id)?,
        category_id: category_id.map(|id| parse_uuid(&id)).transpose()?,
        assigned_by_user_id: assigned_by_user_id.map(|id| parse_uuid(&id)).transpose()?,
        assigned_at: parse_datetime(&assigned_at)?,
    })
}

fn load_document_category(
    connection: &Connection,
    site_id: Uuid,
    document_id: Uuid,
    selector: RevisionSelector,
) -> Result<Option<CategoryRecord>, RepositoryError> {
    let revision_column = match selector {
        RevisionSelector::Current => "document.current_revision_id",
        RevisionSelector::Published => "document.published_revision_id",
    };
    let sql = format!(
        "SELECT placement.category_id
         FROM documents document
         JOIN revision_categories placement
           ON placement.revision_id = {revision_column}
          AND placement.document_id = document.id
          AND placement.site_id = document.site_id
         WHERE document.id = ?1 AND document.site_id = ?2
           AND {revision_column} IS NOT NULL"
    );
    let category_id: Option<String> = connection
        .query_row(
            &sql,
            params![document_id.to_string(), site_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    category_id
        .map(|id| load_category_by_id(connection, site_id, parse_uuid(&id)?))
        .transpose()
}

type StoredSiteMembershipRow = (String, String, String, String);

fn stored_site_membership_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredSiteMembershipRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn parse_site_membership_row(
    raw: StoredSiteMembershipRow,
) -> Result<SiteMembershipRecord, RepositoryError> {
    let (site_id, user_id, role, created_at) = raw;
    Ok(SiteMembershipRecord {
        site_id: parse_uuid(&site_id)?,
        user_id: parse_uuid(&user_id)?,
        role: SiteMembershipRole::from_str(&role)?,
        created_at: parse_datetime(&created_at)?,
    })
}

fn load_site_membership(
    connection: &Connection,
    site_id: Uuid,
    user_id: Uuid,
) -> Result<SiteMembershipRecord, RepositoryError> {
    let raw = connection
        .query_row(
            "SELECT site_id, user_id, role, created_at
             FROM site_memberships WHERE site_id = ?1 AND user_id = ?2",
            params![site_id.to_string(), user_id.to_string()],
            stored_site_membership_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    parse_site_membership_row(raw)
}

type StoredCommentRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

fn stored_comment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCommentRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn parse_comment_row(raw: StoredCommentRow) -> Result<CommentRecord, RepositoryError> {
    let (id, site_id, document_id, author_user_id, source_markdown, status, created_at, updated_at) =
        raw;
    let status = match status.as_str() {
        "pending" => CommentStatus::Pending,
        "approved" => CommentStatus::Approved,
        other => {
            return Err(RepositoryError::Storage(format!(
                "unknown comment status {other}"
            )));
        }
    };
    Ok(CommentRecord {
        id: parse_uuid(&id)?,
        site_id: parse_uuid(&site_id)?,
        document_id: parse_uuid(&document_id)?,
        author_user_id: parse_uuid(&author_user_id)?,
        source_markdown,
        status,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    })
}

fn load_comment_by_id(
    connection: &Connection,
    id: Uuid,
    approved_only: bool,
) -> Result<CommentRecord, RepositoryError> {
    let raw = connection
        .query_row(
            "SELECT id, site_id, document_id, author_user_id, source_markdown,
                    status, created_at, updated_at
             FROM comments
             WHERE id = ?1 AND (?2 = 0 OR status = 'approved')",
            params![id.to_string(), approved_only],
            stored_comment_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    parse_comment_row(raw)
}

fn append_site_appearance_revision(
    transaction: &Transaction<'_>,
    owner_user_id: Uuid,
    site_id: Uuid,
    theme_profile: ThemeProfile,
    custom_css: Option<String>,
) -> Result<(), RepositoryError> {
    let next_revision: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(revision), 0) + 1
             FROM site_theme_revisions WHERE site_id = ?1",
            params![site_id.to_string()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    let now = Utc::now();
    transaction
        .execute(
            "INSERT INTO site_theme_revisions (
                site_id, revision, profile, custom_css, created_by_user_id, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                site_id.to_string(),
                next_revision,
                theme_profile.as_str(),
                custom_css,
                owner_user_id.to_string(),
                now.to_rfc3339(),
            ],
        )
        .map_err(map_community_constraint_error)?;
    transaction
        .execute(
            "UPDATE sites SET current_theme_revision = ?1, updated_at = ?2
             WHERE id = ?3",
            params![next_revision, now.to_rfc3339(), site_id.to_string()],
        )
        .map_err(storage_error)?;
    Ok(())
}

fn owns_site_in_connection(
    connection: &Connection,
    user_id: Uuid,
    site_id: Uuid,
) -> Result<bool, RepositoryError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM site_memberships
             WHERE site_id = ?1 AND user_id = ?2 AND role = 'owner'",
            params![site_id.to_string(), user_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some())
}

fn ensure_site_owner(
    connection: &Connection,
    user_id: Uuid,
    site_id: Uuid,
) -> Result<(), RepositoryError> {
    if owns_site_in_connection(connection, user_id, site_id)? {
        Ok(())
    } else {
        // Scope misses intentionally look identical to absent resources.
        Err(RepositoryError::NotFound)
    }
}

fn ensure_site_writer(
    connection: &Connection,
    user_id: Uuid,
    site_id: Uuid,
) -> Result<SiteMembershipRole, RepositoryError> {
    let membership = load_site_membership(connection, site_id, user_id)?;
    if membership.role.can_write() {
        Ok(membership.role)
    } else {
        // Scope misses intentionally look identical to absent resources.
        Err(RepositoryError::NotFound)
    }
}

fn ensure_user_exists(connection: &Connection, user_id: Uuid) -> Result<(), RepositoryError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM users WHERE id = ?1",
            params![user_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(RepositoryError::NotFound)
    }
}

fn ensure_site_exists(connection: &Connection, site_id: Uuid) -> Result<(), RepositoryError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM sites WHERE id = ?1",
            params![site_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(RepositoryError::NotFound)
    }
}

fn ensure_category_in_site(
    connection: &Connection,
    site_id: Uuid,
    category_id: Uuid,
) -> Result<(), RepositoryError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM categories WHERE id = ?1 AND site_id = ?2",
            params![category_id.to_string(), site_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        // Tenant-scope misses intentionally look identical to absent records.
        Err(RepositoryError::NotFound)
    }
}

fn ensure_document_in_site(
    connection: &Connection,
    site_id: Uuid,
    document_id: Uuid,
) -> Result<(), RepositoryError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM documents WHERE id = ?1 AND site_id = ?2",
            params![document_id.to_string(), site_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(RepositoryError::NotFound)
    }
}

fn ensure_published_document_in_site(
    connection: &Connection,
    site_id: Uuid,
    document_id: Uuid,
) -> Result<(), RepositoryError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM documents
             WHERE id = ?1 AND site_id = ?2
               AND published_revision_id IS NOT NULL AND status != 'archived'",
            params![document_id.to_string(), site_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(RepositoryError::NotFound)
    }
}

fn offline_import_document(site_id: Uuid, source: &str, post: &OfflineImportPost) -> NewDocument {
    NewDocument {
        site_id,
        title: post.title.clone(),
        slug: post.slug.clone(),
        source_markdown: post.source_markdown.clone(),
        embeds: Vec::new(),
        intent: None,
        ontology: None,
        ai_summary: None,
        authorship: PublicAuthorship {
            kind: PublicAuthorshipKind::Imported,
            generator: Some(source.to_owned()),
            human_reviewed: post.human_reviewed,
        },
        actor: RevisionActor {
            kind: RevisionActorKind::Importer,
            id: post.author_id.clone(),
            display_name: Some(post.author_display_name.clone()),
        },
    }
}

fn offline_import_idempotency_key(
    site_id: Uuid,
    source: &str,
    source_id: &str,
) -> Result<String, RepositoryError> {
    let key = format!(
        "offline-import:{site_id}:{}:{source}:{source_id}",
        source.len()
    );
    if key.len() > 200 {
        return Err(RepositoryError::Validation(format!(
            "offline import sourceId '{source_id}' produces an idempotency key longer than 200 bytes"
        )));
    }
    Ok(key)
}

fn validate_offline_import_batch(
    site_id: Uuid,
    batch: &OfflineImportBatch,
    reserved_route_roots: &[&str],
) -> Result<(), RepositoryError> {
    for root in reserved_route_roots {
        validate_offline_import_reserved_root(root)?;
    }
    validate_offline_import_text(&batch.source, "offline import source", 100)?;
    validate_offline_import_text(
        &batch.owner_display_name,
        "offline import owner display name",
        200,
    )?;
    if batch.categories.len() > 500 {
        return Err(RepositoryError::Validation(
            "offline import cannot contain more than 500 category declarations".into(),
        ));
    }
    if batch.posts.is_empty() || batch.posts.len() > 5_000 {
        return Err(RepositoryError::Validation(
            "offline import must contain 1-5000 posts".into(),
        ));
    }

    let mut category_slugs = BTreeSet::new();
    for category in &batch.categories {
        let slug = normalize_new_category_slug(&category.slug)?;
        ensure_offline_import_root_available(&slug, reserved_route_roots)?;
        if !category_slugs.insert(slug.clone()) {
            return Err(RepositoryError::Validation(format!(
                "offline import category '{slug}' is declared more than once"
            )));
        }
        validate_required_text(&category.title, "category title", 200)?;
        validate_optional_text(
            category.description.as_deref(),
            "category description",
            2_000,
        )?;
    }

    let mut source_ids = BTreeSet::new();
    let mut claimed_routes = BTreeMap::<String, String>::new();
    for post in &batch.posts {
        validate_offline_import_text(&post.source_id, "offline import sourceId", 100)?;
        validate_offline_import_text(&post.author_id, "offline import author id", 200)?;
        validate_offline_import_text(
            &post.author_display_name,
            "offline import author display name",
            200,
        )?;
        offline_import_idempotency_key(site_id, &batch.source, &post.source_id)?;
        if !source_ids.insert(post.source_id.clone()) {
            return Err(RepositoryError::Validation(format!(
                "offline import sourceId '{}' is duplicated",
                post.source_id
            )));
        }
        if post.aliases.len() > 1_000 {
            return Err(RepositoryError::Validation(format!(
                "offline import sourceId '{}' has more than 1000 aliases",
                post.source_id
            )));
        }

        let document = offline_import_document(site_id, &batch.source, post);
        document.validate().map_err(|error| {
            RepositoryError::Validation(format!(
                "offline import sourceId '{}' is invalid: {error}",
                post.source_id
            ))
        })?;
        if post.slug != post.slug.trim() {
            return Err(RepositoryError::Validation(format!(
                "offline import sourceId '{}' has a slug with surrounding whitespace",
                post.source_id
            )));
        }
        let category_slug = normalize_new_category_slug(&post.primary_category)?;
        ensure_offline_import_root_available(&category_slug, reserved_route_roots)?;
        let canonical_path = format!("{category_slug}/{}", post.slug);
        claim_offline_import_route(&mut claimed_routes, &canonical_path, &post.source_id)?;
        for alias in &post.aliases {
            validate_offline_import_alias_path(&alias.path, reserved_route_roots)?;
            if alias.path == canonical_path {
                return Err(RepositoryError::Validation(format!(
                    "offline import sourceId '{}' repeats its canonical path as an alias",
                    post.source_id
                )));
            }
            claim_offline_import_route(&mut claimed_routes, &alias.path, &post.source_id)?;
        }
    }
    Ok(())
}

fn validate_offline_import_reserved_root(root: &str) -> Result<(), RepositoryError> {
    if root.is_empty()
        || root.len() > 720
        || matches!(root, "." | "..")
        || root.contains(['/', '\\'])
        || root.chars().any(char::is_control)
    {
        return Err(RepositoryError::Validation(format!(
            "offline import reserved route root '{root}' is not a safe public path segment"
        )));
    }
    Ok(())
}

fn ensure_offline_import_root_available(
    root: &str,
    reserved_route_roots: &[&str],
) -> Result<(), RepositoryError> {
    if reserved_route_roots.contains(&root) {
        return Err(RepositoryError::Validation(format!(
            "offline import route root '{root}' overlaps the configured article base path"
        )));
    }
    Ok(())
}

fn validate_offline_import_text(
    value: &str,
    field: &str,
    max_chars: usize,
) -> Result<(), RepositoryError> {
    let length = value.chars().count();
    if !(1..=max_chars).contains(&length)
        || value != value.trim()
        || value.chars().any(char::is_control)
    {
        return Err(RepositoryError::Validation(format!(
            "{field} must contain 1-{max_chars} printable characters without surrounding whitespace"
        )));
    }
    Ok(())
}

fn claim_offline_import_route(
    claimed: &mut BTreeMap<String, String>,
    path: &str,
    source_id: &str,
) -> Result<(), RepositoryError> {
    if let Some(existing) = claimed.insert(path.to_owned(), source_id.to_owned()) {
        return Err(RepositoryError::Validation(format!(
            "offline import route '{path}' is claimed by sourceIds '{existing}' and '{source_id}'"
        )));
    }
    Ok(())
}

fn validate_offline_import_alias_path(
    path: &str,
    reserved_route_roots: &[&str],
) -> Result<(), RepositoryError> {
    let segments = path.split('/').collect::<Vec<_>>();
    let unsafe_segment = segments.iter().any(|segment| {
        segment.is_empty()
            || *segment == "."
            || *segment == ".."
            || segment.len() > 720
            || segment.contains('\\')
            || segment.chars().any(char::is_control)
    });
    if path.len() > 2_048
        || segments.len() > 32
        || unsafe_segment
        || path.starts_with('/')
        || path.ends_with('/')
    {
        return Err(RepositoryError::Validation(format!(
            "offline import alias '{path}' is not a safe relative public path"
        )));
    }
    const RESERVED_ROOTS: &[&str] = &[
        ".well-known",
        "AI2AI.md",
        "UNLICENSE",
        "agent.txt",
        "agents.txt",
        "api",
        "assets",
        "blog",
        "custom.css",
        "docs",
        "favicon.svg",
        "healthz",
        "index.html",
        "livez",
        "llms.txt",
        "login",
        "media",
        "onboarding",
        "openapi",
        "providers",
        "references",
        "readyz",
        "robots.txt",
        "schemas",
        "sitemap.xml",
        "studio",
        "vendor",
    ];
    if RESERVED_ROOTS.contains(&segments[0])
        || reserved_route_roots.contains(&segments[0])
        || segments[0].starts_with('@')
    {
        return Err(RepositoryError::Validation(format!(
            "offline import alias '{path}' overlaps an application route"
        )));
    }
    Ok(())
}

fn ensure_offline_import_retry_matches(
    connection: &Connection,
    site_id: Uuid,
    source: &str,
    post: &OfflineImportPost,
    category: &CategoryRecord,
    document_id: Uuid,
    revision_id: Uuid,
) -> Result<(), RepositoryError> {
    let document = load_document(connection, document_id, RevisionSelector::Current)?;
    let expected = offline_import_document(site_id, source, post);
    let placement =
        load_revision_category_placement(connection, site_id, document_id, revision_id)?;
    let expected_canonical = category_route_path(Some(category), &post.slug);
    let expected_routes = std::iter::once((expected_canonical, (true, post.created_at)))
        .chain(
            post.aliases
                .iter()
                .map(|alias| (alias.path.clone(), (false, alias.created_at))),
        )
        .collect::<BTreeMap<_, _>>();
    let mut statement = connection
        .prepare(
            "SELECT path, is_canonical, created_at
             FROM routes WHERE site_id = ?1 AND document_id = ?2",
        )
        .map_err(storage_error)?;
    let actual_routes = statement
        .query_map(
            params![site_id.to_string(), document_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(storage_error)?
        .map(|row| {
            let (path, canonical, created_at) = row.map_err(storage_error)?;
            Ok((path, (canonical, parse_datetime(&created_at)?)))
        })
        .collect::<Result<BTreeMap<_, _>, RepositoryError>>()?;
    let exact = document.site_id == site_id
        && document.status == DocumentStatus::Published
        && document.current_revision_id == revision_id
        && document.published_revision_id == Some(revision_id)
        && document.created_at == post.created_at
        && document.updated_at == post.created_at
        && document.revision.id == revision_id
        && document.revision.revision_number == 1
        && document.revision.parent_revision_id.is_none()
        && document.revision.title == expected.title
        && document.revision.slug == expected.slug
        && document.revision.source_markdown == expected.source_markdown
        && document.revision.embeds.is_empty()
        && document.revision.intent.is_none()
        && document.revision.ontology.is_none()
        && document.revision.ai_summary.is_none()
        && document.revision.authorship == expected.authorship
        && document.revision.actor == expected.actor
        && document.revision.created_at == post.created_at
        && placement.category_id == Some(category.id)
        && placement.assigned_at == post.created_at
        && actual_routes == expected_routes;
    if !exact {
        return Err(RepositoryError::Validation(format!(
            "offline import sourceId '{}' conflicts with its previously imported record",
            post.source_id
        )));
    }
    Ok(())
}

fn normalize_category_slug(value: &str) -> Result<String, RepositoryError> {
    normalize_handle(value, "category slug")
}

fn normalize_new_category_slug(value: &str) -> Result<String, RepositoryError> {
    let slug = normalize_category_slug(value)?;
    const RESERVED: &[&str] = &[
        "api",
        "assets",
        "blog",
        "docs",
        "healthz",
        "livez",
        "login",
        "media",
        "onboarding",
        "openapi",
        "providers",
        "references",
        "readyz",
        "schemas",
        "studio",
        "vendor",
    ];
    if RESERVED.contains(&slug.as_str()) {
        return Err(RepositoryError::Validation(format!(
            "category slug '{slug}' is reserved by the application"
        )));
    }
    Ok(slug)
}

fn category_route_path(category: Option<&CategoryRecord>, revision_slug: &str) -> String {
    category
        .map(|category| format!("{}/{revision_slug}", category.slug))
        .unwrap_or_else(|| revision_slug.to_owned())
}

fn revision_category_route_path(
    connection: &Connection,
    site_id: Uuid,
    document_id: Uuid,
    revision_id: Uuid,
    revision_slug: &str,
    require_active: bool,
) -> Result<String, RepositoryError> {
    let placement =
        load_revision_category_placement(connection, site_id, document_id, revision_id)?;
    let category = placement
        .category_id
        .map(|id| load_category_by_id(connection, site_id, id))
        .transpose()?;
    if require_active
        && category
            .as_ref()
            .is_some_and(|category| category.status != CategoryStatus::Active)
    {
        return Err(RepositoryError::Validation(
            "archived categories cannot receive publications".into(),
        ));
    }
    if category.is_none() {
        ensure_root_slug_not_category(connection, site_id, revision_slug)?;
    }
    Ok(category_route_path(category.as_ref(), revision_slug))
}

fn ensure_category_landing_available(
    connection: &Connection,
    site_id: Uuid,
    category_slug: &str,
) -> Result<(), RepositoryError> {
    let occupied = connection
        .query_row(
            "SELECT 1
             WHERE EXISTS (
               SELECT 1 FROM documents WHERE site_id = ?1 AND current_slug = ?2
             ) OR EXISTS (
               SELECT 1 FROM routes WHERE site_id = ?1 AND path = ?2
             )",
            params![site_id.to_string(), category_slug],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some();
    if occupied {
        Err(RepositoryError::DuplicateSlug)
    } else {
        Ok(())
    }
}

fn ensure_root_slug_not_category(
    connection: &Connection,
    site_id: Uuid,
    revision_slug: &str,
) -> Result<(), RepositoryError> {
    let occupied = connection
        .query_row(
            "SELECT 1 FROM categories WHERE site_id = ?1 AND slug = ?2 COLLATE NOCASE",
            params![site_id.to_string(), revision_slug],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some();
    if occupied {
        Err(RepositoryError::DuplicateSlug)
    } else {
        Ok(())
    }
}

fn ensure_document_route_available(
    connection: &Connection,
    site_id: Uuid,
    document_id: Uuid,
    route_path: &str,
) -> Result<(), RepositoryError> {
    let owner: Option<String> = connection
        .query_row(
            "SELECT id FROM documents
             WHERE site_id = ?1 AND current_slug = ?2 AND id != ?3
             UNION ALL
             SELECT document_id FROM routes
             WHERE site_id = ?1 AND path = ?2 AND document_id != ?3
             LIMIT 1",
            params![site_id.to_string(), route_path, document_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    if owner.is_some() {
        Err(RepositoryError::DuplicateSlug)
    } else {
        Ok(())
    }
}

fn normalize_handle(value: &str, field: &str) -> Result<String, RepositoryError> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.len() > 40
        || !normalized.is_ascii()
        || normalized.starts_with('-')
        || normalized.ends_with('-')
        || normalized
            .bytes()
            .any(|byte| !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && byte != b'-')
    {
        return Err(RepositoryError::Validation(format!(
            "{field} must contain 1-40 lowercase ASCII letters, digits, or interior hyphens"
        )));
    }
    Ok(normalized)
}

fn normalize_email(value: &str) -> Result<String, RepositoryError> {
    let normalized = value.trim().to_ascii_lowercase();
    let mut parts = normalized.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if normalized.len() > 320
        || !normalized.is_ascii()
        || local.is_empty()
        || domain.is_empty()
        || parts.next().is_some()
        || normalized
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(RepositoryError::Validation(
            "email must be a valid ASCII address".into(),
        ));
    }
    Ok(normalized)
}

fn validate_required_text(
    value: &str,
    field: &str,
    max_chars: usize,
) -> Result<String, RepositoryError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max_chars || value.contains('\0') {
        return Err(RepositoryError::Validation(format!(
            "{field} must contain 1-{max_chars} characters"
        )));
    }
    Ok(value.to_owned())
}

fn validate_optional_text(
    value: Option<&str>,
    field: &str,
    max_chars: usize,
) -> Result<Option<String>, RepositoryError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.chars().count() > max_chars || value.contains('\0') {
                Err(RepositoryError::Validation(format!(
                    "{field} must not exceed {max_chars} characters"
                )))
            } else {
                Ok(value.to_owned())
            }
        })
        .transpose()
}

fn validate_password_phc(value: &str) -> Result<(), RepositoryError> {
    if !value.starts_with("$argon2id$")
        || value.len() > 1_024
        || value.contains('\0')
        || value.contains(['\r', '\n'])
    {
        return Err(RepositoryError::Validation(
            "password must be stored as a bounded Argon2id PHC string".into(),
        ));
    }
    Ok(())
}

fn validate_token_hash(value: &[u8]) -> Result<(), RepositoryError> {
    if value.len() != 32 {
        return Err(RepositoryError::Validation(
            "session token hash must be exactly 32 bytes".into(),
        ));
    }
    Ok(())
}

fn validate_session_expiry(expires_at: DateTime<Utc>) -> Result<(), RepositoryError> {
    if expires_at <= Utc::now() {
        Err(RepositoryError::Validation(
            "session expiry must be in the future".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_fingerprint(value: &[u8]) -> Result<(), RepositoryError> {
    if value.len() != 32 {
        Err(RepositoryError::Validation(
            "admin binding fingerprint must be exactly 32 bytes".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_subject_hash(value: &[u8]) -> Result<(), RepositoryError> {
    if value.len() != 32 {
        Err(RepositoryError::Validation(
            "external subject hash must be exactly 32 bytes".into(),
        ))
    } else {
        Ok(())
    }
}

fn fixed_hash(value: &[u8], label: &str) -> Result<[u8; 32], RepositoryError> {
    value
        .try_into()
        .map_err(|_| RepositoryError::Storage(format!("{label} is not 32 bytes")))
}

fn validate_external_adapter(value: &str) -> Result<String, RepositoryError> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.len() > 64
        || !normalized
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    {
        return Err(RepositoryError::Validation(
            "external adapter must contain 1-64 lowercase ASCII letters, digits, _, or -".into(),
        ));
    }
    Ok(normalized)
}

fn validate_external_issuer(value: &str) -> Result<String, RepositoryError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 2_048
        || value.contains('\0')
        || value.contains(['\r', '\n'])
    {
        return Err(RepositoryError::Validation(
            "external issuer must contain 1-2048 bounded characters".into(),
        ));
    }
    Ok(value.to_owned())
}

fn validate_comment_markdown(value: &str) -> Result<String, RepositoryError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 64 * 1_024 || value.contains('\0') {
        return Err(RepositoryError::Validation(
            "comment Markdown must contain 1-65536 bytes".into(),
        ));
    }
    Ok(value.to_owned())
}

/// Validates owner-supplied, site-scoped first-party CSS. The server wraps this
/// text in an exact-root `@scope` and serves it from a same-origin stylesheet,
/// so HTML delimiters, CSS escapes, malformed blocks, and every network loading
/// primitive are rejected. This deliberately conservative subset still
/// supports normal selectors, declarations, variables, and pseudo classes.
fn validate_custom_css(value: Option<&str>) -> Result<Option<String>, RepositoryError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 64 * 1_024 {
        return Err(RepositoryError::Validation(
            "custom CSS must not exceed 65536 bytes".into(),
        ));
    }
    if value.contains('\0')
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(RepositoryError::Validation(
            "custom CSS contains unsupported control characters".into(),
        ));
    }
    let lower = value.to_ascii_lowercase();
    let unsafe_fragment = value.contains(['<', '>', '\\'])
        || lower.contains('@')
        || lower.contains("http:")
        || lower.contains("https:")
        || lower.contains("data:")
        || lower.contains("javascript:")
        || lower.contains("//")
        || lower.contains("expression(")
        || lower.contains("behavior:")
        || lower.contains("-moz-binding")
        || contains_css_function(&lower, "url")
        || contains_css_function(&lower, "image-set")
        || contains_css_function(&lower, "src");
    if unsafe_fragment {
        return Err(RepositoryError::Validation(
            "custom CSS must not contain HTML, at-rules, escapes, or external resource loaders"
                .into(),
        ));
    }
    validate_css_block_structure(value)?;
    Ok(Some(value.to_owned()))
}

fn validate_css_block_structure(value: &str) -> Result<(), RepositoryError> {
    #[derive(Clone, Copy)]
    enum ScanState {
        Normal,
        SingleQuoted,
        DoubleQuoted,
        Comment,
    }

    let bytes = value.as_bytes();
    let mut state = ScanState::Normal;
    let mut depth = 0_u32;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        match state {
            ScanState::Normal => match (byte, next) {
                (b'/', Some(b'*')) => {
                    state = ScanState::Comment;
                    cursor += 1;
                }
                (b'\'', _) => state = ScanState::SingleQuoted,
                (b'"', _) => state = ScanState::DoubleQuoted,
                (b'{', _) => depth = depth.saturating_add(1),
                (b'}', _) if depth == 0 => {
                    return Err(RepositoryError::Validation(
                        "custom CSS contains an unmatched closing brace".into(),
                    ));
                }
                (b'}', _) => depth -= 1,
                _ => {}
            },
            ScanState::SingleQuoted if byte == b'\'' => state = ScanState::Normal,
            ScanState::DoubleQuoted if byte == b'"' => state = ScanState::Normal,
            ScanState::Comment if (byte, next) == (b'*', Some(b'/')) => {
                state = ScanState::Normal;
                cursor += 1;
            }
            ScanState::SingleQuoted | ScanState::DoubleQuoted | ScanState::Comment => {}
        }
        cursor += 1;
    }

    if !matches!(state, ScanState::Normal) {
        return Err(RepositoryError::Validation(
            "custom CSS contains an unterminated string or comment".into(),
        ));
    }
    if depth != 0 {
        return Err(RepositoryError::Validation(
            "custom CSS contains an unclosed block".into(),
        ));
    }
    Ok(())
}

fn contains_css_function(value: &str, name: &str) -> bool {
    let bytes = value.as_bytes();
    let name = name.as_bytes();
    let mut offset = 0;
    while let Some(found) = value[offset..].find(std::str::from_utf8(name).unwrap_or_default()) {
        let start = offset + found;
        let mut cursor = start + name.len();
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'(') {
            return true;
        }
        offset = start + name.len();
        if offset >= value.len() {
            break;
        }
    }
    false
}

fn parse_uuid(value: &str) -> Result<Uuid, RepositoryError> {
    Uuid::parse_str(value).map_err(storage_error)
}

fn page_parameter(value: usize) -> Result<i64, RepositoryError> {
    i64::try_from(value)
        .map_err(|_| RepositoryError::Validation("pagination offset or limit is too large".into()))
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>, RepositoryError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(storage_error)
}

fn parse_status(value: &str) -> Result<DocumentStatus, RepositoryError> {
    match value {
        "draft" => Ok(DocumentStatus::Draft),
        "published" => Ok(DocumentStatus::Published),
        "archived" => Ok(DocumentStatus::Archived),
        other => Err(RepositoryError::Storage(format!(
            "unknown document status {other}"
        ))),
    }
}

fn ensure_document_exists(
    connection: &Connection,
    document_id: Uuid,
) -> Result<(), RepositoryError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM documents WHERE id = ?1",
            params![document_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(storage_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(RepositoryError::NotFound)
    }
}

fn map_constraint_error(error: rusqlite::Error) -> RepositoryError {
    let text = error.to_string();
    if text.contains("UNIQUE constraint failed") {
        RepositoryError::DuplicateSlug
    } else {
        storage_error(error)
    }
}

fn map_category_constraint_error(error: rusqlite::Error) -> RepositoryError {
    let text = error.to_string();
    if text.contains("categories.site_id, categories.slug")
        || text.contains("documents.site_id, documents.current_slug")
    {
        RepositoryError::DuplicateSlug
    } else if text.contains("FOREIGN KEY constraint failed") {
        RepositoryError::NotFound
    } else if text.contains("CHECK constraint failed") {
        RepositoryError::Validation("category record violates a storage constraint".into())
    } else {
        storage_error(error)
    }
}

fn map_series_constraint_error(error: rusqlite::Error) -> RepositoryError {
    let text = error.to_string();
    if text.contains("series.site_id, series.category_id") {
        RepositoryError::Validation("the category is already a series".into())
    } else if text.contains("FOREIGN KEY constraint failed") {
        RepositoryError::NotFound
    } else if text.contains("CHECK constraint failed") {
        RepositoryError::Validation("series record violates a storage constraint".into())
    } else {
        storage_error(error)
    }
}

fn map_community_constraint_error(error: rusqlite::Error) -> RepositoryError {
    let text = error.to_string();
    if text.contains("users.email") {
        RepositoryError::Validation("email is already registered".into())
    } else if text.contains("users.handle") {
        RepositoryError::Validation("user handle is already registered".into())
    } else if text.contains("sites.handle") {
        RepositoryError::Validation("site handle is already registered".into())
    } else if text.contains("sessions.token_hash") {
        RepositoryError::Validation("session credential is already registered".into())
    } else if text.contains("FOREIGN KEY constraint failed") {
        RepositoryError::NotFound
    } else if text.contains("CHECK constraint failed") || text.contains("UNIQUE constraint failed")
    {
        RepositoryError::Validation("community record violates a storage constraint".into())
    } else {
        storage_error(error)
    }
}

fn map_ai_proposal_constraint_error(error: rusqlite::Error) -> RepositoryError {
    let text = error.to_string();
    if text.contains("ai_proposal_audits.message_id")
        || text.contains("ai_proposal_audits.idempotency_key")
    {
        RepositoryError::DuplicateIdempotencyKey
    } else {
        storage_error(error)
    }
}

fn storage_error(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Storage(error.to_string())
}

const MIGRATION_1: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS schema_migrations (
  version INTEGER PRIMARY KEY,
  applied_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS documents (
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
  current_revision_id TEXT NOT NULL,
  published_revision_id TEXT,
  current_slug TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (site_id, current_slug),
  FOREIGN KEY (current_revision_id) REFERENCES revisions(id) DEFERRABLE INITIALLY DEFERRED,
  FOREIGN KEY (published_revision_id) REFERENCES revisions(id) DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE IF NOT EXISTS revisions (
  id TEXT PRIMARY KEY,
  document_id TEXT NOT NULL,
  revision_number INTEGER NOT NULL,
  parent_revision_id TEXT,
  slug TEXT NOT NULL,
  snapshot_json TEXT NOT NULL,
  idempotency_key TEXT UNIQUE,
  created_at TEXT NOT NULL,
  UNIQUE (document_id, revision_number),
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
  FOREIGN KEY (parent_revision_id) REFERENCES revisions(id)
);

CREATE TABLE IF NOT EXISTS routes (
  site_id TEXT NOT NULL,
  path TEXT NOT NULL,
  document_id TEXT NOT NULL,
  is_canonical INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  PRIMARY KEY (site_id, path),
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS revisions_document_idx
  ON revisions(document_id, revision_number DESC);
CREATE INDEX IF NOT EXISTS documents_published_idx
  ON documents(site_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS routes_document_idx
  ON routes(document_id, is_canonical);

INSERT OR IGNORE INTO schema_migrations(version, applied_at)
VALUES (1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

const MIGRATION_2: &str = r#"
CREATE UNIQUE INDEX IF NOT EXISTS revisions_id_document_idx
  ON revisions(id, document_id);

CREATE TABLE IF NOT EXISTS ai_proposal_audits (
  schema_version TEXT NOT NULL,
  accepted_revision_id TEXT PRIMARY KEY,
  document_id TEXT NOT NULL,
  message_id TEXT NOT NULL UNIQUE,
  idempotency_key TEXT NOT NULL UNIQUE,
  received_at TEXT NOT NULL,
  envelope_json TEXT NOT NULL,
  FOREIGN KEY (accepted_revision_id, document_id)
    REFERENCES revisions(id, document_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS ai_proposal_audits_document_idx
  ON ai_proposal_audits(document_id, received_at DESC, accepted_revision_id DESC);

INSERT OR IGNORE INTO schema_migrations(version, applied_at)
VALUES (2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

const MIGRATION_3: &str = r#"
CREATE TABLE IF NOT EXISTS users (
  id TEXT PRIMARY KEY,
  email TEXT COLLATE NOCASE NOT NULL UNIQUE,
  handle TEXT COLLATE NOCASE NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  password_phc TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  CHECK (email = lower(email)),
  CHECK (handle = lower(handle)),
  CHECK (length(handle) BETWEEN 1 AND 40),
  CHECK (handle NOT GLOB '*[^a-z0-9-]*'),
  CHECK (password_phc LIKE '$argon2id$%')
);

CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
  user_id TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  created_at TEXT NOT NULL,
  revoked_at TEXT,
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS sessions_user_expiry_idx
  ON sessions(user_id, expires_at DESC);
CREATE INDEX IF NOT EXISTS sessions_active_expiry_idx
  ON sessions(expires_at) WHERE revoked_at IS NULL;

CREATE TABLE IF NOT EXISTS sites (
  id TEXT PRIMARY KEY,
  handle TEXT COLLATE NOCASE NOT NULL UNIQUE,
  title TEXT NOT NULL,
  description TEXT,
  current_theme_revision INTEGER NOT NULL CHECK (current_theme_revision > 0),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  CHECK (handle = lower(handle)),
  CHECK (length(handle) BETWEEN 1 AND 40),
  CHECK (handle NOT GLOB '*[^a-z0-9-]*'),
  FOREIGN KEY (id, current_theme_revision)
    REFERENCES site_theme_revisions(site_id, revision)
    DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE IF NOT EXISTS site_memberships (
  site_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('owner')),
  created_at TEXT NOT NULL,
  PRIMARY KEY (site_id, user_id),
  FOREIGN KEY (site_id) REFERENCES sites(id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS site_single_owner_idx
  ON site_memberships(site_id) WHERE role = 'owner';
CREATE INDEX IF NOT EXISTS site_memberships_user_idx
  ON site_memberships(user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS site_theme_revisions (
  site_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK (revision > 0),
  profile TEXT NOT NULL CHECK (profile IN ('paper', 'ink', 'forest', 'terminal')),
  created_by_user_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (site_id, revision),
  FOREIGN KEY (site_id) REFERENCES sites(id) ON DELETE CASCADE,
  FOREIGN KEY (created_by_user_id) REFERENCES users(id)
);

CREATE TRIGGER IF NOT EXISTS site_theme_revisions_immutable_update
BEFORE UPDATE ON site_theme_revisions
BEGIN
  SELECT RAISE(ABORT, 'site theme revisions are immutable');
END;

CREATE TRIGGER IF NOT EXISTS site_theme_revisions_immutable_delete
BEFORE DELETE ON site_theme_revisions
BEGIN
  SELECT RAISE(ABORT, 'site theme revisions are immutable');
END;

CREATE UNIQUE INDEX IF NOT EXISTS documents_id_site_idx
  ON documents(id, site_id);

CREATE TABLE IF NOT EXISTS comments (
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL,
  document_id TEXT NOT NULL,
  author_user_id TEXT NOT NULL,
  source_markdown TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending', 'approved')),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (site_id) REFERENCES sites(id) ON DELETE CASCADE,
  FOREIGN KEY (document_id, site_id)
    REFERENCES documents(id, site_id) ON DELETE CASCADE,
  FOREIGN KEY (author_user_id) REFERENCES users(id)
);

CREATE INDEX IF NOT EXISTS comments_public_idx
  ON comments(site_id, document_id, status, created_at, id);
CREATE INDEX IF NOT EXISTS comments_author_idx
  ON comments(author_user_id, created_at DESC);

-- Repair the legacy state produced when a new draft revision changed the
-- document status despite retaining a published revision pointer.
UPDATE documents
SET status = 'published'
WHERE published_revision_id IS NOT NULL AND status = 'draft';

INSERT OR IGNORE INTO schema_migrations(version, applied_at)
VALUES (3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

const MIGRATION_4: &str = r#"
-- Give every pre-community site a public, non-loginable owner/profile so an
-- upgraded deployment's feed does not silently lose already-published legacy
-- documents. The synthetic email intentionally fails the server's login
-- validation (`@localhost` has no public domain suffix).
INSERT OR IGNORE INTO users (
  id, email, handle, display_name, password_phc, created_at, updated_at
)
SELECT
  d.site_id,
  'legacy-' || replace(d.site_id, '-', '') || '@localhost',
  'legacy-' || replace(d.site_id, '-', ''),
  'Legacy owner',
  '$argon2id$disabled-for-legacy-readonly-owner',
  MIN(d.created_at),
  MAX(d.updated_at)
FROM documents d
LEFT JOIN sites existing ON existing.id = d.site_id
WHERE existing.id IS NULL
GROUP BY d.site_id;

INSERT OR IGNORE INTO sites (
  id, handle, title, description, current_theme_revision, created_at, updated_at
)
SELECT
  d.site_id,
  'legacy-' || replace(d.site_id, '-', ''),
  'Legacy blog',
  'Content retained from the single-site deployment profile.',
  1,
  MIN(d.created_at),
  MAX(d.updated_at)
FROM documents d
LEFT JOIN sites existing ON existing.id = d.site_id
WHERE existing.id IS NULL
GROUP BY d.site_id;

INSERT OR IGNORE INTO site_memberships (site_id, user_id, role, created_at)
SELECT s.id, s.id, 'owner', s.created_at
FROM sites s
JOIN users synthetic_owner
  ON synthetic_owner.id = s.id AND synthetic_owner.email LIKE 'legacy-%@localhost'
JOIN documents d ON d.site_id = s.id
GROUP BY s.id;

INSERT OR IGNORE INTO site_theme_revisions (
  site_id, revision, profile, created_by_user_id, created_at
)
SELECT s.id, 1, 'paper', membership.user_id, s.created_at
FROM sites s
JOIN users synthetic_owner
  ON synthetic_owner.id = s.id AND synthetic_owner.email LIKE 'legacy-%@localhost'
JOIN site_memberships membership
  ON membership.site_id = s.id AND membership.role = 'owner'
JOIN documents d ON d.site_id = s.id
GROUP BY s.id;

INSERT OR IGNORE INTO schema_migrations(version, applied_at)
VALUES (4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

const MIGRATION_5: &str = r#"
CREATE TABLE site_memberships_v5 (
  site_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('owner', 'editor', 'writer')),
  created_at TEXT NOT NULL,
  PRIMARY KEY (site_id, user_id),
  FOREIGN KEY (site_id) REFERENCES sites(id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

INSERT INTO site_memberships_v5 (site_id, user_id, role, created_at)
SELECT site_id, user_id, role, created_at FROM site_memberships;

DROP TABLE site_memberships;
ALTER TABLE site_memberships_v5 RENAME TO site_memberships;

CREATE UNIQUE INDEX site_single_owner_idx
  ON site_memberships(site_id) WHERE role = 'owner';
CREATE INDEX site_memberships_user_idx
  ON site_memberships(user_id, created_at DESC);

ALTER TABLE site_theme_revisions
  ADD COLUMN custom_css TEXT
  CHECK (custom_css IS NULL OR length(CAST(custom_css AS BLOB)) <= 65536);

INSERT INTO schema_migrations(version, applied_at)
VALUES (5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

const MIGRATION_6: &str = r#"
ALTER TABLE sessions
  ADD COLUMN auth_epoch INTEGER NOT NULL DEFAULT 0 CHECK (auth_epoch >= 0);
ALTER TABLE sessions
  ADD COLUMN auth_method TEXT NOT NULL DEFAULT 'legacy'
  CHECK (auth_method IN ('legacy', 'access_key', 'external'));

-- Sessions created before this migration predate the persisted authentication
-- binding. Fail closed instead of allowing an old local or bearer credential to
-- inherit the primary owner's new administration authority.
UPDATE sessions
SET revoked_at = COALESCE(revoked_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

CREATE INDEX sessions_admin_epoch_idx
  ON sessions(user_id, auth_epoch, auth_method, expires_at DESC)
  WHERE revoked_at IS NULL;

CREATE TABLE admin_control_plane (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  primary_site_id TEXT NOT NULL,
  owner_user_id TEXT NOT NULL,
  auth_mode TEXT NOT NULL
    CHECK (auth_mode IN ('access_key', 'external', 'disabled')),
  auth_epoch INTEGER NOT NULL CHECK (auth_epoch > 0),
  setup_complete INTEGER NOT NULL DEFAULT 1
    CHECK (setup_complete IN (0, 1)),
  binding_fingerprint BLOB NOT NULL CHECK (length(binding_fingerprint) = 32),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (primary_site_id) REFERENCES sites(id) ON DELETE RESTRICT,
  FOREIGN KEY (owner_user_id) REFERENCES users(id) ON DELETE RESTRICT
);

CREATE TABLE external_identities (
  adapter TEXT NOT NULL,
  issuer TEXT NOT NULL,
  subject_hash BLOB NOT NULL CHECK (length(subject_hash) = 32),
  user_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  last_seen_at TEXT NOT NULL,
  PRIMARY KEY (adapter, issuer, subject_hash),
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  CHECK (length(adapter) BETWEEN 1 AND 64),
  CHECK (length(issuer) BETWEEN 1 AND 2048)
);

CREATE INDEX external_identities_user_idx
  ON external_identities(user_id, adapter, issuer);

INSERT INTO schema_migrations(version, applied_at)
VALUES (6, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

const MIGRATION_7: &str = r#"
CREATE TABLE home_pins (
  slot INTEGER PRIMARY KEY CHECK (slot BETWEEN 1 AND 3),
  document_id TEXT NOT NULL UNIQUE,
  pinned_by_user_id TEXT NOT NULL,
  pinned_at TEXT NOT NULL,
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE,
  FOREIGN KEY (pinned_by_user_id) REFERENCES users(id) ON DELETE RESTRICT
);

INSERT INTO schema_migrations(version, applied_at)
VALUES (7, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

const MIGRATION_8: &str = r#"
CREATE TABLE categories (
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL,
  slug TEXT COLLATE NOCASE NOT NULL,
  title TEXT NOT NULL,
  description TEXT,
  theme_profile TEXT CHECK (
    theme_profile IS NULL OR theme_profile IN ('paper', 'ink', 'forest', 'terminal')
  ),
  status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'archived')),
  created_by_user_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (site_id, slug),
  UNIQUE (id, site_id),
  CHECK (slug = lower(slug)),
  CHECK (length(slug) BETWEEN 1 AND 40),
  CHECK (slug NOT GLOB '*[^a-z0-9-]*'),
  CHECK (substr(slug, 1, 1) != '-' AND substr(slug, -1, 1) != '-'),
  FOREIGN KEY (site_id) REFERENCES sites(id) ON DELETE CASCADE,
  FOREIGN KEY (created_by_user_id) REFERENCES users(id) ON DELETE RESTRICT
);

CREATE INDEX categories_site_status_idx
  ON categories(site_id, status, title, slug);

CREATE TRIGGER categories_slug_immutable
BEFORE UPDATE OF slug ON categories
WHEN NEW.slug != OLD.slug
BEGIN
  SELECT RAISE(ABORT, 'category slugs are immutable');
END;

CREATE TABLE revision_categories (
  revision_id TEXT PRIMARY KEY,
  document_id TEXT NOT NULL,
  site_id TEXT NOT NULL,
  category_id TEXT,
  assigned_by_user_id TEXT,
  assigned_at TEXT NOT NULL,
  FOREIGN KEY (revision_id, document_id)
    REFERENCES revisions(id, document_id) ON DELETE CASCADE,
  FOREIGN KEY (document_id, site_id)
    REFERENCES documents(id, site_id) ON DELETE CASCADE,
  FOREIGN KEY (category_id, site_id)
    REFERENCES categories(id, site_id) ON DELETE RESTRICT,
  FOREIGN KEY (assigned_by_user_id) REFERENCES users(id) ON DELETE SET NULL
);

CREATE INDEX revision_categories_category_idx
  ON revision_categories(site_id, category_id, revision_id);
CREATE INDEX revision_categories_document_idx
  ON revision_categories(document_id, revision_id);

-- Existing revisions are intentionally uncategorized. Their public paths stay
-- byte-for-byte compatible after upgrading a delivery database.
INSERT INTO revision_categories (
  revision_id, document_id, site_id, category_id, assigned_by_user_id, assigned_at
)
SELECT revision.id, revision.document_id, document.site_id, NULL, NULL, revision.created_at
FROM revisions revision
JOIN documents document ON document.id = revision.document_id;

-- A new draft inherits its parent's placement. The placement is still stored
-- independently on the new immutable revision and can be moved before publish.
CREATE TRIGGER revisions_default_category_placement
AFTER INSERT ON revisions
BEGIN
  INSERT INTO revision_categories (
    revision_id, document_id, site_id, category_id, assigned_by_user_id, assigned_at
  )
  SELECT NEW.id, NEW.document_id, document.site_id, parent.category_id, NULL, NEW.created_at
  FROM documents document
  LEFT JOIN revision_categories parent ON parent.revision_id = NEW.parent_revision_id
  WHERE document.id = NEW.document_id;
END;

-- Once a revision is the delivery pointer its placement is immutable. Studio
-- must append a new revision, which preserves published-vs-current separation.
CREATE TRIGGER published_revision_category_immutable
BEFORE UPDATE ON revision_categories
WHEN EXISTS (
  SELECT 1 FROM documents
  WHERE id = OLD.document_id AND published_revision_id = OLD.revision_id
)
BEGIN
  SELECT RAISE(ABORT, 'published revision category placement is immutable');
END;

INSERT INTO schema_migrations(version, applied_at)
VALUES (8, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

const MIGRATION_9: &str = r#"
CREATE TABLE series (
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL,
  category_id TEXT NOT NULL,
  home_position INTEGER NOT NULL CHECK (home_position > 0),
  created_by_user_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (id, site_id),
  UNIQUE (site_id, category_id),
  FOREIGN KEY (category_id, site_id)
    REFERENCES categories(id, site_id) ON DELETE RESTRICT,
  FOREIGN KEY (created_by_user_id) REFERENCES users(id) ON DELETE RESTRICT
);

CREATE INDEX series_site_home_idx
  ON series(site_id, home_position, id);

CREATE TABLE series_items (
  series_id TEXT NOT NULL,
  site_id TEXT NOT NULL,
  document_id TEXT NOT NULL,
  position INTEGER NOT NULL CHECK (position > 0),
  added_at TEXT NOT NULL,
  PRIMARY KEY (series_id, document_id),
  FOREIGN KEY (series_id, site_id)
    REFERENCES series(id, site_id) ON DELETE CASCADE,
  FOREIGN KEY (document_id, site_id)
    REFERENCES documents(id, site_id) ON DELETE CASCADE
);

CREATE INDEX series_items_order_idx
  ON series_items(series_id, position, document_id);
CREATE INDEX series_items_document_idx
  ON series_items(document_id, series_id);

-- Existing categories remain categories. Promotion into a first-class series
-- is an explicit, owner-authorized operation so upgrades cannot silently
-- change the meaning of a general-purpose taxonomy.
INSERT INTO schema_migrations(version, applied_at)
VALUES (9, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

const MIGRATION_10: &str = r#"
ALTER TABLE home_pins RENAME TO home_pins_v7;

CREATE TABLE home_pins (
  slot INTEGER PRIMARY KEY CHECK (slot BETWEEN 1 AND 3),
  target_kind TEXT NOT NULL CHECK (target_kind IN ('post', 'series')),
  document_id TEXT UNIQUE,
  series_id TEXT UNIQUE,
  pinned_by_user_id TEXT NOT NULL,
  pinned_at TEXT NOT NULL,
  CHECK (
    (target_kind = 'post' AND document_id IS NOT NULL AND series_id IS NULL)
    OR
    (target_kind = 'series' AND document_id IS NULL AND series_id IS NOT NULL)
  ),
  FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE,
  FOREIGN KEY (series_id) REFERENCES series(id) ON DELETE CASCADE,
  FOREIGN KEY (pinned_by_user_id) REFERENCES users(id) ON DELETE RESTRICT
);

-- A legacy document pin becomes a series pin only when the exact published
-- revision belongs to an active series in the configured primary site.
-- Multiple documents from that series retain the first old slot, then all
-- surviving targets are compacted back to the 1..3 slot invariant.
WITH normalized AS (
  SELECT
    legacy.slot AS old_slot,
    CASE WHEN home_series.id IS NULL THEN 'post' ELSE 'series' END AS target_kind,
    CASE WHEN home_series.id IS NULL THEN legacy.document_id ELSE NULL END AS document_id,
    home_series.id AS series_id,
    legacy.pinned_by_user_id,
    legacy.pinned_at
  FROM home_pins_v7 legacy
  JOIN documents document ON document.id = legacy.document_id
  LEFT JOIN revision_categories placement
    ON placement.revision_id = document.published_revision_id
   AND placement.document_id = document.id
   AND placement.site_id = document.site_id
  LEFT JOIN admin_control_plane control
    ON control.singleton = 1 AND control.primary_site_id = document.site_id
  LEFT JOIN series home_series
    ON home_series.site_id = control.primary_site_id
   AND home_series.category_id = placement.category_id
   AND EXISTS (
     SELECT 1 FROM categories category
     WHERE category.id = home_series.category_id
       AND category.site_id = home_series.site_id
       AND category.status = 'active'
   )
),
ranked AS (
  SELECT
    *,
    ROW_NUMBER() OVER (
      PARTITION BY target_kind, COALESCE(document_id, series_id)
      ORDER BY old_slot
    ) AS target_rank
  FROM normalized
),
compacted AS (
  SELECT
    ROW_NUMBER() OVER (ORDER BY old_slot) AS slot,
    target_kind,
    document_id,
    series_id,
    pinned_by_user_id,
    pinned_at
  FROM ranked
  WHERE target_rank = 1
)
INSERT INTO home_pins (
  slot, target_kind, document_id, series_id, pinned_by_user_id, pinned_at
)
SELECT
  slot, target_kind, document_id, series_id, pinned_by_user_id, pinned_at
FROM compacted
ORDER BY slot;

DROP TABLE home_pins_v7;

INSERT INTO schema_migrations(version, applied_at)
VALUES (10, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
"#;

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Barrier, mpsc},
        thread,
    };

    use osb_kernel::{
        AI2AI_SPEC_VERSION, AiActor, AiActorKind, AiPolicySnapshot, AiProvenanceEntry,
        ContextReceipt, DataBoundary, IntentLayer, RevisionActor, RevisionActorKind,
    };

    use super::*;

    fn actor() -> RevisionActor {
        RevisionActor {
            kind: RevisionActorKind::Human,
            id: "owner".into(),
            display_name: Some("Owner".into()),
        }
    }

    fn ai_envelope(
        document_id: Uuid,
        base_revision_id: Uuid,
        message_id: Uuid,
        idempotency_key: &str,
    ) -> Ai2AiEnvelope {
        Ai2AiEnvelope {
            spec_version: AI2AI_SPEC_VERSION.into(),
            message_id,
            idempotency_key: idempotency_key.into(),
            occurred_at: Utc::now(),
            actor: AiActor {
                kind: AiActorKind::Agent,
                id: "writer-agent".into(),
                provider: Some("local-model".into()),
                model: Some("small-writer-v1".into()),
            },
            intent: "Tighten the introduction without changing its meaning".into(),
            proposal: ProposedRevision {
                document_id,
                base_revision_id,
                title: "AI revised post".into(),
                slug: "ai-revised-post".into(),
                source_markdown: "# AI revised post\n\nReviewed text.".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                ai_summary: None,
                authorship: Default::default(),
                actor: actor(),
                idempotency_key: None,
            },
            policy: AiPolicySnapshot {
                data_boundary: DataBoundary::ApprovedProviders,
                allowed_provider_ids: vec!["local-model".into()],
                allowed_capabilities: vec!["content.propose".into()],
                max_cost: Some(0.01),
                max_tokens: Some(1_000),
            },
            context_receipts: vec![ContextReceipt {
                reference: "revision:base".into(),
                content_hash:
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                scope: "published-content".into(),
                included: true,
                exclusion_reason: None,
            }],
            provenance: vec![AiProvenanceEntry {
                kind: "prompt-template".into(),
                reference: "local:rewrite-v1".into(),
                content_hash: Some(
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .into(),
                ),
            }],
        }
    }

    fn community_user(repository: &SqliteRepository, handle: &str) -> UserRecord {
        repository
            .create_user(
                &format!("{handle}@example.test"),
                handle,
                &format!("{handle} display"),
                "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
            )
            .unwrap()
    }

    fn community_site(
        repository: &SqliteRepository,
        owner_user_id: Uuid,
        handle: &str,
    ) -> SiteRecord {
        repository
            .create_site(
                owner_user_id,
                handle,
                &format!("{handle} title"),
                Some("A readable community blog"),
                ThemeProfile::Paper,
            )
            .unwrap()
    }

    fn primary_owner_bootstrap(site_id: Uuid) -> PrimaryOwnerBootstrap {
        PrimaryOwnerBootstrap {
            site_id,
            site_handle: "primary-blog".into(),
            site_title: "Primary Blog".into(),
            site_description: Some("Owned on this server".into()),
            owner_display_name: "Primary Owner".into(),
            theme_profile: ThemeProfile::Forest,
        }
    }

    fn new_document(site_id: Uuid, title: &str, slug: &str) -> NewDocument {
        NewDocument {
            site_id,
            title: title.into(),
            slug: slug.into(),
            source_markdown: format!("# {title}"),
            embeds: vec![],
            intent: None,
            ontology: None,
            ai_summary: None,
            authorship: Default::default(),
            actor: actor(),
        }
    }

    fn offline_batch(created_at: DateTime<Utc>) -> OfflineImportBatch {
        OfflineImportBatch {
            source: "legacy-static-site".into(),
            owner_display_name: "me".into(),
            categories: vec![OfflineImportCategory {
                slug: "ontology".into(),
                title: "Ontology".into(),
                description: Some("Imported notes".into()),
            }],
            posts: vec![OfflineImportPost {
                source_id: "ontology:intro".into(),
                title: "An ontology introduction".into(),
                slug: "intro".into(),
                source_markdown: "# Ontology\n\nPreserved source.\n".into(),
                created_at,
                author_id: "legacy:me".into(),
                author_display_name: "me".into(),
                primary_category: "ontology".into(),
                human_reviewed: true,
                aliases: vec![OfflineImportAlias {
                    path: "topics/knowledge/ontology-intro.html".into(),
                    created_at,
                }],
            }],
        }
    }

    #[test]
    fn legacy_v2_sites_are_backfilled_and_delivery_can_open_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("legacy-v2.db");
        let mut connection = Connection::open(&database).unwrap();
        connection.execute_batch(MIGRATION_1).unwrap();
        connection.execute_batch(MIGRATION_2).unwrap();
        let site_id = Uuid::now_v7();
        let document_id = Uuid::now_v7();
        let revision_id = Uuid::now_v7();
        let now = Utc::now();
        let revision = with_computed_hash(RevisionSnapshot {
            schema_version: CONTENT_SCHEMA_VERSION.into(),
            id: revision_id,
            document_id,
            revision_number: 1,
            parent_revision_id: None,
            title: "Legacy published post".into(),
            slug: "legacy-published-post".into(),
            source_markdown: "still visible after migration".into(),
            embeds: vec![],
            intent: None,
            ontology: None,
            ai_summary: None,
            authorship: Default::default(),
            actor: actor(),
            content_hash: String::new(),
            created_at: now,
        });
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO documents (
                    id, site_id, status, current_revision_id, published_revision_id,
                    current_slug, created_at, updated_at
                 ) VALUES (?1, ?2, 'published', ?3, ?3, ?4, ?5, ?5)",
                params![
                    document_id.to_string(),
                    site_id.to_string(),
                    revision_id.to_string(),
                    revision.slug,
                    now.to_rfc3339(),
                ],
            )
            .unwrap();
        insert_revision(&transaction, &revision, None).unwrap();
        transaction
            .execute(
                "INSERT INTO routes (site_id, path, document_id, is_canonical, created_at)
                 VALUES (?1, ?2, ?3, 1, ?4)",
                params![
                    site_id.to_string(),
                    revision.slug,
                    document_id.to_string(),
                    now.to_rfc3339(),
                ],
            )
            .unwrap();
        transaction.commit().unwrap();
        connection.execute_batch(MIGRATION_3).unwrap();
        drop(connection);

        assert!(matches!(
            SqliteRepository::open_read_only(&database),
            Err(RepositoryError::Storage(_))
        ));
        let repository = SqliteRepository::open(&database).unwrap();
        let site = repository.get_site_by_id(site_id).unwrap();
        assert!(site.handle.starts_with("legacy-"));
        assert_eq!(site.theme_profile, ThemeProfile::Paper);
        assert_eq!(repository.list_published_across_sites(10).unwrap().len(), 1);
        drop(repository);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o444)).unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o555))
                .unwrap();
        }
        let read_only = SqliteRepository::open_read_only(&database).unwrap();
        assert_eq!(read_only.list_published_across_sites(10).unwrap().len(), 1);
        assert!(matches!(
            read_only.create_user(
                "write@example.test",
                "write",
                "Write",
                "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA"
            ),
            Err(RepositoryError::Storage(_))
        ));
        drop(read_only);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    #[test]
    fn migration_eight_backfills_revision_placements_and_gates_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("legacy-v7.db");
        let mut connection = Connection::open(&database).unwrap();
        for migration in [
            MIGRATION_1,
            MIGRATION_2,
            MIGRATION_3,
            MIGRATION_4,
            MIGRATION_5,
            MIGRATION_6,
            MIGRATION_7,
        ] {
            connection.execute_batch(migration).unwrap();
        }

        let owner_id = Uuid::now_v7();
        let site_id = Uuid::now_v7();
        let document_id = Uuid::now_v7();
        let revision_id = Uuid::now_v7();
        let now = Utc::now();
        let revision = with_computed_hash(RevisionSnapshot {
            schema_version: CONTENT_SCHEMA_VERSION.into(),
            id: revision_id,
            document_id,
            revision_number: 1,
            parent_revision_id: None,
            title: "Version seven post".into(),
            slug: "version-seven-post".into(),
            source_markdown: "Still readable after migration.".into(),
            embeds: vec![],
            intent: None,
            ontology: None,
            ai_summary: None,
            authorship: Default::default(),
            actor: actor(),
            content_hash: String::new(),
            created_at: now,
        });
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO users (
                    id, email, handle, display_name, password_phc, created_at, updated_at
                 ) VALUES (?1, 'v7-owner@example.test', 'v7-owner', 'V7 owner',
                           '$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA', ?2, ?2)",
                params![owner_id.to_string(), now.to_rfc3339()],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO sites (
                    id, handle, title, description, current_theme_revision, created_at, updated_at
                 ) VALUES (?1, 'v7-site', 'V7 site', NULL, 1, ?2, ?2)",
                params![site_id.to_string(), now.to_rfc3339()],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO site_memberships (site_id, user_id, role, created_at)
                 VALUES (?1, ?2, 'owner', ?3)",
                params![site_id.to_string(), owner_id.to_string(), now.to_rfc3339()],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO site_theme_revisions (
                    site_id, revision, profile, custom_css, created_by_user_id, created_at
                 ) VALUES (?1, 1, 'paper', NULL, ?2, ?3)",
                params![site_id.to_string(), owner_id.to_string(), now.to_rfc3339()],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO documents (
                    id, site_id, status, current_revision_id, published_revision_id,
                    current_slug, created_at, updated_at
                 ) VALUES (?1, ?2, 'draft', ?3, NULL, ?4, ?5, ?5)",
                params![
                    document_id.to_string(),
                    site_id.to_string(),
                    revision_id.to_string(),
                    revision.slug,
                    now.to_rfc3339(),
                ],
            )
            .unwrap();
        insert_revision(&transaction, &revision, None).unwrap();
        transaction.commit().unwrap();
        drop(connection);

        assert!(matches!(
            SqliteRepository::open_read_only(&database),
            Err(RepositoryError::Storage(_))
        ));
        let repository = SqliteRepository::open(&database).unwrap();
        let placement = repository
            .get_revision_category_placement(site_id, document_id, revision_id)
            .unwrap();
        assert_eq!(placement.category_id, None);
        assert_eq!(placement.assigned_by_user_id, None);

        let next = repository
            .revise_document_in_owned_site(
                owner_id,
                site_id,
                ProposedRevision {
                    document_id,
                    base_revision_id: revision_id,
                    title: "Version eight draft".into(),
                    slug: "version-eight-draft".into(),
                    source_markdown: "The trigger creates this placement.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
            )
            .unwrap();
        assert_eq!(
            repository
                .get_revision_category_placement(site_id, document_id, next.id)
                .unwrap()
                .category_id,
            None
        );
        repository.migrate().unwrap();
        let placement_count: i64 = repository
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM revision_categories", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(placement_count, 2);
        drop(repository);

        let delivery = SqliteRepository::open_read_only(&database).unwrap();
        assert_eq!(
            delivery
                .get_revision_category_placement(site_id, document_id, next.id)
                .unwrap()
                .category_id,
            None
        );
    }

    #[test]
    fn migration_nine_adds_empty_series_state_explicitly_and_gates_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("schema-v8.db");
        let mut connection = Connection::open(&database).unwrap();
        for migration in [
            MIGRATION_1,
            MIGRATION_2,
            MIGRATION_3,
            MIGRATION_4,
            MIGRATION_5,
            MIGRATION_6,
            MIGRATION_7,
            MIGRATION_8,
        ] {
            connection.execute_batch(migration).unwrap();
        }
        let owner_id = Uuid::now_v7();
        let site_id = Uuid::now_v7();
        let category_id = Uuid::now_v7();
        let now = Utc::now().to_rfc3339();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO users (
                    id, email, handle, display_name, password_phc, created_at, updated_at
                 ) VALUES (?1, 'series-v8@example.test', 'series-v8', 'Series V8',
                           '$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA', ?2, ?2)",
                params![owner_id.to_string(), now],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO sites (
                    id, handle, title, description, current_theme_revision, created_at, updated_at
                 ) VALUES (?1, 'series-v8-site', 'Series V8 site', NULL, 1, ?2, ?2)",
                params![site_id.to_string(), now],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO site_memberships (site_id, user_id, role, created_at)
                 VALUES (?1, ?2, 'owner', ?3)",
                params![site_id.to_string(), owner_id.to_string(), now],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO site_theme_revisions (
                    site_id, revision, profile, custom_css, created_by_user_id, created_at
                 ) VALUES (?1, 1, 'paper', NULL, ?2, ?3)",
                params![site_id.to_string(), owner_id.to_string(), now],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO categories (
                    id, site_id, slug, title, description, theme_profile, status,
                    created_by_user_id, created_at, updated_at
                 ) VALUES (?1, ?2, 'existing-category', 'Existing category', NULL,
                           NULL, 'active', ?3, ?4, ?4)",
                params![
                    category_id.to_string(),
                    site_id.to_string(),
                    owner_id.to_string(),
                    now,
                ],
            )
            .unwrap();
        transaction.commit().unwrap();
        drop(connection);

        assert!(matches!(
            SqliteRepository::open_read_only(&database),
            Err(RepositoryError::Storage(_))
        ));
        let repository = SqliteRepository::open(&database).unwrap();
        assert!(
            repository
                .list_series(site_id, true, 500)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repository
                .get_category_by_id(site_id, category_id)
                .unwrap()
                .slug,
            "existing-category"
        );
        repository.migrate().unwrap();
        let connection = repository.lock().unwrap();
        let versions: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 9",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(versions, 1);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM series", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(connection);
        drop(repository);

        let delivery = SqliteRepository::open_read_only(&database).unwrap();
        assert!(delivery.list_series(site_id, true, 500).unwrap().is_empty());
    }

    #[test]
    fn migration_ten_normalizes_legacy_document_pins_and_compacts_series_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("schema-v9-home-pins.db");
        let repository = SqliteRepository::open(&database).unwrap();
        let site_id = Uuid::now_v7();
        let control = repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(site_id),
                AdminAuthMode::AccessKey,
                &[23; 32],
            )
            .unwrap();
        let series = repository
            .create_series(
                control.owner_user_id,
                site_id,
                CreateSeriesInput {
                    slug: "migration-series".into(),
                    title: "Migration series".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let publish = |title: &str, slug: &str, category_id: Option<Uuid>| {
            let document = repository
                .create_document_in_writable_site_with_category(
                    control.owner_user_id,
                    new_document(site_id, title, slug),
                    category_id,
                )
                .unwrap();
            repository
                .publish_document_in_owned_site(
                    control.owner_user_id,
                    site_id,
                    document.id,
                    document.current_revision_id,
                )
                .unwrap();
            document.id
        };
        let first_series_post = publish(
            "First migration entry",
            "first-migration-entry",
            Some(series.category_id),
        );
        let second_series_post = publish(
            "Second migration entry",
            "second-migration-entry",
            Some(series.category_id),
        );
        let standalone = publish("Migration standalone", "migration-standalone", None);
        let now = Utc::now().to_rfc3339();
        {
            let connection = repository.lock().unwrap();
            connection
                .execute_batch(
                    "DROP TABLE home_pins;
                     CREATE TABLE home_pins (
                       slot INTEGER PRIMARY KEY CHECK (slot BETWEEN 1 AND 3),
                       document_id TEXT NOT NULL UNIQUE,
                       pinned_by_user_id TEXT NOT NULL,
                       pinned_at TEXT NOT NULL,
                       FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE,
                       FOREIGN KEY (pinned_by_user_id) REFERENCES users(id) ON DELETE RESTRICT
                     );
                     DELETE FROM schema_migrations WHERE version = 10;",
                )
                .unwrap();
            for (slot, document_id) in [
                (1_i64, first_series_post),
                (2_i64, second_series_post),
                (3_i64, standalone),
            ] {
                connection
                    .execute(
                        "INSERT INTO home_pins (
                           slot, document_id, pinned_by_user_id, pinned_at
                         ) VALUES (?1, ?2, ?3, ?4)",
                        params![
                            slot,
                            document_id.to_string(),
                            control.owner_user_id.to_string(),
                            now,
                        ],
                    )
                    .unwrap();
            }
        }
        drop(repository);

        assert!(SqliteRepository::open_read_only(&database).is_err());
        let repository = SqliteRepository::open(&database).unwrap();
        let pins = repository.list_home_pins().unwrap();
        assert_eq!(
            pins.iter()
                .map(|pin| (pin.slot, pin.target))
                .collect::<Vec<_>>(),
            vec![
                (1, HomePinTarget::Series { id: series.id }),
                (2, HomePinTarget::Post { id: standalone }),
            ]
        );
        drop(repository);
        let delivery = SqliteRepository::open_read_only(&database).unwrap();
        assert_eq!(delivery.list_home_pins().unwrap().len(), 2);
    }

    #[test]
    fn offline_import_preserves_metadata_aliases_and_exact_retry_safety() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "import-owner");
        let site = community_site(&repository, owner.id, "import-site");
        let created_at = DateTime::parse_from_rfc3339("2019-04-03T02:01:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let batch = offline_batch(created_at);

        let first = repository
            .import_offline_batch(owner.id, site.id, batch.clone(), false)
            .unwrap();
        assert_eq!(first.posts_imported, 1);
        assert_eq!(first.aliases_created, 1);
        assert!(first.owner_display_name_updated);
        assert_eq!(
            repository.get_user_by_id(owner.id).unwrap().display_name,
            "me"
        );

        let canonical = repository
            .get_published_by_slug(site.id, "ontology/intro")
            .unwrap();
        let legacy = repository
            .get_published_by_slug(site.id, "topics/knowledge/ontology-intro.html")
            .unwrap();
        assert_eq!(legacy.id, canonical.id);
        assert_eq!(canonical.created_at, created_at);
        assert_eq!(canonical.updated_at, created_at);
        assert_eq!(canonical.revision.created_at, created_at);
        assert_eq!(canonical.revision.actor.kind, RevisionActorKind::Importer);
        assert_eq!(canonical.revision.actor.id, "legacy:me");
        assert_eq!(canonical.revision.actor.display_name.as_deref(), Some("me"));
        assert_eq!(
            canonical.revision.authorship.kind,
            PublicAuthorshipKind::Imported
        );
        assert_eq!(
            canonical.revision.authorship.generator.as_deref(),
            Some("legacy-static-site")
        );
        let category = repository
            .get_published_category(site.id, canonical.id)
            .unwrap()
            .unwrap();
        assert_eq!(category.slug, "ontology");
        let exported = repository.export_site(site.id).unwrap();
        let routes = &exported.documents[0].routes;
        assert!(routes.iter().any(|route| {
            route.path == "topics/knowledge/ontology-intro.html"
                && !route.canonical
                && route.created_at == created_at
        }));

        let retry = repository
            .import_offline_batch(owner.id, site.id, batch.clone(), false)
            .unwrap();
        assert_eq!(retry.posts_imported, 0);
        assert_eq!(retry.posts_unchanged, 1);
        assert_eq!(retry.categories_reused, 1);
        assert!(!retry.owner_display_name_updated);
        assert_eq!(repository.list_published(site.id, 10).unwrap().len(), 1);

        let mut appended = batch;
        appended.owner_display_name = "Updated owner".into();
        appended.categories.push(OfflineImportCategory {
            slug: "yangja".into(),
            title: "Yangja".into(),
            description: None,
        });
        appended.posts.push(OfflineImportPost {
            source_id: "yangja:intro".into(),
            title: "A quantum introduction".into(),
            slug: "intro".into(),
            source_markdown: "# Quantum\n".into(),
            created_at,
            author_id: "legacy:me".into(),
            author_display_name: "me".into(),
            primary_category: "yangja".into(),
            human_reviewed: true,
            aliases: Vec::new(),
        });
        let append_report = repository
            .import_offline_batch(owner.id, site.id, appended, false)
            .unwrap();
        assert_eq!(append_report.posts_unchanged, 1);
        assert_eq!(append_report.posts_imported, 1);
        assert_eq!(append_report.categories_reused, 1);
        assert_eq!(append_report.categories_created, 1);
        assert!(append_report.owner_display_name_updated);
        assert_eq!(repository.list_published(site.id, 10).unwrap().len(), 2);
    }

    #[test]
    fn offline_import_dry_run_and_conflicts_roll_back_every_change() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "rollback-owner");
        let site = community_site(&repository, owner.id, "rollback-site");
        let created_at = DateTime::parse_from_rfc3339("2020-05-06T07:08:09Z")
            .unwrap()
            .with_timezone(&Utc);
        let batch = offline_batch(created_at);

        let dry_run = repository
            .import_offline_batch(owner.id, site.id, batch.clone(), true)
            .unwrap();
        assert_eq!(dry_run.posts_imported, 1);
        assert!(matches!(
            repository.get_category_by_slug(site.id, "ontology"),
            Err(RepositoryError::NotFound)
        ));
        assert_eq!(
            repository.get_user_by_id(owner.id).unwrap().display_name,
            owner.display_name
        );

        repository
            .import_offline_batch(owner.id, site.id, batch.clone(), false)
            .unwrap();
        let mut conflict = batch;
        conflict.owner_display_name = "must roll back".into();
        conflict.categories.push(OfflineImportCategory {
            slug: "yangja".into(),
            title: "Yangja".into(),
            description: None,
        });
        conflict.posts[0].source_markdown.push_str("drift");
        assert!(matches!(
            repository.import_offline_batch(owner.id, site.id, conflict, false),
            Err(RepositoryError::Validation(_))
        ));
        assert_eq!(
            repository.get_user_by_id(owner.id).unwrap().display_name,
            "me"
        );
        assert!(matches!(
            repository.get_category_by_slug(site.id, "yangja"),
            Err(RepositoryError::NotFound)
        ));
        assert_eq!(
            repository
                .get_published_by_slug(site.id, "ontology/intro")
                .unwrap()
                .revision
                .source_markdown,
            "# Ontology\n\nPreserved source.\n"
        );
    }

    #[test]
    fn offline_import_rejects_effective_article_and_static_route_aliases() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "reserved-import-owner");
        let site = community_site(&repository, owner.id, "reserved-import-site");
        let created_at = DateTime::parse_from_rfc3339("2020-05-06T07:08:09Z")
            .unwrap()
            .with_timezone(&Utc);

        for alias in ["writing/legacy-post.html", "favicon.svg"] {
            let mut batch = offline_batch(created_at);
            batch.posts[0].aliases[0].path = alias.into();
            let result = repository.import_offline_batch_with_reserved_roots(
                owner.id,
                site.id,
                batch,
                &["writing"],
                false,
            );
            assert!(matches!(result, Err(RepositoryError::Validation(_))));
            assert!(matches!(
                repository.get_category_by_slug(site.id, "ontology"),
                Err(RepositoryError::NotFound)
            ));
        }

        let mut category_conflict = offline_batch(created_at);
        category_conflict.categories[0].slug = "writing".into();
        category_conflict.posts[0].primary_category = "writing".into();
        assert!(matches!(
            repository.import_offline_batch_with_reserved_roots(
                owner.id,
                site.id,
                category_conflict,
                &["writing"],
                false,
            ),
            Err(RepositoryError::Validation(_))
        ));
    }

    #[test]
    fn published_noncanonical_route_query_excludes_the_current_storage_route() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "route-kind-owner");
        let site = community_site(&repository, owner.id, "route-kind-site");
        let document = repository
            .create_document_in_writable_site(
                owner.id,
                new_document(site.id, "Policy", "references"),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                owner.id,
                site.id,
                document.id,
                document.current_revision_id,
            )
            .unwrap();

        assert!(
            !repository
                .published_noncanonical_route_exists(site.id, "references")
                .unwrap()
        );

        let revision = repository
            .revise_document_in_owned_site(
                owner.id,
                site.id,
                ProposedRevision {
                    document_id: document.id,
                    base_revision_id: document.current_revision_id,
                    title: "Moved policy".into(),
                    slug: "moved-policy".into(),
                    source_markdown: "# Moved policy".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: Some("route-kind-revision".into()),
                },
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(owner.id, site.id, document.id, revision.id)
            .unwrap();

        assert!(
            repository
                .published_noncanonical_route_exists(site.id, "references")
                .unwrap()
        );
        assert!(
            !repository
                .published_noncanonical_route_exists(site.id, "moved-policy")
                .unwrap()
        );
    }

    #[test]
    fn legacy_reserved_category_slugs_remain_readable_but_cannot_be_created() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "legacy-category-owner");
        let site = community_site(&repository, owner.id, "legacy-category-site");
        let category = repository
            .create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "legacy-references".into(),
                    title: "Legacy references".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        {
            let connection = repository.lock().unwrap();
            connection
                .execute_batch("DROP TRIGGER categories_slug_immutable;")
                .unwrap();
            connection
                .execute(
                    "UPDATE categories SET slug = 'references' WHERE id = ?1",
                    [category.id.to_string()],
                )
                .unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER categories_slug_immutable
                     BEFORE UPDATE OF slug ON categories
                     WHEN NEW.slug != OLD.slug
                     BEGIN
                       SELECT RAISE(ABORT, 'category slugs are immutable');
                     END;",
                )
                .unwrap();
        }

        assert_eq!(
            repository
                .get_category_by_slug(site.id, "references")
                .unwrap()
                .id,
            category.id
        );
        let error = repository
            .create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "references".into(),
                    title: "New references".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap_err();
        assert!(matches!(error, RepositoryError::Validation(_)));
        assert!(error.to_string().contains("reserved by the application"));
    }

    #[test]
    fn categories_are_site_scoped_atomic_and_keep_published_placement_stable() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "category-owner");
        let site = community_site(&repository, owner.id, "category-site");
        let other_owner = community_user(&repository, "other-category-owner");
        let other_site = community_site(&repository, other_owner.id, "other-category-site");

        let reserved = repository.create_category(
            owner.id,
            site.id,
            CreateCategoryInput {
                slug: "references".into(),
                title: "Reserved".into(),
                description: None,
                theme_profile: None,
            },
        );
        assert!(matches!(reserved, Err(RepositoryError::Validation(_))));

        let yangja = repository
            .create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: " Yangja ".into(),
                    title: "Yangja".into(),
                    description: Some("Long-form notes".into()),
                    theme_profile: Some(ThemeProfile::Forest),
                },
            )
            .unwrap();
        assert_eq!(yangja.slug, "yangja");
        let lab = repository
            .create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "lab".into(),
                    title: "Lab".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let other_category = repository
            .create_category(
                other_owner.id,
                other_site.id,
                CreateCategoryInput {
                    slug: "elsewhere".into(),
                    title: "Elsewhere".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();

        assert_eq!(
            repository
                .list_categories(site.id, false, 10)
                .unwrap()
                .len(),
            2
        );
        assert!(matches!(
            repository.create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "YANGJA".into(),
                    title: "Duplicate".into(),
                    description: None,
                    theme_profile: None,
                }
            ),
            Err(RepositoryError::DuplicateSlug)
        ));
        assert!(matches!(
            repository.create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "studio".into(),
                    title: "Reserved".into(),
                    description: None,
                    theme_profile: None,
                }
            ),
            Err(RepositoryError::Validation(_))
        ));
        assert!(matches!(
            repository.get_category_by_id(other_site.id, yangja.id),
            Err(RepositoryError::NotFound)
        ));
        assert!(matches!(
            repository.update_category(
                other_owner.id,
                site.id,
                yangja.id,
                UpdateCategoryInput {
                    title: "Cross-site".into(),
                    description: None,
                    theme_profile: None,
                }
            ),
            Err(RepositoryError::NotFound)
        ));

        assert!(matches!(
            repository.create_document_in_owned_site(
                owner.id,
                new_document(site.id, "Landing collision", "yangja")
            ),
            Err(RepositoryError::DuplicateSlug)
        ));
        repository
            .create_document_in_owned_site(
                owner.id,
                new_document(site.id, "Existing root", "existing-root"),
            )
            .unwrap();
        assert!(matches!(
            repository.create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "existing-root".into(),
                    title: "Collision".into(),
                    description: None,
                    theme_profile: None,
                }
            ),
            Err(RepositoryError::DuplicateSlug)
        ));

        let before_failed_create = repository
            .list_documents_in_writable_site(owner.id, site.id, 100)
            .unwrap()
            .len();
        assert!(matches!(
            repository.create_document_in_writable_site_with_category(
                owner.id,
                new_document(site.id, "Wrong category", "wrong-category"),
                Some(other_category.id),
            ),
            Err(RepositoryError::NotFound)
        ));
        assert_eq!(
            repository
                .list_documents_in_writable_site(owner.id, site.id, 100)
                .unwrap()
                .len(),
            before_failed_create
        );

        let document = repository
            .create_document_in_writable_site_with_category(
                owner.id,
                new_document(site.id, "First public title", "hello"),
                Some(yangja.id),
            )
            .unwrap();
        assert_eq!(
            repository
                .get_current_category(site.id, document.id)
                .unwrap()
                .unwrap()
                .id,
            yangja.id
        );
        repository
            .publish_document_in_owned_site(
                owner.id,
                site.id,
                document.id,
                document.current_revision_id,
            )
            .unwrap();
        assert_eq!(
            repository
                .get_published_by_slug(site.id, "yangja/hello")
                .unwrap()
                .revision
                .title,
            "First public title"
        );
        assert!(matches!(
            repository.assign_revision_category_in_writable_site(
                owner.id,
                site.id,
                document.id,
                document.current_revision_id,
                Some(lab.id)
            ),
            Err(RepositoryError::Validation(_))
        ));

        let inherited = repository
            .revise_document_in_writable_site_with_category(
                owner.id,
                site.id,
                ProposedRevision {
                    document_id: document.id,
                    base_revision_id: document.current_revision_id,
                    title: "Inherited draft".into(),
                    slug: "hello".into(),
                    source_markdown: "The category is inherited.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            repository
                .get_current_category(site.id, document.id)
                .unwrap()
                .unwrap()
                .id,
            yangja.id
        );
        let moved = repository
            .revise_document_in_writable_site_with_category(
                owner.id,
                site.id,
                ProposedRevision {
                    document_id: document.id,
                    base_revision_id: inherited.id,
                    title: "Moved draft".into(),
                    slug: "hello".into(),
                    source_markdown: "Only the draft moves to Lab.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
                Some(Some(lab.id)),
            )
            .unwrap();
        assert!(matches!(
            repository.assign_revision_category_in_writable_site(
                owner.id,
                site.id,
                document.id,
                moved.id,
                Some(other_category.id)
            ),
            Err(RepositoryError::NotFound)
        ));
        assert_eq!(
            repository
                .get_current_category(site.id, document.id)
                .unwrap()
                .unwrap()
                .id,
            lab.id
        );
        assert_eq!(
            repository
                .get_published_category(site.id, document.id)
                .unwrap()
                .unwrap()
                .id,
            yangja.id
        );
        assert_eq!(
            repository
                .list_published_in_category(site.id, yangja.id, 10)
                .unwrap()
                .len(),
            1
        );
        assert!(
            repository
                .list_published_in_category(site.id, lab.id, 10)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            repository.get_published_by_slug(site.id, "lab/hello"),
            Err(RepositoryError::NotFound)
        ));

        let archived_yangja = repository
            .archive_category(owner.id, site.id, yangja.id)
            .unwrap();
        assert_eq!(archived_yangja.status, CategoryStatus::Archived);
        assert_eq!(
            repository
                .get_published_by_slug(site.id, "yangja/hello")
                .unwrap()
                .revision
                .title,
            "First public title"
        );
        repository
            .publish_document_in_owned_site(owner.id, site.id, document.id, moved.id)
            .unwrap();
        assert_eq!(
            repository
                .get_published_category(site.id, document.id)
                .unwrap()
                .unwrap()
                .id,
            lab.id
        );
        assert!(
            repository
                .list_published_in_category(site.id, yangja.id, 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repository
                .list_published_in_category(site.id, lab.id, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            repository
                .get_published_by_slug(site.id, "yangja/hello")
                .unwrap()
                .revision
                .title,
            "Moved draft"
        );
        assert_eq!(
            repository
                .get_published_by_slug(site.id, "lab/hello")
                .unwrap()
                .revision
                .title,
            "Moved draft"
        );

        repository
            .archive_category(owner.id, site.id, lab.id)
            .unwrap();
        let archived_inherited = repository
            .revise_document_in_writable_site_with_category(
                owner.id,
                site.id,
                ProposedRevision {
                    document_id: document.id,
                    base_revision_id: moved.id,
                    title: "Archived category draft".into(),
                    slug: "hello-next".into(),
                    source_markdown: "Drafting remains possible before moving it elsewhere.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
                None,
            )
            .unwrap();
        assert!(matches!(
            repository.publish_document_in_owned_site(
                owner.id,
                site.id,
                document.id,
                archived_inherited.id
            ),
            Err(RepositoryError::Validation(_))
        ));
        assert_eq!(
            repository
                .list_categories(site.id, false, 10)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            repository.list_categories(site.id, true, 10).unwrap().len(),
            2
        );
    }

    #[test]
    fn metadata_pages_cross_legacy_site_and_category_caps_without_private_hydration() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "metadata-page-owner");
        let base_time = Utc::now() - chrono::Duration::days(1);
        let mut site_ids = Vec::with_capacity(501);
        {
            let mut connection = repository.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for index in 0..501 {
                let site_id = Uuid::now_v7();
                let timestamp = (base_time + chrono::Duration::seconds(index)).to_rfc3339();
                let handle = format!("metadata-site-{index:04}");
                transaction
                    .execute(
                        "INSERT INTO sites (
                            id, handle, title, description, current_theme_revision,
                            created_at, updated_at
                         ) VALUES (?1, ?2, ?3, 'PRIVATE DESCRIPTION', 1, ?4, ?4)",
                        params![
                            site_id.to_string(),
                            handle,
                            format!("Metadata site {index:04}"),
                            timestamp,
                        ],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "INSERT INTO site_theme_revisions (
                            site_id, revision, profile, custom_css,
                            created_by_user_id, created_at
                         ) VALUES (?1, 1, 'paper', '/* PRIVATE CSS */', ?2, ?3)",
                        params![site_id.to_string(), owner.id.to_string(), timestamp],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "INSERT INTO site_memberships (site_id, user_id, role, created_at)
                         VALUES (?1, ?2, 'owner', ?3)",
                        params![site_id.to_string(), owner.id.to_string(), timestamp],
                    )
                    .unwrap();
                site_ids.push(site_id);
            }
            transaction.commit().unwrap();
        }

        let first_sites = repository.list_site_metadata_page(0, 2).unwrap();
        assert_eq!(first_sites[0].handle, "metadata-site-0500");
        assert_eq!(first_sites[1].handle, "metadata-site-0499");
        let site_beyond_old_cap = repository.list_site_metadata_page(500, 2).unwrap();
        assert_eq!(site_beyond_old_cap.len(), 1);
        assert_eq!(site_beyond_old_cap[0].handle, "metadata-site-0000");

        let category_site_id = site_ids[0];
        {
            let mut connection = repository.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for index in 0..501 {
                let category_id = Uuid::now_v7();
                let timestamp = (base_time + chrono::Duration::seconds(index)).to_rfc3339();
                transaction
                    .execute(
                        "INSERT INTO categories (
                            id, site_id, slug, title, description, theme_profile,
                            status, created_by_user_id, created_at, updated_at
                         ) VALUES (?1, ?2, ?3, ?4, 'PRIVATE CATEGORY DESCRIPTION',
                                   NULL, 'active', ?5, ?6, ?6)",
                        params![
                            category_id.to_string(),
                            category_site_id.to_string(),
                            format!("category-{index:04}"),
                            format!("Category {index:04}"),
                            owner.id.to_string(),
                            timestamp,
                        ],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        }

        let category_beyond_old_cap = repository
            .list_category_metadata_page(category_site_id, true, 500, 2)
            .unwrap();
        assert_eq!(category_beyond_old_cap.len(), 1);
        assert_eq!(category_beyond_old_cap[0].slug, "category-0500");
        assert!(matches!(
            repository.list_category_metadata_page(Uuid::now_v7(), true, 0, 2),
            Err(RepositoryError::NotFound)
        ));
    }

    #[test]
    fn current_document_and_revision_metadata_pages_cross_legacy_caps_and_stay_scoped() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "document-page-owner");
        let site = community_site(&repository, owner.id, "document-page-site");
        let category = repository
            .create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "selected".into(),
                    title: "Selected".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let other_owner = community_user(&repository, "other-document-page-owner");
        let other_site = community_site(&repository, other_owner.id, "other-document-page-site");
        let other_category = repository
            .create_category(
                other_owner.id,
                other_site.id,
                CreateCategoryInput {
                    slug: "other-selected".into(),
                    title: "Other selected".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let base_time = Utc::now() - chrono::Duration::days(1);
        let mut document_ids = Vec::with_capacity(501);
        let mut first_revision_ids = Vec::with_capacity(501);
        let categorized_document_id;
        {
            let mut connection = repository.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for index in 0..501 {
                let document_id = Uuid::now_v7();
                let revision_id = Uuid::now_v7();
                let slug = format!("document-{index:04}");
                let timestamp = (base_time + chrono::Duration::seconds(index)).to_rfc3339();
                transaction
                    .execute(
                        "INSERT INTO documents (
                            id, site_id, status, current_revision_id,
                            published_revision_id, current_slug, created_at, updated_at
                         ) VALUES (?1, ?2, 'draft', ?3, NULL, ?4, ?5, ?5)",
                        params![
                            document_id.to_string(),
                            site.id.to_string(),
                            revision_id.to_string(),
                            slug,
                            timestamp,
                        ],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "INSERT INTO revisions (
                            id, document_id, revision_number, parent_revision_id,
                            slug, snapshot_json, idempotency_key, created_at
                         ) VALUES (?1, ?2, 1, NULL, ?3, ?4, NULL, ?5)",
                        params![
                            revision_id.to_string(),
                            document_id.to_string(),
                            slug,
                            serde_json::json!({ "title": format!("Document {index:04}") })
                                .to_string(),
                            timestamp,
                        ],
                    )
                    .unwrap();
                document_ids.push(document_id);
                first_revision_ids.push(revision_id);
            }

            let document_id = Uuid::now_v7();
            let revision_id = Uuid::now_v7();
            let timestamp = (base_time + chrono::Duration::seconds(1_000)).to_rfc3339();
            transaction
                .execute(
                    "INSERT INTO documents (
                        id, site_id, status, current_revision_id,
                        published_revision_id, current_slug, created_at, updated_at
                     ) VALUES (?1, ?2, 'draft', ?3, NULL, 'categorized-decoy', ?4, ?4)",
                    params![
                        document_id.to_string(),
                        site.id.to_string(),
                        revision_id.to_string(),
                        timestamp,
                    ],
                )
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO revisions (
                        id, document_id, revision_number, parent_revision_id,
                        slug, snapshot_json, idempotency_key, created_at
                     ) VALUES (?1, ?2, 1, NULL, 'categorized-decoy',
                               '{\"title\":\"Categorized decoy\"}', NULL, ?3)",
                    params![revision_id.to_string(), document_id.to_string(), timestamp,],
                )
                .unwrap();
            transaction
                .execute(
                    "UPDATE revision_categories SET category_id = ?1
                     WHERE revision_id = ?2",
                    params![category.id.to_string(), revision_id.to_string()],
                )
                .unwrap();
            categorized_document_id = document_id;
            transaction.commit().unwrap();
        }

        let first_uncategorized = repository
            .list_current_document_metadata_page(site.id, None, 0, 2)
            .unwrap();
        assert_eq!(first_uncategorized[0].title, "Document 0500");
        assert_eq!(first_uncategorized[1].title, "Document 0499");
        let document_beyond_old_cap = repository
            .list_current_document_metadata_page(site.id, None, 500, 2)
            .unwrap();
        assert_eq!(document_beyond_old_cap.len(), 1);
        assert_eq!(document_beyond_old_cap[0].title, "Document 0000");
        assert_eq!(document_beyond_old_cap[0].category_id, None);
        let categorized = repository
            .list_current_document_metadata_page(site.id, Some(category.id), 0, 2)
            .unwrap();
        assert_eq!(categorized.len(), 1);
        assert_eq!(categorized[0].id, categorized_document_id);
        assert_eq!(categorized[0].category_id, Some(category.id));
        assert!(matches!(
            repository.list_current_document_metadata_page(site.id, Some(other_category.id), 0, 2),
            Err(RepositoryError::NotFound)
        ));

        let history_document_id = document_ids[0];
        let first_revision_id = first_revision_ids[0];
        let latest_revision_id;
        {
            let mut connection = repository.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            let mut last_revision_id = first_revision_id;
            for revision_number in 2..=1_001_i64 {
                let revision_id = Uuid::now_v7();
                let timestamp =
                    (base_time + chrono::Duration::seconds(revision_number)).to_rfc3339();
                let slug = format!("history-{revision_number:04}");
                transaction
                    .execute(
                        "INSERT INTO revisions (
                            id, document_id, revision_number, parent_revision_id,
                            slug, snapshot_json, idempotency_key, created_at
                         ) VALUES (?1, ?2, ?3, ?4, ?5, '{\"title\":\"History\"}', NULL, ?6)",
                        params![
                            revision_id.to_string(),
                            history_document_id.to_string(),
                            revision_number,
                            last_revision_id.to_string(),
                            slug,
                            timestamp,
                        ],
                    )
                    .unwrap();
                last_revision_id = revision_id;
            }
            transaction
                .execute(
                    "UPDATE documents
                     SET current_revision_id = ?1, current_slug = 'history-1001'
                     WHERE id = ?2 AND site_id = ?3",
                    params![
                        last_revision_id.to_string(),
                        history_document_id.to_string(),
                        site.id.to_string(),
                    ],
                )
                .unwrap();
            latest_revision_id = last_revision_id;
            transaction.commit().unwrap();
        }

        let newest_revisions = repository
            .list_revision_metadata_page(history_document_id, 0, 2)
            .unwrap();
        assert_eq!(newest_revisions[0].id, latest_revision_id);
        assert_eq!(newest_revisions[0].revision_number, 1_001);
        assert_eq!(newest_revisions[1].revision_number, 1_000);
        let revision_beyond_old_cap = repository
            .list_revision_metadata_page(history_document_id, 1_000, 2)
            .unwrap();
        assert_eq!(revision_beyond_old_cap.len(), 1);
        assert_eq!(revision_beyond_old_cap[0].id, first_revision_id);
        assert_eq!(revision_beyond_old_cap[0].revision_number, 1);
        assert!(matches!(
            repository.list_revision_metadata_page(Uuid::now_v7(), 0, 2),
            Err(RepositoryError::NotFound)
        ));
    }

    #[test]
    fn atomic_category_move_checks_only_the_final_route() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "atomic-move-owner");
        let site = community_site(&repository, owner.id, "atomic-move-site");
        let old_category = repository
            .create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "old".into(),
                    title: "Old".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let new_category = repository
            .create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "new".into(),
                    title: "New".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        repository
            .create_document_in_writable_site_with_category(
                owner.id,
                new_document(site.id, "Occupied old route", "target"),
                Some(old_category.id),
            )
            .unwrap();
        let moving = repository
            .create_document_in_writable_site_with_category(
                owner.id,
                new_document(site.id, "Moving document", "source"),
                Some(old_category.id),
            )
            .unwrap();

        let moved = repository
            .revise_document_in_writable_site_with_category(
                owner.id,
                site.id,
                ProposedRevision {
                    document_id: moving.id,
                    base_revision_id: moving.current_revision_id,
                    title: "Moved document".into(),
                    slug: "target".into(),
                    source_markdown: "The final route is new/target.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
                Some(Some(new_category.id)),
            )
            .unwrap();

        assert_eq!(
            repository
                .get_revision_category_placement(site.id, moving.id, moved.id)
                .unwrap()
                .category_id,
            Some(new_category.id)
        );
        assert_eq!(
            repository
                .get_document_in_writable_site(owner.id, site.id, moving.id)
                .unwrap()
                .revision
                .slug,
            "target"
        );
    }

    #[test]
    fn site_export_v4_preserves_categories_and_revision_placements() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "category-export-owner");
        let site = community_site(&repository, owner.id, "category-export-site");
        let category = repository
            .create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "notes".into(),
                    title: "Notes".into(),
                    description: Some("Exported category metadata".into()),
                    theme_profile: Some(ThemeProfile::Forest),
                },
            )
            .unwrap();
        let empty_archived = repository
            .create_category(
                owner.id,
                site.id,
                CreateCategoryInput {
                    slug: "archive".into(),
                    title: "Empty archive".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        repository
            .archive_category(owner.id, site.id, empty_archived.id)
            .unwrap();
        let document = repository
            .create_document_in_writable_site_with_category(
                owner.id,
                new_document(site.id, "Categorized", "entry"),
                Some(category.id),
            )
            .unwrap();
        let root_revision = repository
            .revise_document_in_writable_site_with_category(
                owner.id,
                site.id,
                ProposedRevision {
                    document_id: document.id,
                    base_revision_id: document.current_revision_id,
                    title: "Moved to root".into(),
                    slug: "entry-root".into(),
                    source_markdown: "Placement history remains portable.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
                Some(None),
            )
            .unwrap();

        let export = repository.export_site(site.id).unwrap();
        assert_eq!(export.schema_version, "open-soverign-blog-export/4");
        assert_eq!(export.categories.len(), 2);
        assert!(export.categories.iter().any(|item| {
            item.id == empty_archived.id && item.status == CategoryStatus::Archived
        }));
        assert_eq!(export.documents.len(), 1);
        let placements = &export.documents[0].revision_category_placements;
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].category_id, Some(category.id));
        assert_eq!(placements[1].revision_id, root_revision.id);
        assert_eq!(placements[1].category_id, None);

        let encoded = serde_json::to_string(&export).unwrap();
        let decoded: SiteExport = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, export);
    }

    #[test]
    fn community_migration_users_and_hashed_sessions_are_safe_and_idempotent() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        repository.migrate().unwrap();
        repository.migrate().unwrap();
        let migration_count: i64 = repository
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version IN (1, 2, 3, 4)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migration_count, 4);

        let user = repository
            .create_user(
                "  OWNER@EXAMPLE.TEST ",
                "Owner-Blog",
                "Owner",
                "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
            )
            .unwrap();
        assert_eq!(user.email, "owner@example.test");
        assert_eq!(user.handle, "owner-blog");
        assert_eq!(
            repository.find_user_by_email("OWNER@EXAMPLE.TEST").unwrap(),
            user
        );
        assert_eq!(repository.get_user_by_handle("OWNER-BLOG").unwrap(), user);
        assert!(matches!(
            repository.create_user(
                "owner@example.test",
                "other",
                "Other",
                "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
            ),
            Err(RepositoryError::Validation(_))
        ));

        let hash = [0xabu8; 32];
        let session = repository
            .create_session(user.id, &hash, Utc::now() + chrono::Duration::hours(1))
            .unwrap();
        assert_eq!(repository.get_session(&hash).unwrap().id, session.id);
        let (storage_type, stored_length, stored_hash): (String, i64, Vec<u8>) = repository
            .lock()
            .unwrap()
            .query_row(
                "SELECT typeof(token_hash), length(token_hash), token_hash
                 FROM sessions WHERE id = ?1",
                params![session.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(storage_type, "blob");
        assert_eq!(stored_length, 32);
        assert_eq!(stored_hash, hash);
        assert!(repository.revoke_session(&hash).unwrap());
        assert!(matches!(
            repository.get_session(&hash),
            Err(RepositoryError::NotFound)
        ));
        assert!(!repository.revoke_session(&hash).unwrap());

        let expired_hash = [0xcdu8; 32];
        let expired = repository
            .create_session(
                user.id,
                &expired_hash,
                Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        repository
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET expires_at = ?1 WHERE id = ?2",
                params![
                    (Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
                    expired.id.to_string()
                ],
            )
            .unwrap();
        assert!(matches!(
            repository.get_session(&expired_hash),
            Err(RepositoryError::NotFound)
        ));
        assert!(matches!(
            repository.create_session(user.id, &[1, 2, 3], Utc::now() + chrono::Duration::hours(1)),
            Err(RepositoryError::Validation(_))
        ));
    }

    #[test]
    fn migration_six_is_additive_revokes_pre_binding_sessions_and_gates_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("schema-v5.db");
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch(MIGRATION_1).unwrap();
        connection.execute_batch(MIGRATION_2).unwrap();
        connection.execute_batch(MIGRATION_3).unwrap();
        connection.execute_batch(MIGRATION_4).unwrap();
        connection.execute_batch(MIGRATION_5).unwrap();
        let user_id = Uuid::now_v7();
        let session_id = Uuid::now_v7();
        let old_hash = [0x31_u8; 32];
        let now = Utc::now();
        connection
            .execute(
                "INSERT INTO users (
                    id, email, handle, display_name, password_phc, created_at, updated_at
                 ) VALUES (?1, 'v5@example.test', 'v5-user', 'V5 User',
                           '$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA', ?2, ?2)",
                params![user_id.to_string(), now.to_rfc3339()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions (
                    id, token_hash, user_id, expires_at, created_at, revoked_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                params![
                    session_id.to_string(),
                    old_hash,
                    user_id.to_string(),
                    (now + chrono::Duration::hours(1)).to_rfc3339(),
                    now.to_rfc3339(),
                ],
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            SqliteRepository::open_read_only(&database),
            Err(RepositoryError::Storage(_))
        ));
        let repository = SqliteRepository::open(&database).unwrap();
        repository.migrate().unwrap();
        assert!(matches!(
            repository.get_session(&old_hash),
            Err(RepositoryError::NotFound)
        ));
        let connection = repository.lock().unwrap();
        let migration_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version IN (6, 7)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migration_count, 2);
        let columns = connection
            .prepare("PRAGMA table_info(sessions)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "auth_epoch"));
        assert!(columns.iter().any(|column| column == "auth_method"));
        let admin_columns = connection
            .prepare("PRAGMA table_info(admin_control_plane)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            admin_columns
                .iter()
                .any(|column| column == "setup_complete")
        );
        drop(connection);
        drop(repository);

        let read_only = SqliteRepository::open_read_only(&database).unwrap();
        assert!(matches!(
            read_only.get_admin_control_plane(),
            Err(RepositoryError::NotFound)
        ));
    }

    #[test]
    fn fresh_primary_owner_provision_is_atomic_idempotent_and_split_brain_safe() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let bootstrap = primary_owner_bootstrap(site_id);
        let fingerprint = [0x41_u8; 32];
        let control = repository
            .provision_primary_owner_site(&bootstrap, AdminAuthMode::AccessKey, &fingerprint)
            .unwrap();
        assert_eq!(control.primary_site_id, site_id);
        assert_eq!(control.owner_user_id, site_id);
        assert_eq!(control.auth_mode, AdminAuthMode::AccessKey);
        assert_eq!(control.auth_epoch, 1);
        assert!(!control.setup_complete);
        assert_eq!(control.binding_fingerprint, fingerprint);
        let site = repository.get_site_by_id(site_id).unwrap();
        assert_eq!(site.owner_user_id, control.owner_user_id);
        assert_eq!(site.handle, "primary-blog");
        assert_eq!(site.theme_profile, ThemeProfile::Forest);

        assert_eq!(
            repository
                .provision_primary_owner_site(&bootstrap, AdminAuthMode::AccessKey, &fingerprint,)
                .unwrap(),
            control
        );
        assert!(matches!(
            repository.provision_primary_owner_site(
                &bootstrap,
                AdminAuthMode::External,
                &fingerprint,
            ),
            Err(RepositoryError::Validation(_))
        ));
        assert!(matches!(
            repository.reconcile_admin_control_plane(
                site_id,
                AdminAuthMode::AccessKey,
                &[0x42_u8; 32],
            ),
            Err(RepositoryError::Validation(_))
        ));
        let connection = repository.lock().unwrap();
        let counts: (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM admin_control_plane),
                    (SELECT COUNT(*) FROM sites),
                    (SELECT COUNT(*) FROM users),
                    (SELECT COUNT(*) FROM site_memberships WHERE role = 'owner')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(counts, (1, 1, 1, 1));
    }

    #[test]
    fn primary_owner_setup_is_owner_scoped_atomic_and_one_time() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let control = repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(site_id),
                AdminAuthMode::AccessKey,
                &[0x49_u8; 32],
            )
            .unwrap();
        assert!(!control.setup_complete);

        let stranger = community_user(&repository, "setup-stranger");
        assert!(matches!(
            repository.complete_primary_owner_setup(
                stranger.id,
                "finished-blog",
                "Finished Blog",
                Some("Ready for readers"),
                ThemeProfile::Terminal,
            ),
            Err(RepositoryError::NotFound)
        ));
        assert!(!repository.get_admin_control_plane().unwrap().setup_complete);

        let site = repository
            .complete_primary_owner_setup(
                control.owner_user_id,
                "finished-blog",
                "Finished Blog",
                Some("Ready for readers"),
                ThemeProfile::Terminal,
            )
            .unwrap();
        assert_eq!(site.id, site_id);
        assert_eq!(site.handle, "finished-blog");
        assert_eq!(site.title, "Finished Blog");
        assert_eq!(site.description.as_deref(), Some("Ready for readers"));
        assert_eq!(site.theme_profile, ThemeProfile::Terminal);
        assert_eq!(site.theme_revision, 2);
        assert!(repository.get_admin_control_plane().unwrap().setup_complete);

        assert!(matches!(
            repository.complete_primary_owner_setup(
                control.owner_user_id,
                "another-blog",
                "Another Blog",
                None,
                ThemeProfile::Ink,
            ),
            Err(RepositoryError::Validation(_))
        ));
        let connection = repository.lock().unwrap();
        let theme_revision_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM site_theme_revisions WHERE site_id = ?1",
                params![site_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(theme_revision_count, 2);
        assert_eq!(
            load_site_by_id(&connection, site_id, None).unwrap().handle,
            "finished-blog"
        );
    }

    #[test]
    fn access_key_and_external_auth_issue_the_same_scoped_owner_session_shape() {
        let access_repository = SqliteRepository::open_in_memory().unwrap();
        let access_site_id = Uuid::now_v7();
        access_repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(access_site_id),
                AdminAuthMode::AccessKey,
                &[0x51_u8; 32],
            )
            .unwrap();
        let access_hash = [0x52_u8; 32];
        let access_session = access_repository
            .create_primary_owner_session(
                &access_hash,
                Utc::now() + chrono::Duration::hours(1),
                SessionAuthMethod::AccessKey,
                &[0x51_u8; 32],
            )
            .unwrap();
        assert_eq!(access_session.auth_epoch, 1);
        assert_eq!(access_session.auth_method, SessionAuthMethod::AccessKey);
        let access = access_repository
            .get_primary_owner_session(&access_hash)
            .unwrap();
        assert_eq!(access.user.id, access.site.owner_user_id);
        assert_eq!(access.site.id, access_site_id);
        assert_eq!(
            access_repository.get_session(&access_hash).unwrap(),
            access_session
        );
        assert!(matches!(
            access_repository.create_primary_owner_session(
                &[0x53_u8; 32],
                Utc::now() + chrono::Duration::hours(1),
                SessionAuthMethod::External,
                &[0x51_u8; 32],
            ),
            Err(RepositoryError::Validation(_))
        ));

        access_repository
            .lock()
            .unwrap()
            .execute(
                "UPDATE admin_control_plane SET auth_epoch = auth_epoch + 1, updated_at = ?1
                 WHERE singleton = 1",
                params![Utc::now().to_rfc3339()],
            )
            .unwrap();
        assert!(matches!(
            access_repository.get_session(&access_hash),
            Err(RepositoryError::NotFound)
        ));
        assert!(matches!(
            access_repository.get_primary_owner_session(&access_hash),
            Err(RepositoryError::NotFound)
        ));

        let external_repository = SqliteRepository::open_in_memory().unwrap();
        let external_site_id = Uuid::now_v7();
        let external_control = external_repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(external_site_id),
                AdminAuthMode::External,
                &[0x61_u8; 32],
            )
            .unwrap();
        let subject_hash = [0x62_u8; 32];
        let identity = external_repository
            .bind_external_identity(
                "Firebase",
                "https://securetoken.google.com/example",
                &subject_hash,
                &[0x61_u8; 32],
            )
            .unwrap();
        assert_eq!(identity.adapter, "firebase");
        assert_eq!(identity.user_id, external_control.owner_user_id);
        assert_eq!(
            external_repository
                .bind_external_identity(
                    "firebase",
                    "https://securetoken.google.com/example",
                    &subject_hash,
                    &[0x61_u8; 32],
                )
                .unwrap()
                .user_id,
            identity.user_id
        );
        let external_hash = [0x63_u8; 32];
        let external_session = external_repository
            .create_primary_owner_session(
                &external_hash,
                Utc::now() + chrono::Duration::hours(1),
                SessionAuthMethod::External,
                &[0x61_u8; 32],
            )
            .unwrap();
        let external = external_repository
            .get_primary_owner_session(&external_hash)
            .unwrap();
        assert_eq!(external.user.id, external_control.owner_user_id);
        assert_eq!(external.site.id, external_site_id);
        assert_eq!(external.session, external_session);
        assert_eq!(external.session.auth_epoch, access.session.auth_epoch);
    }

    #[test]
    fn explicit_admin_rotation_is_atomic_idempotent_and_binding_scoped() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let old_fingerprint = [0x64_u8; 32];
        let new_fingerprint = [0x65_u8; 32];
        let disabled_fingerprint = [0x66_u8; 32];
        let subject_hash = [0x67_u8; 32];
        let control = repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(site_id),
                AdminAuthMode::External,
                &old_fingerprint,
            )
            .unwrap();
        repository
            .bind_external_identity(
                "oidc",
                "https://identity.example",
                &subject_hash,
                &old_fingerprint,
            )
            .unwrap();

        let old_admin_hash = [0x68_u8; 32];
        repository
            .create_primary_owner_session(
                &old_admin_hash,
                Utc::now() + chrono::Duration::hours(1),
                SessionAuthMethod::External,
                &old_fingerprint,
            )
            .unwrap();
        let member_hash = [0x69_u8; 32];
        repository
            .create_session(
                control.owner_user_id,
                &member_hash,
                Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();

        let rotated = repository
            .rotate_admin_control_plane(site_id, AdminAuthMode::External, &new_fingerprint)
            .unwrap();
        assert_eq!(rotated.auth_epoch, control.auth_epoch + 1);
        assert_eq!(rotated.binding_fingerprint, new_fingerprint);
        assert!(matches!(
            repository.get_primary_owner_session(&old_admin_hash),
            Err(RepositoryError::NotFound)
        ));
        assert!(repository.get_session(&member_hash).is_ok());
        assert!(matches!(
            repository.get_external_identity("oidc", "https://identity.example", &subject_hash,),
            Err(RepositoryError::NotFound)
        ));
        assert!(matches!(
            repository.bind_external_identity(
                "oidc",
                "https://identity.example",
                &subject_hash,
                &old_fingerprint,
            ),
            Err(RepositoryError::Validation(_))
        ));
        assert!(matches!(
            repository.create_primary_owner_session(
                &[0x6a_u8; 32],
                Utc::now() + chrono::Duration::hours(1),
                SessionAuthMethod::External,
                &old_fingerprint,
            ),
            Err(RepositoryError::Validation(_))
        ));

        repository
            .bind_external_identity(
                "oidc",
                "https://identity.example",
                &subject_hash,
                &new_fingerprint,
            )
            .unwrap();
        let new_admin_hash = [0x6b_u8; 32];
        repository
            .create_primary_owner_session(
                &new_admin_hash,
                Utc::now() + chrono::Duration::hours(1),
                SessionAuthMethod::External,
                &new_fingerprint,
            )
            .unwrap();
        let repeated = repository
            .rotate_admin_control_plane(site_id, AdminAuthMode::External, &new_fingerprint)
            .unwrap();
        assert_eq!(repeated, rotated);
        assert!(
            repository
                .get_primary_owner_session(&new_admin_hash)
                .is_ok()
        );

        let disabled = repository
            .rotate_admin_control_plane(site_id, AdminAuthMode::Disabled, &disabled_fingerprint)
            .unwrap();
        assert_eq!(disabled.auth_epoch, rotated.auth_epoch + 1);
        assert_eq!(disabled.auth_mode, AdminAuthMode::Disabled);
        assert!(matches!(
            repository.get_primary_owner_session(&new_admin_hash),
            Err(RepositoryError::NotFound)
        ));
        assert!(repository.get_session(&member_hash).is_ok());
    }

    #[test]
    fn existing_site_reconciliation_preserves_membership_owned_authority() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "reconciled-owner");
        let site = community_site(&repository, owner.id, "reconciled-site");
        let control = repository
            .reconcile_admin_control_plane(site.id, AdminAuthMode::External, &[0x71_u8; 32])
            .unwrap();
        assert_eq!(control.owner_user_id, owner.id);
        assert_eq!(control.primary_site_id, site.id);
        assert!(control.setup_complete);
        assert_eq!(repository.get_admin_control_plane().unwrap(), control);
    }

    #[test]
    fn concurrent_conflicting_owner_bootstraps_do_not_overwrite_the_winner() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("owner-race.db");
        drop(SqliteRepository::open(&database).unwrap());
        let first = SqliteRepository::open(&database).unwrap();
        let second = SqliteRepository::open(&database).unwrap();
        let site_id = Uuid::now_v7();
        let bootstrap = primary_owner_bootstrap(site_id);
        let barrier = Arc::new(Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let first_bootstrap = bootstrap.clone();
        let first_thread = thread::spawn(move || {
            first_barrier.wait();
            first.provision_primary_owner_site(
                &first_bootstrap,
                AdminAuthMode::AccessKey,
                &[0x81_u8; 32],
            )
        });
        let second_barrier = Arc::clone(&barrier);
        let second_thread = thread::spawn(move || {
            second_barrier.wait();
            second.provision_primary_owner_site(
                &bootstrap,
                AdminAuthMode::AccessKey,
                &[0x82_u8; 32],
            )
        });
        let results = [first_thread.join().unwrap(), second_thread.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(RepositoryError::Validation(_))))
                .count(),
            1
        );

        let repository = SqliteRepository::open(&database).unwrap();
        let control = repository.get_admin_control_plane().unwrap();
        assert!(
            control.binding_fingerprint == [0x81_u8; 32]
                || control.binding_fingerprint == [0x82_u8; 32]
        );
        let counts: (i64, i64, i64) = repository
            .lock()
            .unwrap()
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM admin_control_plane),
                    (SELECT COUNT(*) FROM sites),
                    (SELECT COUNT(*) FROM site_memberships WHERE role = 'owner')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(counts, (1, 1, 1));
    }

    #[test]
    fn sites_have_one_owner_and_immutable_validated_appearance_revisions() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "theme-owner");
        let stranger = community_user(&repository, "theme-stranger");
        let site = repository
            .create_site(
                owner.id,
                "THEME-SITE",
                "Theme site",
                None,
                ThemeProfile::Forest,
            )
            .unwrap();
        assert_eq!(site.handle, "theme-site");
        assert_eq!(site.owner_user_id, owner.id);
        assert_eq!(site.theme_profile, ThemeProfile::Forest);
        assert_eq!(site.theme_revision, 1);
        assert_eq!(site.custom_css, None);
        assert!(repository.owns_site(owner.id, site.id).unwrap());
        assert!(!repository.owns_site(stranger.id, site.id).unwrap());
        assert!(matches!(
            repository.get_owned_site(stranger.id, site.id),
            Err(RepositoryError::NotFound)
        ));

        let changed = repository
            .change_site_theme(owner.id, site.id, ThemeProfile::Terminal)
            .unwrap();
        assert_eq!(changed.theme_profile, ThemeProfile::Terminal);
        assert_eq!(changed.theme_revision, 2);
        assert_eq!(changed.custom_css, None);
        let styled = repository
            .change_site_appearance(
                owner.id,
                site.id,
                ThemeProfile::Ink,
                Some(".article-content { color: rebeccapurple; }"),
            )
            .unwrap();
        assert_eq!(styled.theme_revision, 3);
        assert_eq!(
            styled.custom_css.as_deref(),
            Some(".article-content { color: rebeccapurple; }")
        );
        assert_eq!(
            repository.list_owned_sites(owner.id, 10).unwrap(),
            vec![styled]
        );
        assert!(
            repository
                .lock()
                .unwrap()
                .execute(
                    "UPDATE site_theme_revisions SET profile = 'ink'
                     WHERE site_id = ?1 AND revision = 1",
                    params![site.id.to_string()],
                )
                .is_err()
        );

        let theme_columns = {
            let connection = repository.lock().unwrap();
            let mut statement = connection
                .prepare("PRAGMA table_info(site_theme_revisions)")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(theme_columns.iter().any(|column| column == "custom_css"));
        for unsafe_css in [
            "@import 'https://example.test/theme.css';",
            ".post { background: url(https://example.test/pixel); }",
            ".post { background: image-set('https://example.test/a.png'); }",
            "</style><script>alert(1)</script>",
            ".post { color: red; \\75rl(https://example.test); }",
            ".post { color: red;",
            ".post { color: red; }} .escape { display: block; }",
            ".post { content: 'unterminated; }",
            ".post { color: red; /* unterminated",
        ] {
            assert!(matches!(
                repository.change_site_appearance(
                    owner.id,
                    site.id,
                    ThemeProfile::Paper,
                    Some(unsafe_css)
                ),
                Err(RepositoryError::Validation(_))
            ));
        }
        let oversized = "a".repeat(64 * 1_024 + 1);
        assert!(matches!(
            repository.change_site_appearance(
                owner.id,
                site.id,
                ThemeProfile::Paper,
                Some(&oversized)
            ),
            Err(RepositoryError::Validation(_))
        ));
        assert!(
            validate_custom_css(Some(
                ".post { content: \"}\"; /* ignored { brace */ color: purple; }"
            ))
            .is_ok()
        );
    }

    #[test]
    fn theme_change_preserves_css_committed_while_waiting_for_the_write_lock() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("appearance-race.sqlite3");
        let setup = SqliteRepository::open(&database).unwrap();
        let owner = community_user(&setup, "appearance-race-owner");
        let site = community_site(&setup, owner.id, "appearance-race-site");
        drop(setup);

        let writer = SqliteRepository::open(&database).unwrap();
        let theme_writer = SqliteRepository::open(&database).unwrap();
        let (write_started, write_started_rx) = mpsc::channel();
        let (allow_commit, wait_for_commit) = mpsc::channel();
        let owner_id = owner.id;
        let site_id = site.id;
        let css_commit = thread::spawn(move || {
            let mut connection = writer.lock().unwrap();
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            append_site_appearance_revision(
                &transaction,
                owner_id,
                site_id,
                ThemeProfile::Paper,
                Some(".article-content { color: rebeccapurple; }".into()),
            )
            .unwrap();
            write_started.send(()).unwrap();
            wait_for_commit.recv().unwrap();
            transaction.commit().unwrap();
        });

        write_started_rx.recv().unwrap();
        let theme_change = thread::spawn(move || {
            theme_writer
                .change_site_theme(owner_id, site_id, ThemeProfile::Terminal)
                .unwrap()
        });
        // Give the competing operation time to reach SQLite's held write lock.
        // The appearance implementation must acquire that lock before reading
        // the CSS value it intends to preserve.
        thread::sleep(Duration::from_millis(150));
        allow_commit.send(()).unwrap();
        css_commit.join().unwrap();
        let changed = theme_change.join().unwrap();

        assert_eq!(changed.theme_profile, ThemeProfile::Terminal);
        assert_eq!(
            changed.custom_css.as_deref(),
            Some(".article-content { color: rebeccapurple; }")
        );
    }

    #[test]
    fn collaborators_can_draft_but_owner_alone_can_publish_and_manage_memberships() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "collab-owner");
        let writer = community_user(&repository, "collab-writer");
        let editor = community_user(&repository, "collab-editor");
        let outsider = community_user(&repository, "collab-outsider");
        let site = community_site(&repository, owner.id, "collab-site");
        let other_site = community_site(&repository, outsider.id, "other-collab-site");

        let invited_writer = repository
            .add_site_collaborator(owner.id, site.id, &writer.email, SiteMembershipRole::Writer)
            .unwrap();
        assert_eq!(invited_writer.role, SiteMembershipRole::Writer);
        repository
            .add_site_collaborator(owner.id, site.id, &editor.email, SiteMembershipRole::Editor)
            .unwrap();
        assert!(matches!(
            repository.add_site_collaborator(
                owner.id,
                site.id,
                &outsider.email,
                SiteMembershipRole::Owner
            ),
            Err(RepositoryError::Validation(_))
        ));
        assert_eq!(
            repository
                .list_site_memberships(owner.id, site.id, 10)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            repository.list_accessible_sites(writer.id, 10).unwrap(),
            vec![site.clone()]
        );

        let document = repository
            .create_document_in_writable_site(
                writer.id,
                new_document(site.id, "Writer draft", "writer-draft"),
            )
            .unwrap();
        let revision = repository
            .revise_document_in_writable_site(
                editor.id,
                site.id,
                ProposedRevision {
                    document_id: document.id,
                    base_revision_id: document.current_revision_id,
                    title: "Editor revision".into(),
                    slug: "editor-revision".into(),
                    source_markdown: "Collaborative draft only.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
            )
            .unwrap();
        assert!(matches!(
            repository.publish_document_in_owned_site(writer.id, site.id, document.id, revision.id),
            Err(RepositoryError::NotFound)
        ));
        assert!(matches!(
            repository.get_document_in_writable_site(writer.id, other_site.id, document.id),
            Err(RepositoryError::NotFound)
        ));
        let published = repository
            .publish_document_in_owned_site(owner.id, site.id, document.id, revision.id)
            .unwrap();
        assert_eq!(published.published_revision_id, Some(revision.id));

        assert!(matches!(
            repository.remove_site_collaborator(owner.id, site.id, owner.id),
            Err(RepositoryError::Validation(_))
        ));
        let removed = repository
            .remove_site_collaborator(owner.id, site.id, writer.id)
            .unwrap();
        assert_eq!(removed.role, SiteMembershipRole::Writer);
        assert!(matches!(
            repository.get_document_in_writable_site(writer.id, site.id, document.id),
            Err(RepositoryError::NotFound)
        ));
    }

    #[test]
    fn legacy_site_handles_use_the_complete_uuid_identity() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let first = Uuid::parse_str("aaaaaaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let second = Uuid::parse_str("aaaaaaaa-aaaa-7aaa-8aaa-aaaaaaaabbbb").unwrap();
        let first = repository.ensure_legacy_site(first).unwrap();
        let second = repository.ensure_legacy_site(second).unwrap();
        assert_ne!(first.handle, second.handle);
        assert_eq!(first.handle.len(), 39);
        assert_eq!(second.handle.len(), 39);
    }

    #[test]
    fn owned_document_methods_do_not_cross_site_or_owner_boundaries() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let first_owner = community_user(&repository, "first-owner");
        let second_owner = community_user(&repository, "second-owner");
        let first_site = community_site(&repository, first_owner.id, "first-site");
        let second_site = community_site(&repository, second_owner.id, "second-site");
        let document = repository
            .create_document_in_owned_site(
                first_owner.id,
                new_document(first_site.id, "Private draft", "private-draft"),
            )
            .unwrap();

        assert!(matches!(
            repository.get_document_in_owned_site(second_owner.id, first_site.id, document.id),
            Err(RepositoryError::NotFound)
        ));
        assert!(matches!(
            repository.get_document_in_owned_site(second_owner.id, second_site.id, document.id),
            Err(RepositoryError::NotFound)
        ));
        let cross_tenant_revision = ProposedRevision {
            document_id: document.id,
            base_revision_id: document.current_revision_id,
            title: "Stolen".into(),
            slug: "stolen".into(),
            source_markdown: "must not persist".into(),
            embeds: vec![],
            intent: None,
            ontology: None,
            ai_summary: None,
            authorship: Default::default(),
            actor: actor(),
            idempotency_key: None,
        };
        assert!(matches!(
            repository.revise_document_in_owned_site(
                second_owner.id,
                second_site.id,
                cross_tenant_revision
            ),
            Err(RepositoryError::NotFound)
        ));
        assert_eq!(repository.list_revisions(document.id, 10).unwrap().len(), 1);
        assert!(matches!(
            repository.publish_document_in_owned_site(
                second_owner.id,
                second_site.id,
                document.id,
                document.current_revision_id
            ),
            Err(RepositoryError::NotFound)
        ));
        assert_eq!(
            repository
                .list_documents_in_owned_site(first_owner.id, first_site.id, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn malformed_cross_tenant_routes_and_revision_pointers_are_not_trusted() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let first_owner = community_user(&repository, "pointer-owner-one");
        let second_owner = community_user(&repository, "pointer-owner-two");
        let first_site = community_site(&repository, first_owner.id, "pointer-site-one");
        let second_site = community_site(&repository, second_owner.id, "pointer-site-two");
        let first = repository
            .create_document_in_owned_site(
                first_owner.id,
                new_document(first_site.id, "First", "first"),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                first_owner.id,
                first_site.id,
                first.id,
                first.current_revision_id,
            )
            .unwrap();
        let second = repository
            .create_document_in_owned_site(
                second_owner.id,
                new_document(second_site.id, "Second", "second"),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                second_owner.id,
                second_site.id,
                second.id,
                second.current_revision_id,
            )
            .unwrap();

        let connection = repository.lock().unwrap();
        connection
            .execute(
                "INSERT INTO routes (site_id, path, document_id, is_canonical, created_at)
                 VALUES (?1, 'poisoned-route', ?2, 0, ?3)",
                params![
                    second_site.id.to_string(),
                    first.id.to_string(),
                    Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE documents SET published_revision_id = ?1 WHERE id = ?2",
                params![first.current_revision_id.to_string(), second.id.to_string()],
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            repository.get_published_by_slug(second_site.id, "poisoned-route"),
            Err(RepositoryError::NotFound)
        ));
        assert!(matches!(
            repository.get_published_document_by_id(second.id),
            Err(RepositoryError::NotFound)
        ));
    }

    #[test]
    fn published_revision_stays_public_during_drafting_and_comments_are_scoped() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "post-owner");
        let commenter = community_user(&repository, "commenter");
        let site = community_site(&repository, owner.id, "post-site");
        let other_site = community_site(&repository, commenter.id, "other-site");
        let document = repository
            .create_document_in_owned_site(
                owner.id,
                new_document(site.id, "Published", "published"),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                owner.id,
                site.id,
                document.id,
                document.current_revision_id,
            )
            .unwrap();

        let draft = repository
            .revise_document_in_owned_site(
                owner.id,
                site.id,
                ProposedRevision {
                    document_id: document.id,
                    base_revision_id: document.current_revision_id,
                    title: "Unpublished rewrite".into(),
                    slug: "unpublished-rewrite".into(),
                    source_markdown: "This is still private".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: Some("private-rewrite".into()),
                },
            )
            .unwrap();
        let administered = repository
            .get_document_in_owned_site(owner.id, site.id, document.id)
            .unwrap();
        assert_eq!(administered.current_revision_id, draft.id);
        assert_eq!(administered.status, DocumentStatus::Published);
        let public = repository
            .get_published_document_by_id(document.id)
            .unwrap();
        assert_eq!(public.revision.title, "Published");
        assert_eq!(
            repository
                .get_published_by_slug(site.id, "published")
                .unwrap()
                .revision
                .title,
            "Published"
        );
        assert_eq!(repository.list_published_across_sites(10).unwrap().len(), 1);

        let comment = repository
            .create_comment(commenter.id, site.id, document.id, "  Great post!  ")
            .unwrap();
        assert_eq!(comment.status, CommentStatus::Approved);
        assert_eq!(comment.source_markdown, "Great post!");
        assert_eq!(
            repository
                .list_approved_comments(site.id, document.id, 10)
                .unwrap(),
            vec![comment]
        );
        assert!(matches!(
            repository.create_comment(commenter.id, other_site.id, document.id, "wrong scope"),
            Err(RepositoryError::NotFound)
        ));

        let unpublished = repository
            .create_document_in_owned_site(
                owner.id,
                new_document(site.id, "Draft only", "draft-only"),
            )
            .unwrap();
        assert!(matches!(
            repository.create_comment(commenter.id, site.id, unpublished.id, "too early"),
            Err(RepositoryError::NotFound)
        ));

        repository
            .lock()
            .unwrap()
            .execute(
                "UPDATE documents SET status = 'archived' WHERE id = ?1",
                params![document.id.to_string()],
            )
            .unwrap();
        assert!(matches!(
            repository.get_published_document_by_id(document.id),
            Err(RepositoryError::NotFound)
        ));
        assert!(
            repository
                .list_published_across_sites(10)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            repository.list_approved_comments(site.id, document.id, 10),
            Err(RepositoryError::NotFound)
        ));
    }

    #[test]
    fn cross_site_public_feed_orders_by_published_revision_time() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "feed-owner");
        let site = community_site(&repository, owner.id, "feed-site");
        let first = repository
            .create_document_in_owned_site(owner.id, new_document(site.id, "First", "first-feed"))
            .unwrap();
        let second = repository
            .create_document_in_owned_site(owner.id, new_document(site.id, "Second", "second-feed"))
            .unwrap();
        repository
            .publish_document_in_owned_site(owner.id, site.id, first.id, first.current_revision_id)
            .unwrap();
        repository
            .publish_document_in_owned_site(
                owner.id,
                site.id,
                second.id,
                second.current_revision_id,
            )
            .unwrap();

        let connection = repository.lock().unwrap();
        connection
            .execute(
                "UPDATE revisions SET created_at = '2026-01-01T00:00:00+00:00'
                 WHERE id = ?1",
                params![first.current_revision_id.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE revisions SET created_at = '2026-02-01T00:00:00+00:00'
                 WHERE id = ?1",
                params![second.current_revision_id.to_string()],
            )
            .unwrap();
        // Deliberately reverse document update time. Public ordering must not
        // use this mutable control-plane timestamp.
        connection
            .execute(
                "UPDATE documents SET updated_at = '2030-01-01T00:00:00+00:00'
                 WHERE id = ?1",
                params![first.id.to_string()],
            )
            .unwrap();
        drop(connection);

        let feed = repository.list_published_across_sites(10).unwrap();
        assert_eq!(
            feed.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![second.id, first.id]
        );
        assert_eq!(
            feed[0].updated_at,
            parse_datetime("2026-02-01T00:00:00+00:00").unwrap()
        );
    }

    #[test]
    fn series_promotion_is_idempotent_ordered_reorderable_and_exported() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let control = repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(site_id),
                AdminAuthMode::AccessKey,
                &[0x51; 32],
            )
            .unwrap();
        let category = repository
            .create_category(
                control.owner_user_id,
                site_id,
                CreateCategoryInput {
                    slug: "yangja".into(),
                    title: "yangja".into(),
                    description: Some("양자 컴퓨팅".into()),
                    theme_profile: None,
                },
            )
            .unwrap();
        let publish = |title: &str, slug: &str| {
            let document = repository
                .create_document_in_writable_site_with_category(
                    control.owner_user_id,
                    new_document(site_id, title, slug),
                    Some(category.id),
                )
                .unwrap();
            repository
                .publish_document_in_owned_site(
                    control.owner_user_id,
                    site_id,
                    document.id,
                    document.current_revision_id,
                )
                .unwrap()
        };
        let first = publish("First", "first");
        let second = publish("Second", "second");

        let promoted = repository
            .promote_category_to_series(control.owner_user_id, site_id, category.id)
            .unwrap();
        assert_eq!(
            repository
                .promote_category_to_series(control.owner_user_id, site_id, category.id)
                .unwrap(),
            promoted,
            "an exact promotion retry must not create another series or reorder it"
        );
        assert_eq!(
            repository.get_series_by_id(site_id, promoted.id).unwrap(),
            promoted
        );
        assert_eq!(
            repository.get_series_by_slug(site_id, "YANGJA").unwrap(),
            promoted
        );
        assert_eq!(
            repository
                .get_series_by_category_id(site_id, category.id)
                .unwrap(),
            promoted
        );
        assert_eq!(
            repository
                .list_published_in_series(site_id, promoted.id, 500)
                .unwrap()
                .into_iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            vec![first.id, second.id]
        );

        let third = publish("Third", "third");
        assert_eq!(
            repository
                .list_published_in_series(site_id, promoted.id, 500)
                .unwrap()
                .into_iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            vec![first.id, second.id, third.id],
            "publication appends a new member to the series tail"
        );

        let reordered = [third.id, first.id, second.id];
        let items = repository
            .replace_series_order(control.owner_user_id, site_id, promoted.id, &reordered)
            .unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.document_id)
                .collect::<Vec<_>>(),
            reordered
        );
        assert_eq!(
            items.iter().map(|item| item.position).collect::<Vec<_>>(),
            vec![1_024, 2_048, 3_072]
        );
        assert_eq!(
            repository
                .list_published_in_series(site_id, promoted.id, 500)
                .unwrap()
                .into_iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            reordered
        );
        assert!(matches!(
            repository.replace_series_order(
                control.owner_user_id,
                site_id,
                promoted.id,
                &[first.id, second.id]
            ),
            Err(RepositoryError::RevisionConflict)
        ));
        assert!(matches!(
            repository.replace_series_order(
                control.owner_user_id,
                site_id,
                promoted.id,
                &[first.id, first.id, third.id]
            ),
            Err(RepositoryError::Validation(_))
        ));
        assert_eq!(
            repository
                .list_published_in_series(site_id, promoted.id, 500)
                .unwrap()
                .into_iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            reordered,
            "failed reorder requests must leave the committed order untouched"
        );

        let updated = repository
            .update_series(
                control.owner_user_id,
                site_id,
                promoted.id,
                UpdateCategoryInput {
                    title: "Quantum notes".into(),
                    description: Some("Ordered quantum notes".into()),
                    theme_profile: Some(ThemeProfile::Ink),
                },
            )
            .unwrap();
        assert_eq!(updated.slug, "yangja");
        assert_eq!(updated.title, "Quantum notes");
        assert_eq!(updated.theme_profile, Some(ThemeProfile::Ink));
        assert_eq!(
            repository
                .get_category_by_id(site_id, category.id)
                .unwrap()
                .title,
            "Quantum notes"
        );

        let home = repository.home_feed(site_id, 500).unwrap();
        assert!(home.category_sections.is_empty());
        assert_eq!(home.series_sections.len(), 1);
        assert_eq!(home.series_sections[0].series.id, promoted.id);
        assert_eq!(
            home.series_sections[0]
                .items
                .iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            reordered
        );

        let export = repository.export_site(site_id).unwrap();
        assert_eq!(export.schema_version, "open-soverign-blog-export/4");
        assert_eq!(export.series, vec![updated.clone()]);
        assert_eq!(
            export
                .series_items
                .iter()
                .map(|item| item.document_id)
                .collect::<Vec<_>>(),
            reordered
        );
        let encoded = serde_json::to_string(&export).unwrap();
        assert_eq!(
            serde_json::from_str::<SiteExport>(&encoded).unwrap(),
            export
        );

        let archived = repository
            .archive_series(control.owner_user_id, site_id, promoted.id)
            .unwrap();
        assert_eq!(archived.status, CategoryStatus::Archived);
        assert!(
            repository
                .list_series(site_id, false, 500)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repository
                .list_published_in_series(site_id, promoted.id, 500)
                .unwrap()
                .len(),
            3,
            "archiving retains the historical public collection"
        );
        assert!(matches!(
            repository.create_document_in_writable_site_with_category(
                control.owner_user_id,
                new_document(site_id, "Rejected", "rejected"),
                Some(category.id),
            ),
            Err(RepositoryError::Validation(_))
        ));
    }

    #[test]
    fn series_membership_follows_only_the_exact_published_revision() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "series-revision-owner");
        let site = community_site(&repository, owner.id, "series-revision-site");
        let first_series = repository
            .create_series(
                owner.id,
                site.id,
                CreateSeriesInput {
                    slug: "first-series".into(),
                    title: "First series".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let second_series = repository
            .create_series(
                owner.id,
                site.id,
                CreateSeriesInput {
                    slug: "second-series".into(),
                    title: "Second series".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let draft = repository
            .create_document_in_writable_site_with_category(
                owner.id,
                new_document(site.id, "Series post", "series-post"),
                Some(first_series.category_id),
            )
            .unwrap();
        let first_revision_id = draft.current_revision_id;
        repository
            .publish_document_in_owned_site(owner.id, site.id, draft.id, first_revision_id)
            .unwrap();
        let moved = repository
            .revise_document_in_writable_site_with_category(
                owner.id,
                site.id,
                ProposedRevision {
                    document_id: draft.id,
                    base_revision_id: draft.current_revision_id,
                    title: "Series post moved".into(),
                    slug: "series-post".into(),
                    source_markdown: "A private move must not affect readers.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
                Some(Some(second_series.category_id)),
            )
            .unwrap();
        assert_eq!(
            repository
                .list_published_in_series(site.id, first_series.id, 500)
                .unwrap()
                .iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            vec![draft.id]
        );
        assert!(
            repository
                .list_published_in_series(site.id, second_series.id, 500)
                .unwrap()
                .is_empty(),
            "the current private revision must not leak into series delivery"
        );

        repository
            .publish_document_in_owned_site(owner.id, site.id, draft.id, moved.id)
            .unwrap();
        assert!(
            repository
                .list_published_in_series(site.id, first_series.id, 500)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repository
                .list_published_in_series(site.id, second_series.id, 500)
                .unwrap()[0]
                .id,
            draft.id
        );

        repository
            .publish_document_in_owned_site(owner.id, site.id, draft.id, first_revision_id)
            .unwrap();
        assert_eq!(
            repository
                .list_published_in_series(site.id, first_series.id, 500)
                .unwrap()[0]
                .id,
            draft.id,
            "publishing historical content can reuse its retained series item"
        );
        assert!(
            repository
                .list_published_in_series(site.id, second_series.id, 500)
                .unwrap()
                .is_empty()
        );
        let export = repository.export_site(site.id).unwrap();
        assert_eq!(export.series.len(), 2);
        assert_eq!(
            export
                .series_items
                .iter()
                .filter(|item| item.document_id == draft.id)
                .count(),
            2,
            "historical memberships are portable and public filtering remains revision-scoped"
        );
    }

    #[test]
    fn global_home_curation_is_atomic_bounded_and_excludes_pins_from_recent() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let control = repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(site_id),
                AdminAuthMode::AccessKey,
                &[7; 32],
            )
            .unwrap();
        let mut published = Vec::new();
        for index in 0..4 {
            let document = repository
                .create_document_in_owned_site(
                    control.owner_user_id,
                    new_document(site_id, &format!("Post {index}"), &format!("post-{index}")),
                )
                .unwrap();
            repository
                .publish_document_in_owned_site(
                    control.owner_user_id,
                    site_id,
                    document.id,
                    document.current_revision_id,
                )
                .unwrap();
            published.push(document.id);
        }

        let ordered = [
            HomePinTarget::Post { id: published[1] },
            HomePinTarget::Post { id: published[0] },
            HomePinTarget::Post { id: published[2] },
        ];
        let pins = repository
            .replace_home_pins(control.owner_user_id, &ordered)
            .unwrap();
        assert_eq!(
            pins.iter().map(|pin| pin.target).collect::<Vec<_>>(),
            ordered
        );
        let home = repository.home_feed(site_id, 100).unwrap();
        assert_eq!(
            home.pinned.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![published[1], published[0], published[2]]
        );
        assert_eq!(home.recent.len(), 1);
        assert_eq!(home.recent[0].id, published[3]);
        assert!(home.category_sections.is_empty());

        assert!(matches!(
            repository.replace_home_pins(
                control.owner_user_id,
                &[
                    HomePinTarget::Post { id: published[0] },
                    HomePinTarget::Post { id: published[0] },
                ],
            ),
            Err(RepositoryError::Validation(_))
        ));
        assert_eq!(repository.list_home_pins().unwrap(), pins);

        let draft = repository
            .create_document_in_owned_site(
                control.owner_user_id,
                new_document(site_id, "Draft", "draft-only"),
            )
            .unwrap();
        assert!(matches!(
            repository.replace_home_pins(
                control.owner_user_id,
                &[HomePinTarget::Post { id: draft.id }],
            ),
            Err(RepositoryError::Validation(_))
        ));
        assert_eq!(repository.list_home_pins().unwrap(), pins);
    }

    #[test]
    fn typed_home_pins_share_three_slots_and_legacy_series_members_normalize() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let control = repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(site_id),
                AdminAuthMode::AccessKey,
                &[17; 32],
            )
            .unwrap();
        let series = repository
            .create_series(
                control.owner_user_id,
                site_id,
                CreateSeriesInput {
                    slug: "ordered-notes".into(),
                    title: "Ordered notes".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let series_post = repository
            .create_document_in_writable_site_with_category(
                control.owner_user_id,
                new_document(site_id, "Series entry", "series-entry"),
                Some(series.category_id),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                control.owner_user_id,
                site_id,
                series_post.id,
                series_post.current_revision_id,
            )
            .unwrap();
        let mut standalone = Vec::new();
        for index in 0..3 {
            let document = repository
                .create_document_in_owned_site(
                    control.owner_user_id,
                    new_document(
                        site_id,
                        &format!("Standalone {index}"),
                        &format!("standalone-{index}"),
                    ),
                )
                .unwrap();
            repository
                .publish_document_in_owned_site(
                    control.owner_user_id,
                    site_id,
                    document.id,
                    document.current_revision_id,
                )
                .unwrap();
            standalone.push(document.id);
        }

        assert!(matches!(
            repository.replace_home_pins(
                control.owner_user_id,
                &[HomePinTarget::Post { id: series_post.id }],
            ),
            Err(RepositoryError::Validation(message))
                if message.contains("must be pinned through its series")
        ));
        let normalized = repository
            .replace_legacy_home_document_pins(
                control.owner_user_id,
                &[series_post.id, standalone[0], standalone[1]],
            )
            .unwrap();
        assert_eq!(
            normalized.iter().map(|pin| pin.target).collect::<Vec<_>>(),
            vec![
                HomePinTarget::Series { id: series.id },
                HomePinTarget::Post { id: standalone[0] },
                HomePinTarget::Post { id: standalone[1] },
            ]
        );
        assert!(matches!(
            repository.replace_home_pins(
                control.owner_user_id,
                &[
                    HomePinTarget::Series { id: series.id },
                    HomePinTarget::Post { id: standalone[0] },
                    HomePinTarget::Post { id: standalone[1] },
                    HomePinTarget::Post { id: standalone[2] },
                ],
            ),
            Err(RepositoryError::Validation(_))
        ));

        let home = repository.home_feed(site_id, 100).unwrap();
        assert!(matches!(
            home.units.first(),
            Some(HomeUnitRecords::Series(section)) if section.series.id == series.id
        ));
        assert!(matches!(
            home.units.get(1),
            Some(HomeUnitRecords::Post(document)) if document.id == standalone[0]
        ));
        assert!(matches!(
            home.units.get(2),
            Some(HomeUnitRecords::Post(document)) if document.id == standalone[1]
        ));
    }

    #[test]
    fn home_pins_canonicalize_and_compact_when_posts_enter_a_series() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let control = repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(site_id),
                AdminAuthMode::AccessKey,
                &[18; 32],
            )
            .unwrap();
        let category = repository
            .create_category(
                control.owner_user_id,
                site_id,
                CreateCategoryInput {
                    slug: "research-notes".into(),
                    title: "Research notes".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let publish = |title: &str, slug: &str, category_id: Option<Uuid>| {
            let document = repository
                .create_document_in_writable_site_with_category(
                    control.owner_user_id,
                    new_document(site_id, title, slug),
                    category_id,
                )
                .unwrap();
            repository
                .publish_document_in_owned_site(
                    control.owner_user_id,
                    site_id,
                    document.id,
                    document.current_revision_id,
                )
                .unwrap()
        };
        let category_first = publish("Research one", "research-one", Some(category.id));
        let category_second = publish("Research two", "research-two", Some(category.id));
        let keep = publish("Standalone keeper", "standalone-keeper", None);
        repository
            .replace_home_pins(
                control.owner_user_id,
                &[
                    HomePinTarget::Post {
                        id: category_first.id,
                    },
                    HomePinTarget::Post { id: keep.id },
                    HomePinTarget::Post {
                        id: category_second.id,
                    },
                ],
            )
            .unwrap();

        let series = repository
            .promote_category_to_series(control.owner_user_id, site_id, category.id)
            .unwrap();
        {
            let connection = repository.lock().unwrap();
            let raw = load_home_pins(&connection).unwrap();
            assert_eq!(
                raw.iter()
                    .map(|pin| (pin.slot, pin.target))
                    .collect::<Vec<_>>(),
                vec![
                    (1, HomePinTarget::Series { id: series.id }),
                    (2, HomePinTarget::Post { id: keep.id }),
                ],
                "promotion must durably canonicalize both category posts to one Series slot"
            );
        }

        let moving = publish("Moving note", "moving-note", None);
        repository
            .replace_home_pins(
                control.owner_user_id,
                &[
                    HomePinTarget::Post { id: moving.id },
                    HomePinTarget::Series { id: series.id },
                    HomePinTarget::Post { id: keep.id },
                ],
            )
            .unwrap();
        let moved_revision = repository
            .revise_document_in_writable_site_with_category(
                control.owner_user_id,
                site_id,
                ProposedRevision {
                    document_id: moving.id,
                    base_revision_id: moving.current_revision_id,
                    title: moving.revision.title.clone(),
                    slug: moving.revision.slug.clone(),
                    source_markdown: "Now part of the ordered research Series.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
                Some(Some(category.id)),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                control.owner_user_id,
                site_id,
                moving.id,
                moved_revision.id,
            )
            .unwrap();

        let canonical = repository.list_home_pins().unwrap();
        assert_eq!(
            canonical
                .iter()
                .map(|pin| (pin.slot, pin.target))
                .collect::<Vec<_>>(),
            vec![
                (1, HomePinTarget::Series { id: series.id }),
                (2, HomePinTarget::Post { id: keep.id }),
            ],
            "republishing a pinned post into an already-pinned Series keeps the first slot and compacts survivors"
        );
        let changes_before_home = repository.lock().unwrap().total_changes();
        let home = repository.home_feed(site_id, 100).unwrap();
        assert_eq!(
            repository.lock().unwrap().total_changes(),
            changes_before_home,
            "anonymous home projection must not start or commit a write transaction"
        );
        assert!(matches!(
            home.units.first(),
            Some(HomeUnitRecords::Series(section)) if section.series.id == series.id
        ));
        assert!(matches!(
            home.units.get(1),
            Some(HomeUnitRecords::Post(document)) if document.id == keep.id
        ));
    }

    #[test]
    fn every_nonempty_series_gets_a_bounded_home_unit_after_a_large_first_series() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let control = repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(site_id),
                AdminAuthMode::AccessKey,
                &[19; 32],
            )
            .unwrap();
        let create_series = |slug: &str, title: &str| {
            repository
                .create_series(
                    control.owner_user_id,
                    site_id,
                    CreateSeriesInput {
                        slug: slug.into(),
                        title: title.into(),
                        description: None,
                        theme_profile: None,
                    },
                )
                .unwrap()
        };
        let first = create_series("first-series", "First series");
        let second = create_series("second-series", "Second series");
        let third = create_series("third-series", "Third series");
        let publish = |title: &str, slug: &str, category_id: Uuid| {
            let document = repository
                .create_document_in_writable_site_with_category(
                    control.owner_user_id,
                    new_document(site_id, title, slug),
                    Some(category_id),
                )
                .unwrap();
            repository
                .publish_document_in_owned_site(
                    control.owner_user_id,
                    site_id,
                    document.id,
                    document.current_revision_id,
                )
                .unwrap()
        };
        let standalone = repository
            .create_document_in_owned_site(
                control.owner_user_id,
                new_document(site_id, "Older standalone", "older-standalone"),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                control.owner_user_id,
                site_id,
                standalone.id,
                standalone.current_revision_id,
            )
            .unwrap();
        let mut first_order = Vec::new();
        for index in 0..100 {
            first_order.push(
                publish(
                    &format!("First entry {index}"),
                    &format!("first-entry-{index}"),
                    first.category_id,
                )
                .id,
            );
        }
        let second_post = publish("Second entry", "second-entry", second.category_id);
        let third_post = publish("Third entry", "third-entry", third.category_id);

        let home = repository.home_feed(site_id, 100).unwrap();
        assert_eq!(
            home.series_sections
                .iter()
                .map(|section| section.series.id)
                .collect::<Vec<_>>(),
            vec![first.id, second.id, third.id]
        );
        assert_eq!(
            home.series_sections
                .iter()
                .map(|section| section.items.len())
                .collect::<Vec<_>>(),
            vec![98, 1, 1]
        );
        assert_eq!(
            home.series_sections[0]
                .items
                .iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            first_order[..98]
        );
        assert_eq!(home.series_sections[1].items[0].id, second_post.id);
        assert_eq!(home.series_sections[2].items[0].id, third_post.id);
        assert_eq!(
            home.series_sections
                .iter()
                .map(|section| section.items.len())
                .sum::<usize>(),
            HOME_FEED_MAX_SECTION_ITEMS.min(100)
        );
        assert_eq!(
            home.units
                .iter()
                .filter(|unit| matches!(unit, HomeUnitRecords::Series(_)))
                .count(),
            3
        );
        assert_eq!(
            home.recent
                .iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            vec![standalone.id],
            "newer Series members must not consume the standalone-post bound"
        );
        assert!(matches!(
            home.units.last(),
            Some(HomeUnitRecords::Post(document)) if document.id == standalone.id
        ));
    }

    #[test]
    fn home_category_sections_are_primary_active_oldest_first_bounded_and_pin_free() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let control = repository
            .provision_primary_owner_site(
                &primary_owner_bootstrap(site_id),
                AdminAuthMode::AccessKey,
                &[9; 32],
            )
            .unwrap();
        let create_category = |slug: &str, title: &str| {
            repository
                .create_category(
                    control.owner_user_id,
                    site_id,
                    CreateCategoryInput {
                        slug: slug.into(),
                        title: title.into(),
                        description: Some(format!("{title} description")),
                        theme_profile: None,
                    },
                )
                .unwrap()
        };
        let empty = create_category("empty", "Empty");
        let yangja = create_category("yangja", "yangja");
        let ontology = create_category("ontology", "ontology");
        let archived = create_category("archived", "Archived");
        let publish = |title: &str, slug: &str, category_id: Uuid| {
            let document = repository
                .create_document_in_writable_site_with_category(
                    control.owner_user_id,
                    new_document(site_id, title, slug),
                    Some(category_id),
                )
                .unwrap();
            repository
                .publish_document_in_owned_site(
                    control.owner_user_id,
                    site_id,
                    document.id,
                    document.current_revision_id,
                )
                .unwrap()
        };
        let tied_a = publish("Tied A", "tied-a", yangja.id);
        let middle = publish("Middle", "middle", yangja.id);
        let tied_b = publish("Tied B", "tied-b", yangja.id);
        let pinned = publish("Pinned", "pinned", yangja.id);
        let ontology_post = publish("Ontology post", "ontology-post", ontology.id);
        publish("Archived post", "archived-post", archived.id);
        repository
            .archive_category(control.owner_user_id, site_id, archived.id)
            .unwrap();

        let foreign_owner = community_user(&repository, "foreign-home-owner");
        let foreign_site = community_site(&repository, foreign_owner.id, "foreign-home-site");
        let foreign_category = repository
            .create_category(
                foreign_owner.id,
                foreign_site.id,
                CreateCategoryInput {
                    slug: "foreign".into(),
                    title: "Foreign".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let foreign_document = repository
            .create_document_in_writable_site_with_category(
                foreign_owner.id,
                new_document(foreign_site.id, "Foreign post", "foreign-post"),
                Some(foreign_category.id),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                foreign_owner.id,
                foreign_site.id,
                foreign_document.id,
                foreign_document.current_revision_id,
            )
            .unwrap();

        {
            let connection = repository.lock().unwrap();
            for (category_id, created_at) in [
                (empty.id, "2025-12-01T00:00:00+00:00"),
                (yangja.id, "2026-01-01T00:00:00+00:00"),
                (ontology.id, "2026-02-01T00:00:00+00:00"),
            ] {
                connection
                    .execute(
                        "UPDATE categories SET created_at = ?1 WHERE id = ?2",
                        params![created_at, category_id.to_string()],
                    )
                    .unwrap();
            }
            for (document_id, created_at) in [
                (tied_a.id, "2026-03-01T00:00:00+00:00"),
                (tied_b.id, "2026-03-01T00:00:00+00:00"),
                (middle.id, "2026-04-01T00:00:00+00:00"),
                (pinned.id, "2026-05-01T00:00:00+00:00"),
            ] {
                connection
                    .execute(
                        "UPDATE documents SET created_at = ?1 WHERE id = ?2",
                        params![created_at, document_id.to_string()],
                    )
                    .unwrap();
            }
            for (revision_id, published_at) in [
                (
                    tied_a.published_revision_id.unwrap(),
                    "2026-03-01T00:00:00+00:00",
                ),
                (
                    tied_b.published_revision_id.unwrap(),
                    "2026-03-01T00:00:00+00:00",
                ),
                (
                    middle.published_revision_id.unwrap(),
                    "2026-04-01T00:00:00+00:00",
                ),
                (
                    pinned.published_revision_id.unwrap(),
                    "2026-05-01T00:00:00+00:00",
                ),
            ] {
                connection
                    .execute(
                        "UPDATE revisions SET created_at = ?1 WHERE id = ?2",
                        params![published_at, revision_id.to_string()],
                    )
                    .unwrap();
            }
        }

        let earliest_tied = if tied_a.id < tied_b.id {
            &tied_a
        } else {
            &tied_b
        };
        let republished = repository
            .revise_document_in_owned_site(
                control.owner_user_id,
                site_id,
                ProposedRevision {
                    document_id: earliest_tied.id,
                    base_revision_id: earliest_tied.current_revision_id,
                    title: format!("{} republished", earliest_tied.revision.title),
                    slug: earliest_tied.revision.slug.clone(),
                    source_markdown: "A later publication must not change first-written order."
                        .into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                    idempotency_key: None,
                },
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                control.owner_user_id,
                site_id,
                earliest_tied.id,
                republished.id,
            )
            .unwrap();
        assert_eq!(
            repository
                .list_published_in_category(site_id, yangja.id, 100)
                .unwrap()[0]
                .id,
            earliest_tied.id,
            "the category archive remains publication-newest-first"
        );

        repository
            .replace_home_pins(
                control.owner_user_id,
                &[HomePinTarget::Post { id: pinned.id }],
            )
            .unwrap();
        let home = repository.home_feed(site_id, 100).unwrap();
        assert_eq!(
            home.category_sections
                .iter()
                .map(|section| section.category.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["yangja", "ontology"]
        );
        let mut tied_ids = vec![tied_a.id, tied_b.id];
        tied_ids.sort();
        tied_ids.push(middle.id);
        assert_eq!(
            home.category_sections[0]
                .items
                .iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            tied_ids
        );
        assert_eq!(
            home.category_sections[1]
                .items
                .iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            vec![ontology_post.id]
        );
        assert!(home.category_sections.iter().all(|section| {
            section
                .items
                .iter()
                .all(|document| document.id != pinned.id)
        }));

        let expected_recent = repository
            .list_published_across_sites(100)
            .unwrap()
            .into_iter()
            .filter(|document| document.id != pinned.id)
            .map(|document| document.id)
            .collect::<Vec<_>>();
        assert_eq!(
            home.recent
                .iter()
                .map(|document| document.id)
                .collect::<Vec<_>>(),
            expected_recent
        );
        assert!(
            home.recent
                .iter()
                .any(|document| document.id == foreign_document.id),
            "the primary home keeps installation-wide community posts as standalone peer units"
        );

        let bounded = repository.home_feed(site_id, 2).unwrap();
        assert_eq!(
            bounded
                .category_sections
                .iter()
                .map(|section| section.items.len())
                .sum::<usize>(),
            2
        );
        assert_eq!(bounded.category_sections.len(), 1);
        assert_eq!(bounded.category_sections[0].category.slug, "yangja");
    }

    #[test]
    fn create_revise_publish_and_resolve_old_route() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let site_id = Uuid::now_v7();
        let document = repository
            .create_document(NewDocument {
                site_id,
                title: "First".into(),
                slug: "first".into(),
                source_markdown: "# First".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                ai_summary: None,
                authorship: Default::default(),
                actor: actor(),
            })
            .unwrap();
        repository
            .publish(document.id, document.current_revision_id)
            .unwrap();

        let revision = repository
            .append_revision(ProposedRevision {
                document_id: document.id,
                base_revision_id: document.current_revision_id,
                title: "Renamed".into(),
                slug: "renamed".into(),
                source_markdown: "# Renamed".into(),
                embeds: vec![],
                intent: Some(IntentLayer {
                    format: "enhanced-html-v1".into(),
                    source_html: "<h1>Renamed</h1>".into(),
                    renderer_hints: BTreeMap::new(),
                    provenance: None,
                }),
                ontology: None,
                ai_summary: None,
                authorship: Default::default(),
                actor: actor(),
                idempotency_key: Some("rename-1".into()),
            })
            .unwrap();
        repository.publish(document.id, revision.id).unwrap();

        assert_eq!(
            repository
                .get_published_by_slug(site_id, "first")
                .unwrap()
                .revision
                .slug,
            "renamed"
        );
        assert_eq!(
            repository
                .get_published_by_slug(site_id, "renamed")
                .unwrap()
                .revision
                .slug,
            "renamed"
        );
        let export = repository.export_site(site_id).unwrap();
        assert_eq!(export.documents.len(), 1);
        assert_eq!(export.documents[0].revisions.len(), 2);
        assert_eq!(export.documents[0].routes.len(), 2);
        assert!(
            export.documents[0]
                .routes
                .iter()
                .any(|route| route.path == "first" && !route.canonical)
        );
        let administered = repository.list_documents(site_id, 10).unwrap();
        assert_eq!(administered.len(), 1);
        assert_eq!(administered[0].revision.id, revision.id);
        let history = repository.list_revisions(document.id, 10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, revision.id);
        assert_eq!(history[1].revision_number, 1);
    }

    #[test]
    fn stale_agent_revision_cannot_overwrite_current_content() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let document = repository
            .create_document(NewDocument {
                site_id: Uuid::now_v7(),
                title: "Post".into(),
                slug: "post".into(),
                source_markdown: "one".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                ai_summary: None,
                authorship: Default::default(),
                actor: actor(),
            })
            .unwrap();
        let proposal = ProposedRevision {
            document_id: document.id,
            base_revision_id: document.current_revision_id,
            title: "Post".into(),
            slug: "post".into(),
            source_markdown: "two".into(),
            embeds: vec![],
            intent: None,
            ontology: None,
            ai_summary: None,
            authorship: Default::default(),
            actor: actor(),
            idempotency_key: None,
        };
        repository.append_revision(proposal.clone()).unwrap();
        assert!(matches!(
            repository.append_revision(proposal),
            Err(RepositoryError::RevisionConflict)
        ));
    }

    #[test]
    fn ai_proposal_audit_roundtrips_lists_and_exports() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        repository.migrate().unwrap();
        repository.migrate().unwrap();
        let migration_count: i64 = repository
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version IN (1, 2)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migration_count, 2);

        let site_id = Uuid::now_v7();
        let document = repository
            .create_document(NewDocument {
                site_id,
                title: "Original".into(),
                slug: "original".into(),
                source_markdown: "# Original".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                ai_summary: None,
                authorship: Default::default(),
                actor: actor(),
            })
            .unwrap();
        let envelope = ai_envelope(
            document.id,
            document.current_revision_id,
            Uuid::now_v7(),
            "ai-proposal-1",
        );

        let revision = repository.append_ai_proposal(envelope.clone()).unwrap();
        assert_eq!(revision.actor.kind, RevisionActorKind::Agent);
        assert_eq!(revision.actor.id, envelope.actor.id);
        assert_eq!(revision.authorship.kind, PublicAuthorshipKind::AiGenerated);
        assert_eq!(
            revision.authorship.generator.as_deref(),
            Some("small-writer-v1")
        );
        let records = repository.list_ai_proposals(document.id, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].schema_version, AI_PROPOSAL_AUDIT_SCHEMA_VERSION);
        assert_eq!(records[0].document_id, document.id);
        assert_eq!(records[0].accepted_revision_id, revision.id);
        assert_eq!(records[0].envelope, envelope);

        let export = repository.export_site(site_id).unwrap();
        assert_eq!(export.schema_version, "open-soverign-blog-export/4");
        assert_eq!(export.documents[0].ai_proposals, records);
    }

    #[test]
    fn duplicate_ai_message_rolls_back_revision_and_document_update() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let document = repository
            .create_document(NewDocument {
                site_id: Uuid::now_v7(),
                title: "Original".into(),
                slug: "original".into(),
                source_markdown: "# Original".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                ai_summary: None,
                authorship: Default::default(),
                actor: actor(),
            })
            .unwrap();
        let message_id = Uuid::now_v7();
        let first = ai_envelope(
            document.id,
            document.current_revision_id,
            message_id,
            "ai-proposal-first",
        );
        let accepted = repository.append_ai_proposal(first).unwrap();

        let mut duplicate = ai_envelope(document.id, accepted.id, message_id, "ai-proposal-second");
        duplicate.proposal.source_markdown = "This must roll back".into();
        assert!(matches!(
            repository.append_ai_proposal(duplicate),
            Err(RepositoryError::DuplicateIdempotencyKey)
        ));

        assert_eq!(
            repository
                .get_document(document.id)
                .unwrap()
                .current_revision_id,
            accepted.id
        );
        assert_eq!(repository.list_revisions(document.id, 10).unwrap().len(), 2);
        assert_eq!(
            repository.list_ai_proposals(document.id, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn invalid_ai_envelope_creates_neither_revision_nor_audit() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let document = repository
            .create_document(NewDocument {
                site_id: Uuid::now_v7(),
                title: "Original".into(),
                slug: "original".into(),
                source_markdown: "# Original".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                ai_summary: None,
                authorship: Default::default(),
                actor: actor(),
            })
            .unwrap();
        let mut envelope = ai_envelope(
            document.id,
            document.current_revision_id,
            Uuid::now_v7(),
            "invalid-policy",
        );
        envelope.policy.allowed_provider_ids = vec!["different-provider".into()];

        assert!(matches!(
            repository.append_ai_proposal(envelope),
            Err(RepositoryError::Validation(_))
        ));
        assert_eq!(repository.list_revisions(document.id, 10).unwrap().len(), 1);
        assert!(
            repository
                .list_ai_proposals(document.id, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn online_backup_restores_published_content() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("site.db");
        let backup = directory.path().join("backup.db");
        let site_id = Uuid::now_v7();
        {
            let repository = SqliteRepository::open(&database).unwrap();
            let document = repository
                .create_document(NewDocument {
                    site_id,
                    title: "Backup".into(),
                    slug: "backup".into(),
                    source_markdown: "durable".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    ai_summary: None,
                    authorship: Default::default(),
                    actor: actor(),
                })
                .unwrap();
            repository
                .publish(document.id, document.current_revision_id)
                .unwrap();
            repository.backup_to(&backup).unwrap();
        }
        let restored = SqliteRepository::open(&backup).unwrap();
        assert_eq!(
            restored
                .get_published_by_slug(site_id, "backup")
                .unwrap()
                .revision
                .source_markdown,
            "durable"
        );
    }
}
