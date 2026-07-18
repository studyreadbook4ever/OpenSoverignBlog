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
    RepositoryError, RevisionActorKind, RevisionSnapshot, content_hash,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

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
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub type CommunitySession = SessionRecord;

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

pub type CommunitySite = SiteRecord;

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
    pub documents: Vec<ExportedDocument>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedDocument {
    pub current: DocumentSnapshot,
    pub revisions: Vec<RevisionSnapshot>,
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
                "SELECT 1 FROM schema_migrations WHERE version = 5",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage_error)?
            .is_some();
        if !migrated {
            return Err(RepositoryError::Storage(
                "delivery-only database must be migrated through schema version 5".into(),
            ));
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
                        row.map_err(storage_error)
                            .and_then(|json| serde_json::from_str(&json).map_err(storage_error))
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
            let ai_proposals = load_ai_proposals(
                &connection,
                document_id,
                usize::MAX,
                AuditOrder::OldestFirst,
            )?;
            documents.push(ExportedDocument {
                current,
                revisions,
                ai_proposals,
                routes,
            });
        }
        Ok(SiteExport {
            schema_version: "open-soverign-blog-export/2".into(),
            site_id,
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

    /// Stores only a 32-byte SHA-256 digest of the opaque browser credential.
    pub fn create_session(
        &self,
        user_id: Uuid,
        token_hash: &[u8],
        expires_at: DateTime<Utc>,
    ) -> Result<SessionRecord, RepositoryError> {
        validate_token_hash(token_hash)?;
        let now = Utc::now();
        if expires_at <= now {
            return Err(RepositoryError::Validation(
                "session expiry must be in the future".into(),
            ));
        }
        let connection = self.lock()?;
        let id = Uuid::now_v7();
        connection
            .execute(
                "INSERT INTO sessions (
                    id, token_hash, user_id, expires_at, created_at, revoked_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                params![
                    id.to_string(),
                    token_hash,
                    user_id.to_string(),
                    expires_at.to_rfc3339(),
                    now.to_rfc3339(),
                ],
            )
            .map_err(map_community_constraint_error)?;
        load_session_by_id(&connection, id)
    }

    /// Returns only a currently valid session. Expired and revoked credentials
    /// are indistinguishable from unknown credentials.
    pub fn get_session(&self, token_hash: &[u8]) -> Result<SessionRecord, RepositoryError> {
        validate_token_hash(token_hash)?;
        let connection = self.lock()?;
        let raw: Option<StoredSessionRow> = connection
            .query_row(
                "SELECT id, user_id, expires_at, created_at, revoked_at
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
        let document = create_document_in_transaction(&transaction, input, Utc::now())?;
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
        let document = create_document_in_transaction(&transaction, input, Utc::now())?;
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

impl ContentRepository for SqliteRepository {
    fn create_document(&self, input: NewDocument) -> Result<DocumentSnapshot, RepositoryError> {
        input
            .validate()
            .map_err(|error| RepositoryError::Validation(error.to_string()))?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction().map_err(storage_error)?;
        let document = create_document_in_transaction(&transaction, input, Utc::now())?;
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
        proposal.idempotency_key = Some(envelope.idempotency_key.clone());

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
        actor: input.actor,
        content_hash: String::new(),
        created_at: now,
    });

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
        .map_err(map_constraint_error)?;
    insert_revision(transaction, &revision, None)?;

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
                revision.slug,
                document_id.to_string(),
                now.to_rfc3339()
            ],
        )
        .map_err(map_constraint_error)?;
    let routed_document: String = transaction
        .query_row(
            "SELECT document_id FROM routes WHERE site_id = ?1 AND path = ?2",
            params![site_id, revision.slug],
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
        actor,
        idempotency_key,
    } = input;
    let current: Option<(String, i64)> = transaction
        .query_row(
            "SELECT d.current_revision_id, r.revision_number
             FROM documents d
             JOIN revisions r ON r.id = d.current_revision_id
             WHERE d.id = ?1",
            params![document_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    let (current_revision_id, revision_number) = current.ok_or(RepositoryError::NotFound)?;
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
        actor,
        content_hash: String::new(),
        created_at: now,
    });
    insert_revision(transaction, &revision, idempotency_key.as_deref())?;
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
                revision.slug,
                revision.created_at.to_rfc3339(),
                revision.document_id.to_string(),
            ],
        )
        .map_err(map_constraint_error)?;
    Ok(revision)
}

fn with_computed_hash(mut revision: RevisionSnapshot) -> RevisionSnapshot {
    revision.content_hash = content_hash(
        &revision.title,
        &revision.slug,
        &revision.source_markdown,
        &revision.embeds,
        revision.intent.as_ref(),
        revision.ontology.as_ref(),
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

type StoredSessionRow = (String, String, String, String, Option<String>);

fn stored_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSessionRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn parse_session_row(raw: StoredSessionRow) -> Result<SessionRecord, RepositoryError> {
    let (id, user_id, expires_at, created_at, revoked_at) = raw;
    Ok(SessionRecord {
        id: parse_uuid(&id)?,
        user_id: parse_uuid(&user_id)?,
        expires_at: parse_datetime(&expires_at)?,
        created_at: parse_datetime(&created_at)?,
        revoked_at: revoked_at.as_deref().map(parse_datetime).transpose()?,
    })
}

fn load_session_by_id(connection: &Connection, id: Uuid) -> Result<SessionRecord, RepositoryError> {
    let raw = connection
        .query_row(
            "SELECT id, user_id, expires_at, created_at, revoked_at
             FROM sessions WHERE id = ?1",
            params![id.to_string()],
            stored_session_row,
        )
        .optional()
        .map_err(storage_error)?
        .ok_or(RepositoryError::NotFound)?;
    parse_session_row(raw)
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

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::mpsc, thread};

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

    fn new_document(site_id: Uuid, title: &str, slug: &str) -> NewDocument {
        NewDocument {
            site_id,
            title: title.into(),
            slug: slug.into(),
            source_markdown: format!("# {title}"),
            embeds: vec![],
            intent: None,
            ontology: None,
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
        let records = repository.list_ai_proposals(document.id, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].schema_version, AI_PROPOSAL_AUDIT_SCHEMA_VERSION);
        assert_eq!(records[0].document_id, document.id);
        assert_eq!(records[0].accepted_revision_id, revision.id);
        assert_eq!(records[0].envelope, envelope);

        let export = repository.export_site(site_id).unwrap();
        assert_eq!(export.schema_version, "open-soverign-blog-export/2");
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
