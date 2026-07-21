//! SQLite-first content repository.
//!
//! The connection uses WAL for file-backed databases, foreign keys, a busy
//! timeout, and short transactions. External work must never run while a
//! repository transaction is held.

use std::{
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
    PublicAuthorship, PublicAuthorshipKind, RepositoryError, RevisionActorKind, RevisionSnapshot,
    content_hash_with_ai_summary,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

/// Latest schema version required by both mutable and delivery-only runtimes.
pub const DATABASE_SCHEMA_VERSION: u64 = 8;

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

/// One globally curated home-page position. Pins reference documents rather
/// than revisions so a deliberate republish updates the visible pinned item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HomePinRecord {
    pub slot: u8,
    pub document_id: Uuid,
    pub pinned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HomeFeedRecords {
    pub pinned: Vec<DocumentSnapshot>,
    pub recent: Vec<DocumentSnapshot>,
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
            schema_version: "open-soverign-blog-export/3".into(),
            site_id,
            categories,
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
        let slug = normalize_category_slug(&input.slug)?;
        let title = validate_required_text(&input.title, "category title", 200)?;
        let description =
            validate_optional_text(input.description.as_deref(), "category description", 2_000)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure_site_owner(&transaction, owner_user_id, site_id)?;
        ensure_category_landing_available(&transaction, site_id, &slug)?;
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
        transaction.commit().map_err(storage_error)?;
        load_category_by_id(&connection, site_id, id)
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
        transaction.commit().map_err(storage_error)?;
        load_category_by_id(&connection, site_id, category_id)
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
        let mut statement = connection
            .prepare(
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
                 ORDER BY published.created_at DESC, document.id DESC LIMIT ?3",
            )
            .map_err(storage_error)?;
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
            .map(|id| load_document(&connection, parse_uuid(&id)?, RevisionSelector::Published))
            .collect()
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
        let document = create_document_in_transaction(&transaction, input, Utc::now(), None)?;
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
        let document = create_document_in_transaction(&transaction, input, Utc::now(), None)?;
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
        let document =
            create_document_in_transaction(&transaction, input, Utc::now(), initial_category)?;
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
        document_ids: &[Uuid],
    ) -> Result<Vec<HomePinRecord>, RepositoryError> {
        if document_ids.len() > 3
            || document_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != document_ids.len()
        {
            return Err(RepositoryError::Validation(
                "home pins must contain at most three unique document ids".into(),
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
        for document_id in document_ids {
            let published = transaction
                .query_row(
                    "SELECT 1 FROM documents
                     WHERE id = ?1
                       AND published_revision_id IS NOT NULL
                       AND status != 'archived'",
                    params![document_id.to_string()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(storage_error)?
                .is_some();
            if !published {
                return Err(RepositoryError::Validation(
                    "only currently published documents can be pinned".into(),
                ));
            }
        }

        transaction
            .execute("DELETE FROM home_pins", [])
            .map_err(storage_error)?;
        let now = Utc::now();
        for (index, document_id) in document_ids.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO home_pins (
                        slot, document_id, pinned_by_user_id, pinned_at
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        (index + 1) as i64,
                        document_id.to_string(),
                        administrator_user_id.to_string(),
                        now.to_rfc3339(),
                    ],
                )
                .map_err(map_constraint_error)?;
        }
        let pins = load_home_pins(&transaction)?;
        transaction.commit().map_err(storage_error)?;
        Ok(pins)
    }

    pub fn list_home_pins(&self) -> Result<Vec<HomePinRecord>, RepositoryError> {
        let connection = self.lock()?;
        load_home_pins(&connection)
    }

    /// Returns a coherent public home snapshot: curated documents in slot
    /// order and then newest published documents with every pin removed.
    pub fn home_feed(&self, recent_limit: usize) -> Result<HomeFeedRecords, RepositoryError> {
        let connection = self.lock()?;
        let pins = load_home_pins(&connection)?;
        let mut pinned = Vec::with_capacity(pins.len());
        let mut pinned_ids = std::collections::BTreeSet::new();
        for pin in pins {
            match load_document(&connection, pin.document_id, RevisionSelector::Published) {
                Ok(document) if document.status != DocumentStatus::Archived => {
                    pinned_ids.insert(document.id);
                    pinned.push(document);
                }
                Ok(_) | Err(RepositoryError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        let recent = list_documents_with_selector(
            &connection,
            None,
            recent_limit.saturating_add(pinned_ids.len()).min(500),
            RevisionSelector::Published,
        )?
        .into_iter()
        .filter(|document| !pinned_ids.contains(&document.id))
        .take(recent_limit.min(500))
        .collect();
        Ok(HomeFeedRecords { pinned, recent })
    }
}

fn load_home_pins(connection: &Connection) -> Result<Vec<HomePinRecord>, RepositoryError> {
    let mut statement = connection
        .prepare(
            "SELECT slot, document_id, pinned_at
             FROM home_pins ORDER BY slot ASC",
        )
        .map_err(storage_error)?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(storage_error)?
        .map(|row| {
            let (slot, document_id, pinned_at) = row.map_err(storage_error)?;
            Ok(HomePinRecord {
                slot: u8::try_from(slot).map_err(storage_error)?,
                document_id: parse_uuid(&document_id)?,
                pinned_at: parse_datetime(&pinned_at)?,
            })
        })
        .collect()
}

impl ContentRepository for SqliteRepository {
    fn create_document(&self, input: NewDocument) -> Result<DocumentSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        let document = create_document_in_transaction(&transaction, input, Utc::now(), None)?;
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
    insert_revision(transaction, &revision, None)?;
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
    let now = Utc::now();

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

fn normalize_category_slug(value: &str) -> Result<String, RepositoryError> {
    let slug = normalize_handle(value, "category slug")?;
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
    fn categories_are_site_scoped_atomic_and_keep_published_placement_stable() {
        let repository = SqliteRepository::open_in_memory().unwrap();
        let owner = community_user(&repository, "category-owner");
        let site = community_site(&repository, owner.id, "category-site");
        let other_owner = community_user(&repository, "other-category-owner");
        let other_site = community_site(&repository, other_owner.id, "other-category-site");

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
    fn site_export_v3_preserves_categories_and_revision_placements() {
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
        assert_eq!(export.schema_version, "open-soverign-blog-export/3");
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

        let ordered = [published[1], published[0], published[2]];
        let pins = repository
            .replace_home_pins(control.owner_user_id, &ordered)
            .unwrap();
        assert_eq!(
            pins.iter().map(|pin| pin.document_id).collect::<Vec<_>>(),
            ordered
        );
        let home = repository.home_feed(100).unwrap();
        assert_eq!(
            home.pinned.iter().map(|item| item.id).collect::<Vec<_>>(),
            ordered
        );
        assert_eq!(home.recent.len(), 1);
        assert_eq!(home.recent[0].id, published[3]);

        assert!(matches!(
            repository.replace_home_pins(control.owner_user_id, &[published[0], published[0]]),
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
            repository.replace_home_pins(control.owner_user_id, &[draft.id]),
            Err(RepositoryError::Validation(_))
        ));
        assert_eq!(repository.list_home_pins().unwrap(), pins);
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
        assert_eq!(export.schema_version, "open-soverign-blog-export/3");
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
