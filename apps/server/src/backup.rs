use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use osb_storage_sqlite::SqliteRepository;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::config::OperationsSettings;

const BACKUP_SCHEMA: &str = "open-soverign-blog-backup/1";
const BACKUP_DATABASE: &str = "database.sqlite3";
const BACKUP_BLOBS: &str = "blobs";
const BACKUP_MANIFEST: &str = "manifest.json";

#[derive(Clone)]
pub struct BackupService {
    status: Arc<RwLock<BackupStatus>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum BackupState {
    Waiting,
    Running,
    Healthy,
    Degraded,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupStatus {
    state: BackupState,
    directory: String,
    interval_minutes: u64,
    retention: usize,
    last_started_at: Option<DateTime<Utc>>,
    last_completed_at: Option<DateTime<Utc>>,
    last_generation: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupManifest {
    schema_version: String,
    files: Vec<FileRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileRecord {
    path: String,
    sha256: String,
    size: u64,
}

impl BackupService {
    pub fn start(
        repository: Arc<SqliteRepository>,
        blob_directory: PathBuf,
        settings: OperationsSettings,
    ) -> Self {
        let service = Self {
            status: Arc::new(RwLock::new(BackupStatus {
                state: BackupState::Waiting,
                directory: settings.backup_directory.display().to_string(),
                interval_minutes: settings.backup_interval_minutes,
                retention: settings.backup_retention,
                last_started_at: None,
                last_completed_at: None,
                last_generation: None,
                last_error: None,
            })),
        };
        let worker = service.clone();
        tokio::spawn(async move {
            loop {
                worker
                    .run_once(
                        Arc::clone(&repository),
                        blob_directory.clone(),
                        settings.backup_directory.clone(),
                        settings.backup_retention,
                    )
                    .await;
                tokio::time::sleep(Duration::from_secs(
                    settings.backup_interval_minutes.saturating_mul(60),
                ))
                .await;
            }
        });
        service
    }

    pub async fn snapshot(&self) -> serde_json::Value {
        serde_json::to_value(self.status.read().await.clone())
            .unwrap_or_else(|_| serde_json::json!({"state": "unknown"}))
    }

    async fn run_once(
        &self,
        repository: Arc<SqliteRepository>,
        blob_directory: PathBuf,
        backup_directory: PathBuf,
        retention: usize,
    ) {
        {
            let mut status = self.status.write().await;
            status.state = BackupState::Running;
            status.last_started_at = Some(Utc::now());
        }
        let result = tokio::task::spawn_blocking(move || {
            create_generation(
                repository.as_ref(),
                &blob_directory,
                &backup_directory,
                retention,
            )
        })
        .await
        .context("managed backup worker panicked")
        .and_then(|result| result);
        let mut status = self.status.write().await;
        match result {
            Ok(generation) => {
                status.state = BackupState::Healthy;
                status.last_completed_at = Some(Utc::now());
                status.last_generation = Some(generation);
                status.last_error = None;
            }
            Err(error) => {
                tracing::error!(%error, "managed backup generation failed");
                status.state = BackupState::Degraded;
                status.last_error = Some(format!("{error:#}"));
            }
        }
    }
}

fn create_generation(
    repository: &SqliteRepository,
    blob_directory: &Path,
    backup_directory: &Path,
    retention: usize,
) -> Result<String> {
    ensure_safe_backup_root(backup_directory)?;
    let resolved_backup_directory =
        validate_backup_directory_relationship(blob_directory, backup_directory)?;
    fs::create_dir_all(backup_directory).with_context(|| {
        format!(
            "failed to create managed backup directory {}",
            backup_directory.display()
        )
    })?;
    ensure_safe_backup_root(backup_directory)?;
    let created_backup_directory =
        validate_backup_directory_relationship(blob_directory, backup_directory)?;
    ensure!(
        created_backup_directory == resolved_backup_directory,
        "managed backup directory changed while it was being prepared"
    );
    let generations = backup_directory.join("generations");
    fs::create_dir_all(&generations)?;
    ensure_real_directory(&generations, "backup generations")?;

    let id = Uuid::now_v7().simple().to_string();
    let generation = format!("generation-{}-{id}", Utc::now().format("%Y%m%dT%H%M%SZ"));
    let staging = generations.join(format!(".{generation}.staging"));
    let final_path = generations.join(&generation);
    fs::create_dir(&staging)?;
    let guard = StagingGuard(staging.clone());

    let database_path = staging.join(BACKUP_DATABASE);
    repository
        .backup_to(&database_path)
        .map_err(anyhow::Error::msg)
        .context("SQLite Online Backup API failed")?;
    let mut files = vec![record_file(&staging, &database_path)?];

    let blobs_target = staging.join(BACKUP_BLOBS);
    fs::create_dir(&blobs_target)?;
    if blob_directory.exists() {
        ensure_real_directory(blob_directory, "content-addressed blob source")?;
        copy_tree(blob_directory, &blobs_target)?;
    }
    collect_records(&staging, &blobs_target, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));

    let manifest = BackupManifest {
        schema_version: BACKUP_SCHEMA.into(),
        files,
    };
    validate_manifest(&manifest)?;
    let manifest_path = staging.join(BACKUP_MANIFEST);
    let mut encoded = serde_json::to_vec_pretty(&manifest)?;
    encoded.push(b'\n');
    write_new_synced(&manifest_path, &encoded)?;
    verify_generation(&staging, &manifest)
        .context("new managed backup generation failed self-verification")?;
    sync_directory(&staging)?;
    fs::rename(&staging, &final_path)?;
    std::mem::forget(guard);
    sync_directory(&generations)?;
    prune_generations(&generations, retention)?;
    tracing::info!(
        generation,
        files = manifest.files.len(),
        "managed backup generation completed"
    );
    Ok(generation)
}

fn ensure_safe_backup_root(path: &Path) -> Result<()> {
    ensure!(
        path.file_name().is_some(),
        "managed backup directory cannot be a filesystem root or current-directory alias"
    );
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "managed backup root must be a real directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a real directory: {}",
        path.display()
    );
    Ok(())
}

fn validate_backup_directory_relationship(
    blob_directory: &Path,
    backup_directory: &Path,
) -> Result<PathBuf> {
    let destination = canonical_destination(backup_directory)?;
    ensure!(
        destination.file_name().is_some(),
        "managed backup directory cannot resolve to a filesystem root"
    );
    let metadata = match fs::symlink_metadata(blob_directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(destination),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "content-addressed blob source must be a real directory: {}",
        blob_directory.display()
    );
    let source = fs::canonicalize(blob_directory)?;
    ensure!(
        !destination.starts_with(&source) && !source.starts_with(&destination),
        "managed backup directory and content-addressed blob source must not equal or contain one another: {}",
        source.display()
    );
    Ok(destination)
}

/// Resolves every existing ancestor, including symlinks, while retaining the
/// not-yet-created suffix so an apparently separate destination cannot resolve
/// back into the live blob tree.
fn canonical_destination(path: &Path) -> Result<PathBuf> {
    let absolute = absolute_lexical(path)?;
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let resolved = fs::canonicalize(ancestor)?;
                ensure!(
                    fs::metadata(&resolved)?.is_dir(),
                    "managed backup ancestor is not a directory: {}",
                    ancestor.display()
                );
                let mut destination = resolved;
                for component in missing.iter().rev() {
                    destination.push(component);
                }
                return Ok(destination);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = ancestor.file_name().with_context(|| {
                    format!(
                        "cannot resolve a filesystem ancestor for managed backup destination {}",
                        path.display()
                    )
                })?;
                missing.push(component.to_os_string());
                ancestor = ancestor.parent().with_context(|| {
                    format!(
                        "cannot resolve a filesystem ancestor for managed backup destination {}",
                        path.display()
                    )
                })?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(normalized.pop(), "path escapes its filesystem root");
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(
                "symlinks are forbidden in backup sources: {}",
                entry.path().display()
            );
        }
        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else if metadata.is_file() {
            let mut input = File::open(entry.path())?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        } else {
            bail!(
                "special files are forbidden in backup sources: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn collect_records(root: &Path, directory: &Path, records: &mut Vec<FileRecord>) -> Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            collect_records(root, &entry.path(), records)?;
        } else if metadata.is_file() && !metadata.file_type().is_symlink() {
            records.push(record_file(root, &entry.path())?);
        } else {
            bail!("backup payload contains a forbidden filesystem entry");
        }
    }
    Ok(())
}

fn record_file(root: &Path, path: &Path) -> Result<FileRecord> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "backup payload is not a regular file: {}",
        path.display()
    );
    let relative = path.strip_prefix(root)?;
    let portable = relative
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .context("backup paths must be UTF-8")
        })
        .collect::<Result<Vec<_>>>()?
        .join("/");
    validate_manifest_path(&portable)?;
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "opened backup payload is not a regular file: {}",
        path.display()
    );
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok(FileRecord {
        path: portable,
        sha256: format!("{:x}", hasher.finalize()),
        size,
    })
}

fn verify_generation(root: &Path, expected: &BackupManifest) -> Result<()> {
    ensure_real_directory(root, "managed backup generation")?;
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .context("backup root entries must be UTF-8")?
            .to_owned();
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symlinks are forbidden in managed backup generations"
        );
        match name.as_str() {
            BACKUP_DATABASE | BACKUP_MANIFEST if metadata.is_file() => {}
            BACKUP_BLOBS if metadata.is_dir() => {}
            _ => bail!(
                "unexpected managed backup root entry: {}",
                entry.path().display()
            ),
        }
        names.insert(name);
    }
    for required in [BACKUP_DATABASE, BACKUP_BLOBS, BACKUP_MANIFEST] {
        ensure!(
            names.contains(required),
            "managed backup generation is missing required entry {required}"
        );
    }

    let decoded: BackupManifest = read_json_regular(&root.join(BACKUP_MANIFEST))?;
    validate_manifest(&decoded)?;
    ensure!(
        &decoded == expected,
        "managed backup manifest changed while it was being verified"
    );

    let mut actual = vec![record_file(root, &root.join(BACKUP_DATABASE))?];
    collect_records(root, &root.join(BACKUP_BLOBS), &mut actual)?;
    actual.sort_by(|left, right| left.path.cmp(&right.path));
    ensure!(
        actual.as_slice() == expected.files.as_slice(),
        "managed backup payload does not match its manifest"
    );
    let database = SqliteRepository::open_read_only(root.join(BACKUP_DATABASE))
        .map_err(anyhow::Error::msg)
        .context("managed backup database is not a readable, migrated SQLite snapshot")?;
    drop(database);
    Ok(())
}

fn validate_manifest(manifest: &BackupManifest) -> Result<()> {
    ensure!(
        manifest.schema_version == BACKUP_SCHEMA,
        "unsupported managed backup schema {:?}",
        manifest.schema_version
    );
    let mut previous: Option<&str> = None;
    for entry in &manifest.files {
        validate_manifest_path(&entry.path)?;
        ensure!(
            entry.sha256.len() == 64
                && entry
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "managed backup manifest has an invalid SHA-256 digest"
        );
        if let Some(previous) = previous {
            ensure!(
                previous < entry.path.as_str(),
                "managed backup manifest paths must be unique and sorted"
            );
        }
        previous = Some(&entry.path);
    }
    ensure!(
        manifest
            .files
            .iter()
            .any(|entry| entry.path == BACKUP_DATABASE),
        "managed backup manifest is missing {BACKUP_DATABASE}"
    );
    ensure!(
        manifest
            .files
            .iter()
            .all(|entry| { entry.path == BACKUP_DATABASE || entry.path.starts_with("blobs/") }),
        "managed backup manifest contains a payload outside database.sqlite3 and blobs/"
    );
    Ok(())
}

fn validate_manifest_path(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "backup manifest path is empty");
    ensure!(
        !value.contains(['\\', '\0', ':']),
        "backup manifest path is not portable: {value:?}"
    );
    ensure!(
        value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.chars().any(char::is_control)
        }),
        "backup manifest path is unsafe: {value:?}"
    );
    let path = Path::new(value);
    ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "backup manifest path is unsafe: {value:?}"
    );
    Ok(())
}

fn read_json_regular<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "backup manifest is not a regular file: {}",
        path.display()
    );
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse backup manifest {}", path.display()))
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn prune_generations(root: &Path, retention: usize) -> Result<()> {
    let mut generations = fs::read_dir(root)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            name.starts_with("generation-")
                .then_some((name, entry.path()))
        })
        .collect::<Vec<_>>();
    generations.sort_by(|left, right| left.0.cmp(&right.0));
    let remove = generations.len().saturating_sub(retention);
    for (_, path) in generations.into_iter().take(remove) {
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "refusing to prune a non-directory backup generation"
        );
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

struct StagingGuard(PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generation_contains_a_verified_database_and_blob_manifest() {
        let root = tempdir().unwrap();
        let blobs = root.path().join("blobs/sha256/ab");
        fs::create_dir_all(&blobs).unwrap();
        fs::write(blobs.join("abcdef"), b"image").unwrap();
        let repository = SqliteRepository::open(root.path().join("source.sqlite3")).unwrap();
        let backups = root.path().join("backups");
        let generation =
            create_generation(&repository, &root.path().join("blobs"), &backups, 2).unwrap();
        let generation = backups.join("generations").join(generation);
        let manifest: BackupManifest =
            read_json_regular(&generation.join(BACKUP_MANIFEST)).unwrap();
        assert_eq!(manifest.schema_version, BACKUP_SCHEMA);
        let blob = manifest
            .files
            .iter()
            .find(|entry| entry.path == "blobs/sha256/ab/abcdef")
            .unwrap();
        assert_eq!(blob.size, 5);
        verify_generation(&generation, &manifest).unwrap();
    }

    #[test]
    fn generation_self_verification_detects_payload_tampering() {
        let root = tempdir().unwrap();
        let blob_source = root.path().join("blobs/sha256/ab");
        fs::create_dir_all(&blob_source).unwrap();
        fs::write(blob_source.join("abcdef"), b"image").unwrap();
        let repository = SqliteRepository::open(root.path().join("source.sqlite3")).unwrap();
        let backups = root.path().join("backups");
        let generation =
            create_generation(&repository, &root.path().join("blobs"), &backups, 2).unwrap();
        let generation = backups.join("generations").join(generation);
        let manifest: BackupManifest =
            read_json_regular(&generation.join(BACKUP_MANIFEST)).unwrap();
        fs::write(generation.join("blobs/sha256/ab/abcdef"), b"tampered").unwrap();

        assert!(verify_generation(&generation, &manifest).is_err());
    }

    #[test]
    fn managed_backup_rejects_blob_source_and_its_descendants() {
        let root = tempdir().unwrap();
        let blob_source = root.path().join("blobs");
        fs::create_dir_all(&blob_source).unwrap();
        let repository = SqliteRepository::open(root.path().join("source.sqlite3")).unwrap();

        for destination in [&blob_source, &blob_source.join("nested-backups")] {
            let error = create_generation(&repository, &blob_source, destination, 2).unwrap_err();
            assert!(format!("{error:#}").contains("must not equal or contain one another"));
        }
        assert!(!blob_source.join("nested-backups").exists());
    }

    #[test]
    fn managed_backup_rejects_a_root_that_contains_the_blob_source() {
        let root = tempdir().unwrap();
        let blob_source = root.path().join("live/blobs");
        fs::create_dir_all(&blob_source).unwrap();
        let repository = SqliteRepository::open(root.path().join("source.sqlite3")).unwrap();

        let error = create_generation(&repository, &blob_source, root.path(), 2).unwrap_err();
        assert!(format!("{error:#}").contains("must not equal or contain one another"));
        assert!(!root.path().join("generations").exists());
    }

    #[test]
    fn managed_backup_rejects_the_live_database_file_as_its_root() {
        let root = tempdir().unwrap();
        let database = root.path().join("source.sqlite3");
        let repository = SqliteRepository::open(&database).unwrap();
        let blob_source = root.path().join("blobs");
        fs::create_dir_all(&blob_source).unwrap();

        let error = create_generation(&repository, &blob_source, &database, 2).unwrap_err();
        assert!(format!("{error:#}").contains("must be a real directory"));
    }

    #[cfg(unix)]
    #[test]
    fn managed_backup_rejects_a_lexical_alias_for_the_filesystem_root() {
        let root = tempdir().unwrap();
        let database = root.path().join("source.sqlite3");
        let repository = SqliteRepository::open(&database).unwrap();
        let blob_source = root.path().join("blobs");
        fs::create_dir_all(&blob_source).unwrap();
        let root_alias = Path::new("/tmp/..");

        let error = create_generation(&repository, &blob_source, root_alias, 2).unwrap_err();
        assert!(format!("{error:#}").contains("filesystem root"));
    }

    #[cfg(unix)]
    #[test]
    fn managed_backup_detects_a_parent_symlink_back_into_the_blob_source() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let blob_source = root.path().join("blobs");
        fs::create_dir_all(&blob_source).unwrap();
        let alias = root.path().join("blob-alias");
        symlink(&blob_source, &alias).unwrap();
        let repository = SqliteRepository::open(root.path().join("source.sqlite3")).unwrap();

        let error = create_generation(&repository, &blob_source, &alias.join("nested-backups"), 2)
            .unwrap_err();
        assert!(format!("{error:#}").contains("must not equal or contain one another"));
    }

    #[test]
    fn retention_only_prunes_named_generation_directories() {
        let root = tempdir().unwrap();
        for name in ["generation-1", "generation-2", "generation-3", "keep-me"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        prune_generations(root.path(), 2).unwrap();
        assert!(!root.path().join("generation-1").exists());
        assert!(root.path().join("generation-2").exists());
        assert!(root.path().join("generation-3").exists());
        assert!(root.path().join("keep-me").exists());
    }
}
