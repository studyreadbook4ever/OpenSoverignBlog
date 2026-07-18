use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use osb_plugin_api::PluginManifest;
use osb_storage_sqlite::SqliteRepository;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

mod bootstrap;

use bootstrap::{BootstrapArgs, DoctorArgs};

const BUNDLE_SCHEMA_VERSION: &str = "open-soverign-blog-backup/1";
const LEGACY_MANAGED_BUNDLE_SCHEMA_VERSION: &str = "open-soverign-blog-managed-backup/1";
const ASSET_EXPORT_SCHEMA_VERSION: &str = "open-soverign-blog-asset-export/1";
const BUNDLE_DATABASE: &str = "database.sqlite3";
const BUNDLE_BLOBS: &str = "blobs";
const BUNDLE_MANIFEST: &str = "manifest.json";
const EXPORT_ASSET_MANIFEST: &str = "assets-manifest.json";

#[derive(Debug, Parser)]
#[command(name = "osb", version, about = "OpenSoverignBlog administration CLI")]
struct Args {
    #[arg(
        long,
        env = "OSB_DATABASE",
        default_value = ".data/open-soverign-blog.db"
    )]
    database: PathBuf,
    #[arg(long, env = "OSB_BLOB_DIRECTORY", default_value = ".data/blobs")]
    blob_directory: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a complete human- and AI-readable on-premise deployment intent.
    #[command(alias = "init")]
    Bootstrap(BootstrapArgs),
    /// Validate semantic configuration, storage paths, and Redis readiness.
    Doctor(DoctorArgs),
    /// Create a SQLite snapshot or verify a complete backup generation.
    Backup {
        #[arg(value_name = "OUTPUT")]
        output: Option<PathBuf>,
        #[command(subcommand)]
        action: Option<BackupAction>,
    },
    /// Create a verified SQLite and first-party blob backup directory.
    BackupBundle { output: PathBuf },
    /// Verify every payload file in a backup bundle.
    VerifyBundle { bundle: PathBuf },
    /// Verify and restore a bundle into new database and blob targets.
    #[command(alias = "restore-bundle")]
    Restore { bundle: PathBuf },
    /// Export portable Markdown, revisions, and a verified blob copy.
    Export { site_id: Uuid, output: PathBuf },
    /// Parse and enforce the v1 plugin manifest security contract.
    ValidatePlugin { manifest: PathBuf },
}

#[derive(Debug, Subcommand)]
enum BackupAction {
    /// Verify every payload in a portable or managed backup generation.
    Verify { bundle: PathBuf },
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Bootstrap(options) => bootstrap::bootstrap(options),
        Command::Doctor(options) => bootstrap::doctor(options),
        Command::Backup { output, action } => match (output, action) {
            (Some(output), None) => backup(args.database, output),
            (None, Some(BackupAction::Verify { bundle })) => verify_bundle_command(bundle),
            (None, None) => bail!("backup requires OUTPUT or the verify subcommand"),
            (Some(_), Some(_)) => {
                bail!("backup OUTPUT and a backup subcommand cannot be used together")
            }
        },
        Command::BackupBundle { output } => {
            backup_bundle(args.database, args.blob_directory, output)
        }
        Command::VerifyBundle { bundle } => verify_bundle_command(bundle),
        Command::Restore { bundle } => restore_bundle(bundle, args.database, args.blob_directory),
        Command::Export { site_id, output } => {
            export(args.database, args.blob_directory, site_id, output)
        }
        Command::ValidatePlugin { manifest } => validate_plugin(manifest),
    }
}

/// Retains the original database-only backup behavior.
fn backup(database: PathBuf, output: PathBuf) -> Result<()> {
    if output.exists() {
        bail!("refusing to overwrite existing backup {}", output.display());
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).context("failed to create backup parent directory")?;
    }
    let repository = SqliteRepository::open(database).map_err(anyhow::Error::msg)?;
    repository
        .backup_to(&output)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("failed to back up to {}", output.display()))?;
    println!("backup created: {}", output.display());
    Ok(())
}

fn backup_bundle(database: PathBuf, blob_directory: PathBuf, output: PathBuf) -> Result<()> {
    ensure_regular_source_file(&database, "SQLite database")?;
    let output_guard = FreshDirectory::create(&output, "backup bundle")?;
    reject_nested_output(&blob_directory, &output, "blob source")?;

    let database_output = output.join(BUNDLE_DATABASE);
    let repository = SqliteRepository::open(&database).map_err(anyhow::Error::msg)?;
    repository
        .backup_to(&database_output)
        .map_err(anyhow::Error::msg)
        .with_context(|| {
            format!(
                "failed to create online SQLite backup {}",
                database_output.display()
            )
        })?;

    copy_tree_strict(&blob_directory, &output.join(BUNDLE_BLOBS), true)
        .context("failed to copy the content-addressed blob tree")?;

    let manifest = FileManifest {
        schema_version: BUNDLE_SCHEMA_VERSION.into(),
        files: collect_file_records(&output, &[BUNDLE_MANIFEST])?,
    };
    write_json_new(&output.join(BUNDLE_MANIFEST), &manifest)?;
    verify_bundle(&output).context("new backup bundle failed self-verification")?;

    output_guard.commit();
    println!(
        "backup bundle created and verified: {} ({} files)",
        output.display(),
        manifest.files.len()
    );
    Ok(())
}

fn verify_bundle_command(bundle: PathBuf) -> Result<()> {
    let manifest = verify_bundle(&bundle)?;
    println!(
        "backup bundle verified: {} ({} files)",
        bundle.display(),
        manifest.files.len()
    );
    Ok(())
}

fn verify_bundle(bundle: &Path) -> Result<FileManifest> {
    ensure_directory_without_symlink(bundle, "backup bundle")?;
    verify_bundle_root_layout(bundle)?;
    ensure_directory_without_symlink(
        &bundle.join(BUNDLE_BLOBS).join("sha256"),
        "backup content-addressed blob namespace",
    )?;

    let manifest = read_bundle_manifest(&bundle.join(BUNDLE_MANIFEST))?;
    validate_manifest(&manifest, BUNDLE_SCHEMA_VERSION)?;
    ensure!(
        manifest
            .files
            .iter()
            .any(|entry| entry.path == BUNDLE_DATABASE),
        "backup manifest does not contain {BUNDLE_DATABASE}"
    );
    ensure!(
        manifest
            .files
            .iter()
            .all(|entry| entry.path == BUNDLE_DATABASE || entry.path.starts_with("blobs/")),
        "backup manifest contains a payload outside database.sqlite3 and blobs/"
    );

    let actual = collect_file_records(bundle, &[BUNDLE_MANIFEST])?;
    compare_file_records(&manifest.files, &actual, "backup bundle")?;
    let database = SqliteRepository::open_read_only(bundle.join(BUNDLE_DATABASE))
        .map_err(anyhow::Error::msg)
        .context("backup database is not a readable, migrated SQLite snapshot")?;
    drop(database);
    Ok(manifest)
}

fn restore_bundle(bundle: PathBuf, database: PathBuf, blob_directory: PathBuf) -> Result<()> {
    // Verification intentionally precedes target inspection. A broken bundle
    // must never be described merely as a target collision.
    let manifest = verify_bundle(&bundle).context("bundle verification failed")?;

    refuse_existing_path(&database, "target database")?;
    refuse_existing_path(&blob_directory, "target blob directory")?;
    reject_restore_path_relationships(&bundle, &database, &blob_directory)?;

    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent).context("failed to create target database parent")?;
    }
    if let Some(parent) = blob_directory.parent() {
        fs::create_dir_all(parent).context("failed to create target blob parent")?;
    }
    fs::create_dir(&blob_directory).with_context(|| {
        format!(
            "refusing to merge into target blob directory {}",
            blob_directory.display()
        )
    })?;
    let mut guard = RestoreGuard::new(database.clone(), blob_directory.clone());
    guard.blob_created = true;
    // Empty content-addressed namespaces have no manifest file record, but a
    // delivery node still requires the canonical sha256 directory to exist.
    fs::create_dir(blob_directory.join("sha256"))
        .context("failed to create restored content-addressed blob namespace")?;

    for entry in manifest
        .files
        .iter()
        .filter(|entry| entry.path.starts_with("blobs/"))
    {
        let relative = entry
            .path
            .strip_prefix("blobs/")
            .expect("filtered bundle blob path");
        let destination = blob_directory.join(manifest_path(relative)?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).context("failed to create restored blob directory")?;
        }
        copy_verified_new(
            &bundle.join(manifest_path(&entry.path)?),
            &destination,
            entry,
        )?;
    }

    let database_entry = manifest
        .files
        .iter()
        .find(|entry| entry.path == BUNDLE_DATABASE)
        .expect("validated bundle database entry");
    copy_verified_new(&bundle.join(BUNDLE_DATABASE), &database, database_entry)?;
    guard.database_created = true;
    guard.commit();

    println!(
        "backup bundle restored to database {} and blobs {}",
        database.display(),
        blob_directory.display()
    );
    Ok(())
}

fn export(
    database: PathBuf,
    blob_directory: PathBuf,
    site_id: Uuid,
    output: PathBuf,
) -> Result<()> {
    let output_guard = FreshDirectory::create(&output, "export directory")?;
    reject_nested_output(&blob_directory, &output, "blob source")?;
    let content_directory = output.join("content");
    fs::create_dir(&content_directory).context("failed to create export content directory")?;

    let repository = SqliteRepository::open(database).map_err(anyhow::Error::msg)?;
    let export = repository
        .export_site(site_id)
        .map_err(anyhow::Error::msg)?;
    for document in &export.documents {
        let stem = document.current.id.to_string();
        fs::write(
            content_directory.join(format!("{stem}.md")),
            &document.current.revision.source_markdown,
        )
        .context("failed to write Markdown export")?;
        fs::write(
            content_directory.join(format!("{stem}.json")),
            serde_json::to_vec_pretty(document)?,
        )
        .context("failed to write structured content export")?;
    }
    fs::write(
        output.join(BUNDLE_MANIFEST),
        serde_json::to_vec_pretty(&export)?,
    )
    .context("failed to write export manifest")?;

    let exported_blobs = output.join(BUNDLE_BLOBS);
    copy_tree_strict(&blob_directory, &exported_blobs, true)
        .context("failed to copy export blobs")?;
    let asset_manifest = FileManifest {
        schema_version: ASSET_EXPORT_SCHEMA_VERSION.into(),
        files: collect_file_records(&exported_blobs, &[])?,
    };
    write_json_new(&output.join(EXPORT_ASSET_MANIFEST), &asset_manifest)?;
    let rechecked = collect_file_records(&exported_blobs, &[])?;
    compare_file_records(&asset_manifest.files, &rechecked, "exported blob tree")?;

    output_guard.commit();
    println!(
        "exported {} documents and {} blob files to {}",
        export.documents.len(),
        asset_manifest.files.len(),
        output.display()
    );
    Ok(())
}

fn validate_plugin(manifest: PathBuf) -> Result<()> {
    let source = fs::read_to_string(&manifest)
        .with_context(|| format!("failed to read {}", manifest.display()))?;
    let parsed = PluginManifest::from_toml(&source).map_err(anyhow::Error::msg)?;
    println!("plugin manifest valid: {} {}", parsed.id, parsed.version);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileManifest {
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestVersion {
    schema_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyManagedManifest {
    schema_version: String,
    generation: String,
    created_at: String,
    database: FileRecord,
    blobs: Vec<FileRecord>,
}

fn read_bundle_manifest(path: &Path) -> Result<FileManifest> {
    let version: ManifestVersion = read_json_regular(path)?;
    match version.schema_version.as_str() {
        BUNDLE_SCHEMA_VERSION => read_json_regular(path),
        LEGACY_MANAGED_BUNDLE_SCHEMA_VERSION => {
            let managed: LegacyManagedManifest = read_json_regular(path)?;
            normalize_legacy_managed_manifest(managed)
        }
        version => {
            bail!("unsupported manifest version {version:?}; expected {BUNDLE_SCHEMA_VERSION:?}")
        }
    }
}

fn normalize_legacy_managed_manifest(manifest: LegacyManagedManifest) -> Result<FileManifest> {
    ensure!(
        manifest.schema_version == LEGACY_MANAGED_BUNDLE_SCHEMA_VERSION,
        "unsupported legacy managed backup schema"
    );
    ensure!(
        manifest.generation.starts_with("generation-")
            && manifest.generation.len() <= 160
            && manifest
                .generation
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "legacy managed backup generation identifier is invalid"
    );
    ensure!(
        !manifest.created_at.trim().is_empty()
            && manifest.created_at.len() <= 64
            && !manifest.created_at.chars().any(char::is_control),
        "legacy managed backup creation timestamp is invalid"
    );
    ensure!(
        manifest.database.path == BUNDLE_DATABASE,
        "legacy managed backup database path must be {BUNDLE_DATABASE}"
    );
    ensure!(
        manifest
            .blobs
            .iter()
            .all(|entry| entry.path.starts_with("blobs/")),
        "legacy managed backup contains a non-blob payload in its blobs list"
    );
    validate_file_records(std::slice::from_ref(&manifest.database))?;
    validate_file_records(&manifest.blobs)?;

    let mut files = Vec::with_capacity(manifest.blobs.len() + 1);
    files.push(manifest.database);
    files.extend(manifest.blobs);
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let normalized = FileManifest {
        schema_version: BUNDLE_SCHEMA_VERSION.into(),
        files,
    };
    validate_manifest(&normalized, BUNDLE_SCHEMA_VERSION)?;
    Ok(normalized)
}

fn validate_manifest(manifest: &FileManifest, expected_version: &str) -> Result<()> {
    ensure!(
        manifest.schema_version == expected_version,
        "unsupported manifest version {:?}; expected {expected_version:?}",
        manifest.schema_version
    );
    validate_file_records(&manifest.files)
}

fn validate_file_records(files: &[FileRecord]) -> Result<()> {
    let mut previous: Option<&str> = None;
    for entry in files {
        validate_manifest_path(&entry.path)?;
        ensure!(
            valid_sha256(&entry.sha256),
            "manifest has an invalid SHA-256 digest for {:?}",
            entry.path
        );
        if let Some(previous) = previous {
            ensure!(
                previous < entry.path.as_str(),
                "manifest file paths must be unique and sorted"
            );
        }
        previous = Some(&entry.path);
    }
    Ok(())
}

fn collect_file_records(root: &Path, excluded: &[&str]) -> Result<Vec<FileRecord>> {
    ensure_directory_without_symlink(root, "manifest root")?;
    let excluded: BTreeSet<&str> = excluded.iter().copied().collect();
    let mut relative_files = Vec::new();
    collect_regular_files(root, Path::new(""), &mut relative_files)?;
    let mut records = Vec::with_capacity(relative_files.len());
    for relative in relative_files {
        let path = portable_relative_path(&relative)?;
        if excluded.contains(path.as_str()) {
            continue;
        }
        let (sha256, size) = hash_regular_file(&root.join(&relative))?;
        records.push(FileRecord { path, sha256, size });
    }
    records.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(records)
}

fn collect_regular_files(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let directory = root.join(relative);
    let mut entries = fs::read_dir(&directory)
        .with_context(|| format!("failed to read directory {}", directory.display()))?
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let name = portable_component(&entry.file_name())?;
        let child_relative = relative.join(name);
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(
                "symlink is forbidden in portable tree: {}",
                entry.path().display()
            );
        }
        if metadata.is_dir() {
            collect_regular_files(root, &child_relative, files)?;
        } else if metadata.is_file() {
            files.push(child_relative);
        } else {
            bail!(
                "non-regular filesystem entry is forbidden: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn copy_tree_strict(source: &Path, destination: &Path, allow_missing: bool) -> Result<()> {
    fs::create_dir(destination).with_context(|| {
        format!(
            "refusing to merge into destination directory {}",
            destination.display()
        )
    })?;
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if allow_missing && error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "blob source must be a real directory, not a symlink or special entry: {}",
            source.display()
        );
    }
    copy_directory_contents(source, destination)
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<()> {
    let mut entries = fs::read_dir(source)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = portable_component(&entry.file_name())?;
        let source_path = entry.path();
        let destination_path = destination.join(name);
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "symlink is forbidden in blob tree: {}",
                source_path.display()
            );
        }
        if metadata.is_dir() {
            fs::create_dir(&destination_path)?;
            copy_directory_contents(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            copy_regular_new(&source_path, &destination_path)?;
        } else {
            bail!(
                "non-regular entry is forbidden in blob tree: {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

fn copy_regular_new(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "source is not a regular file: {}",
        source.display()
    );
    let mut input = File::open(source)?;
    ensure!(
        input.metadata()?.is_file(),
        "opened source is not a regular file: {}",
        source.display()
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| format!("refusing to overwrite {}", destination.display()))?;
    if let Err(error) = io::copy(&mut input, &mut output).and_then(|_| output.sync_all()) {
        drop(output);
        let _ = fs::remove_file(destination);
        return Err(error.into());
    }
    Ok(())
}

fn copy_verified_new(source: &Path, destination: &Path, expected: &FileRecord) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "verified source is not a regular file: {}",
        source.display()
    );
    let mut input = File::open(source)?;
    ensure!(
        input.metadata()?.is_file(),
        "opened verified source is not a regular file: {}",
        source.display()
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| format!("refusing to overwrite {}", destination.display()))?;
    let result = (|| -> Result<()> {
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
            size += read as u64;
        }
        output.sync_all()?;
        let digest = format!("{:x}", hasher.finalize());
        ensure!(
            size == expected.size && digest == expected.sha256,
            "source changed after bundle verification: {}",
            source.display()
        );
        Ok(())
    })();
    if let Err(error) = result {
        drop(output);
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

fn hash_regular_file(path: &Path) -> Result<(String, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "manifest payload is not a regular file: {}",
        path.display()
    );
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "opened manifest payload is not regular: {}",
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
    Ok((format!("{:x}", hasher.finalize()), size))
}

fn compare_file_records(expected: &[FileRecord], actual: &[FileRecord], label: &str) -> Result<()> {
    ensure!(
        expected == actual,
        "{label} files do not match the manifest (missing, extra, modified, or reordered payload)"
    );
    Ok(())
}

fn verify_bundle_root_layout(bundle: &Path) -> Result<()> {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(bundle)? {
        let entry = entry?;
        let name = portable_component(&entry.file_name())?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(
                "symlink is forbidden in bundle root: {}",
                entry.path().display()
            );
        }
        match name.as_str() {
            BUNDLE_DATABASE | BUNDLE_MANIFEST if metadata.is_file() => {}
            BUNDLE_BLOBS if metadata.is_dir() => {}
            _ => bail!("unexpected bundle root entry: {}", entry.path().display()),
        }
        names.insert(name);
    }
    for required in [BUNDLE_DATABASE, BUNDLE_BLOBS, BUNDLE_MANIFEST] {
        ensure!(
            names.contains(required),
            "bundle is missing required entry {required}"
        );
    }
    Ok(())
}

fn validate_manifest_path(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "manifest path is empty");
    ensure!(
        !value.contains(['\\', '\0', ':']),
        "manifest path is not portable: {value:?}"
    );
    ensure!(
        value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.chars().any(char::is_control)
        }),
        "manifest path is unsafe: {value:?}"
    );
    let path = Path::new(value);
    ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "manifest path is unsafe: {value:?}"
    );
    Ok(())
}

fn manifest_path(value: &str) -> Result<PathBuf> {
    validate_manifest_path(value)?;
    Ok(value.split('/').collect())
}

fn portable_relative_path(path: &Path) -> Result<String> {
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            bail!("path is not relative and portable: {}", path.display());
        };
        components.push(portable_component(component)?);
    }
    let value = components.join("/");
    validate_manifest_path(&value)?;
    Ok(value)
}

fn portable_component(value: &std::ffi::OsStr) -> Result<String> {
    let value = value
        .to_str()
        .context("portable backups require UTF-8 filesystem names")?;
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value.contains(['/', '\\', '\0', ':'])
            && !value.chars().any(char::is_control),
        "filesystem name is not portable: {value:?}"
    );
    Ok(value.into())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("refusing to overwrite manifest {}", path.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_json_regular<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "JSON input is not a regular file: {}",
        path.display()
    );
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse JSON manifest {}", path.display()))
}

fn ensure_regular_source_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label} {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{label} must be a regular file, not a symlink: {}",
        path.display()
    );
    Ok(())
}

fn ensure_directory_without_symlink(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label} {}", path.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a real directory, not a symlink: {}",
        path.display()
    );
    Ok(())
}

fn refuse_existing_path(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("refusing to overwrite existing {label} {}", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reject_nested_output(source: &Path, output: &Path, label: &str) -> Result<()> {
    let source_metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        source_metadata.is_dir() && !source_metadata.file_type().is_symlink(),
        "{label} must be a real directory: {}",
        source.display()
    );
    let source = fs::canonicalize(source)?;
    let output = fs::canonicalize(output)?;
    ensure!(
        !output.starts_with(&source),
        "output directory cannot be inside {label} {}",
        source.display()
    );
    Ok(())
}

fn reject_restore_path_relationships(
    bundle: &Path,
    database: &Path,
    blob_directory: &Path,
) -> Result<()> {
    let bundle = fs::canonicalize(bundle)?;
    let database = canonical_destination(database)?;
    let blob_directory = canonical_destination(blob_directory)?;
    ensure!(
        !database.starts_with(&bundle) && !blob_directory.starts_with(&bundle),
        "restore targets cannot be inside the verified bundle"
    );
    ensure!(
        !database.starts_with(&blob_directory) && !blob_directory.starts_with(&database),
        "target database and blob directory cannot contain one another"
    );
    Ok(())
}

/// Resolves every existing ancestor, including symlinks, while retaining the
/// not-yet-created suffix. This prevents an apparently external restore target
/// from reaching back into the verified bundle through a parent symlink.
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
                    "restore target ancestor is not a directory: {}",
                    ancestor.display()
                );
                let mut destination = resolved;
                for component in missing.iter().rev() {
                    destination.push(component);
                }
                return Ok(destination);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = ancestor.file_name().with_context(|| {
                    format!(
                        "cannot resolve a filesystem ancestor for restore target {}",
                        path.display()
                    )
                })?;
                missing.push(component.to_os_string());
                ancestor = ancestor.parent().with_context(|| {
                    format!(
                        "cannot resolve a filesystem ancestor for restore target {}",
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

struct FreshDirectory {
    path: PathBuf,
    committed: bool,
}

impl FreshDirectory {
    fn create(path: &Path, label: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {label} parent"))?;
        }
        fs::create_dir(path).with_context(|| {
            format!(
                "refusing to merge into existing {label}: {}",
                path.display()
            )
        })?;
        Ok(Self {
            path: path.to_owned(),
            committed: false,
        })
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for FreshDirectory {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct RestoreGuard {
    database: PathBuf,
    blobs: PathBuf,
    database_created: bool,
    blob_created: bool,
    committed: bool,
}

impl RestoreGuard {
    fn new(database: PathBuf, blobs: PathBuf) -> Self {
        Self {
            database,
            blobs,
            database_created: false,
            blob_created: false,
            committed: false,
        }
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if self.database_created {
            let _ = fs::remove_file(&self.database);
        }
        if self.blob_created {
            let _ = fs::remove_dir_all(&self.blobs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_exposes_backup_verify_and_restore_with_legacy_aliases() {
        let parsed = Args::try_parse_from(["osb", "backup", "/backup/database.sqlite3"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Backup {
                output: Some(output),
                action: None
            } if output == PathBuf::from("/backup/database.sqlite3")
        ));

        let parsed =
            Args::try_parse_from(["osb", "backup", "verify", "/backup/generation"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Backup {
                output: None,
                action: Some(BackupAction::Verify { bundle })
            } if bundle == PathBuf::from("/backup/generation")
        ));

        let parsed = Args::try_parse_from(["osb", "restore", "/backup/generation"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Restore { bundle } if bundle == PathBuf::from("/backup/generation")
        ));

        let parsed = Args::try_parse_from(["osb", "restore-bundle", "/backup/generation"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Restore { bundle } if bundle == PathBuf::from("/backup/generation")
        ));
    }

    fn database(path: &Path) {
        let repository = SqliteRepository::open(path).unwrap();
        drop(repository);
    }

    fn blob_fixture(root: &Path) -> PathBuf {
        let directory = root.join("sha256").join("ab");
        fs::create_dir_all(&directory).unwrap();
        let blob = directory.join(format!("{}{}", "ab", "0".repeat(62)));
        fs::write(&blob, b"first-party-image").unwrap();
        fs::write(
            blob.with_file_name(format!("{}{}.json", "ab", "0".repeat(62))),
            b"{\"mediaType\":\"image/png\"}",
        )
        .unwrap();
        blob
    }

    #[test]
    fn bundle_roundtrip_covers_database_and_blob_tree() {
        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        let source_blob = blob_fixture(&source_blobs);
        let bundle = temporary.path().join("bundle");

        backup_bundle(source_database, source_blobs.clone(), bundle.clone()).unwrap();
        let manifest = verify_bundle(&bundle).unwrap();
        assert!(
            manifest
                .files
                .iter()
                .any(|entry| entry.path == BUNDLE_DATABASE)
        );
        assert!(
            manifest
                .files
                .iter()
                .any(|entry| entry.path.starts_with("blobs/"))
        );

        let restored_database = temporary.path().join("restored/open-soverign-blog.db");
        let restored_blobs = temporary.path().join("restored/blobs");
        restore_bundle(
            bundle.clone(),
            restored_database.clone(),
            restored_blobs.clone(),
        )
        .unwrap();
        assert_eq!(
            fs::read(restored_database).unwrap(),
            fs::read(bundle.join(BUNDLE_DATABASE)).unwrap()
        );
        let relative_blob = source_blob.strip_prefix(source_blobs).unwrap();
        assert_eq!(
            fs::read(restored_blobs.join(relative_blob)).unwrap(),
            b"first-party-image"
        );
    }

    #[test]
    fn empty_blob_namespace_survives_backup_verification_and_restore() {
        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        fs::create_dir_all(source_blobs.join("sha256")).unwrap();
        let bundle = temporary.path().join("bundle");
        backup_bundle(source_database, source_blobs, bundle.clone()).unwrap();
        verify_bundle(&bundle).unwrap();

        let restored_database = temporary.path().join("restored/blog.db");
        let restored_blobs = temporary.path().join("restored/blobs");
        restore_bundle(bundle, restored_database, restored_blobs.clone()).unwrap();
        assert!(restored_blobs.join("sha256").is_dir());
    }

    #[test]
    fn legacy_managed_generation_can_be_verified_and_restored() {
        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        let source_blob = blob_fixture(&source_blobs);
        let bundle = temporary.path().join("managed-generation");
        backup_bundle(source_database, source_blobs.clone(), bundle.clone()).unwrap();

        let portable: FileManifest = read_json_regular(&bundle.join(BUNDLE_MANIFEST)).unwrap();
        let database = portable
            .files
            .iter()
            .find(|entry| entry.path == BUNDLE_DATABASE)
            .unwrap()
            .clone();
        let blobs = portable
            .files
            .into_iter()
            .filter(|entry| entry.path.starts_with("blobs/"))
            .collect();
        let legacy = LegacyManagedManifest {
            schema_version: LEGACY_MANAGED_BUNDLE_SCHEMA_VERSION.into(),
            generation: "generation-20260719T120000Z-01900000000070008000000000000000".into(),
            created_at: "2026-07-19T12:00:00Z".into(),
            database,
            blobs,
        };
        fs::write(
            bundle.join(BUNDLE_MANIFEST),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let verified = verify_bundle(&bundle).unwrap();
        assert_eq!(verified.schema_version, BUNDLE_SCHEMA_VERSION);
        let restored_database = temporary.path().join("restored/open-soverign-blog.db");
        let restored_blobs = temporary.path().join("restored/blobs");
        restore_bundle(bundle, restored_database.clone(), restored_blobs.clone()).unwrap();
        assert!(restored_database.is_file());
        let relative_blob = source_blob.strip_prefix(source_blobs).unwrap();
        assert_eq!(
            fs::read(restored_blobs.join(relative_blob)).unwrap(),
            b"first-party-image"
        );
    }

    #[test]
    fn verification_detects_tampering_before_restore_target_checks() {
        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        let source_blob = blob_fixture(&source_blobs);
        let bundle = temporary.path().join("bundle");
        backup_bundle(source_database, source_blobs.clone(), bundle.clone()).unwrap();

        let relative_blob = source_blob.strip_prefix(source_blobs).unwrap();
        fs::write(bundle.join(BUNDLE_BLOBS).join(relative_blob), b"tampered").unwrap();
        let existing_target = temporary.path().join("already.db");
        fs::write(&existing_target, b"do not overwrite").unwrap();
        let error = restore_bundle(
            bundle,
            existing_target,
            temporary.path().join("restored-blobs"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("bundle verification failed"));
    }

    #[test]
    fn verification_rejects_a_corrupt_database_even_when_its_hash_matches() {
        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        blob_fixture(&source_blobs);
        let bundle = temporary.path().join("bundle");
        backup_bundle(source_database, source_blobs, bundle.clone()).unwrap();

        let database_path = bundle.join(BUNDLE_DATABASE);
        fs::write(&database_path, b"not a SQLite database").unwrap();
        let mut manifest: FileManifest = read_json_regular(&bundle.join(BUNDLE_MANIFEST)).unwrap();
        let database = manifest
            .files
            .iter_mut()
            .find(|entry| entry.path == BUNDLE_DATABASE)
            .unwrap();
        let (sha256, size) = hash_regular_file(&database_path).unwrap();
        database.sha256 = sha256;
        database.size = size;
        fs::write(
            bundle.join(BUNDLE_MANIFEST),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = verify_bundle(&bundle).unwrap_err();
        assert!(format!("{error:#}").contains("readable, migrated SQLite snapshot"));
    }

    #[test]
    fn verification_rejects_unknown_schemas_and_unsafe_legacy_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        blob_fixture(&source_blobs);
        let bundle = temporary.path().join("bundle");
        backup_bundle(source_database, source_blobs, bundle.clone()).unwrap();

        let mut manifest: serde_json::Value =
            read_json_regular(&bundle.join(BUNDLE_MANIFEST)).unwrap();
        manifest["schemaVersion"] = "open-soverign-blog-backup/999".into();
        fs::write(
            bundle.join(BUNDLE_MANIFEST),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let error = verify_bundle(&bundle).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported manifest version"));

        let valid_digest = "0".repeat(64);
        let legacy = LegacyManagedManifest {
            schema_version: LEGACY_MANAGED_BUNDLE_SCHEMA_VERSION.into(),
            generation: "generation-20260719T120000Z-01900000000070008000000000000000".into(),
            created_at: "2026-07-19T12:00:00Z".into(),
            database: FileRecord {
                path: BUNDLE_DATABASE.into(),
                sha256: valid_digest.clone(),
                size: 1,
            },
            blobs: vec![FileRecord {
                path: "blobs/../../escape".into(),
                sha256: valid_digest,
                size: 1,
            }],
        };
        let error = normalize_legacy_managed_manifest(legacy).unwrap_err();
        assert!(format!("{error:#}").contains("unsafe"));

        let legacy = LegacyManagedManifest {
            schema_version: LEGACY_MANAGED_BUNDLE_SCHEMA_VERSION.into(),
            generation: "generation-20260719T120000Z-01900000000070008000000000000000".into(),
            created_at: "2026-07-19T12:00:00Z".into(),
            database: FileRecord {
                path: BUNDLE_DATABASE.into(),
                sha256: "0".repeat(64),
                size: 1,
            },
            blobs: vec![
                FileRecord {
                    path: "blobs/z".into(),
                    sha256: "1".repeat(64),
                    size: 1,
                },
                FileRecord {
                    path: "blobs/a".into(),
                    sha256: "2".repeat(64),
                    size: 1,
                },
            ],
        };
        let error = normalize_legacy_managed_manifest(legacy).unwrap_err();
        assert!(format!("{error:#}").contains("unique and sorted"));
    }

    #[test]
    fn manifest_paths_cannot_escape_the_bundle() {
        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        blob_fixture(&source_blobs);
        let bundle = temporary.path().join("bundle");
        backup_bundle(source_database, source_blobs, bundle.clone()).unwrap();

        let manifest_path = bundle.join(BUNDLE_MANIFEST);
        let mut manifest: FileManifest = read_json_regular(&manifest_path).unwrap();
        let blob = manifest
            .files
            .iter_mut()
            .find(|entry| entry.path.starts_with("blobs/"))
            .unwrap();
        blob.path = "blobs/../../escape".into();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = verify_bundle(&bundle).unwrap_err();
        assert!(format!("{error:#}").contains("unsafe"));
        assert!(!temporary.path().join("escape").exists());
    }

    #[test]
    fn backup_and_restore_refuse_existing_destinations() {
        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        blob_fixture(&source_blobs);

        let existing_bundle = temporary.path().join("existing-bundle");
        fs::create_dir(&existing_bundle).unwrap();
        assert!(
            backup_bundle(
                source_database.clone(),
                source_blobs.clone(),
                existing_bundle
            )
            .is_err()
        );

        let bundle = temporary.path().join("bundle");
        backup_bundle(source_database, source_blobs, bundle.clone()).unwrap();
        let target_database = temporary.path().join("target.db");
        fs::write(&target_database, b"existing").unwrap();
        let target_blobs = temporary.path().join("target-blobs");
        assert!(
            restore_bundle(
                bundle.clone(),
                target_database.clone(),
                target_blobs.clone()
            )
            .is_err()
        );
        assert_eq!(fs::read(&target_database).unwrap(), b"existing");
        assert!(!target_blobs.exists());

        fs::remove_file(target_database).unwrap();
        fs::create_dir(&target_blobs).unwrap();
        assert!(restore_bundle(bundle, temporary.path().join("new.db"), target_blobs).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_blob_sources_are_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        fs::create_dir(&source_blobs).unwrap();
        let outside = temporary.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, source_blobs.join("link")).unwrap();

        let error = backup_bundle(
            source_database,
            source_blobs,
            temporary.path().join("bundle"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_payloads_in_stored_bundles_are_rejected_before_restore() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        let source_blob = blob_fixture(&source_blobs);
        let bundle = temporary.path().join("bundle");
        backup_bundle(source_database, source_blobs.clone(), bundle.clone()).unwrap();

        let relative_blob = source_blob.strip_prefix(source_blobs).unwrap();
        let bundled_blob = bundle.join(BUNDLE_BLOBS).join(relative_blob);
        fs::remove_file(&bundled_blob).unwrap();
        let outside = temporary.path().join("outside");
        fs::write(&outside, b"first-party-image").unwrap();
        symlink(&outside, &bundled_blob).unwrap();

        let error = restore_bundle(
            bundle,
            temporary.path().join("restored.db"),
            temporary.path().join("restored-blobs"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("symlink"));
        assert!(!temporary.path().join("restored.db").exists());
        assert!(!temporary.path().join("restored-blobs").exists());
    }

    #[test]
    fn export_copies_blobs_with_a_verified_asset_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        let source_database = temporary.path().join("source.db");
        let source_blobs = temporary.path().join("source-blobs");
        database(&source_database);
        let source_blob = blob_fixture(&source_blobs);
        let output = temporary.path().join("export");

        export(
            source_database,
            source_blobs.clone(),
            Uuid::now_v7(),
            output.clone(),
        )
        .unwrap();
        let manifest: FileManifest =
            read_json_regular(&output.join(EXPORT_ASSET_MANIFEST)).unwrap();
        validate_manifest(&manifest, ASSET_EXPORT_SCHEMA_VERSION).unwrap();
        let relative = source_blob.strip_prefix(source_blobs).unwrap();
        assert_eq!(
            fs::read(output.join(BUNDLE_BLOBS).join(relative)).unwrap(),
            b"first-party-image"
        );
        assert_eq!(
            manifest.files,
            collect_file_records(&output.join(BUNDLE_BLOBS), &[]).unwrap()
        );
    }
}
