use std::{
    fs::{self, File, Metadata},
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::Args;
use osb_storage_sqlite::{
    OfflineImportAlias, OfflineImportBatch, OfflineImportCategory, OfflineImportPost,
    SqliteRepository,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MANIFEST_SCHEMA: &str = "open-soverign-blog-offline-import/1";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_MARKDOWN_BYTES: u64 = 10 * 1024 * 1024;
const MAX_BATCH_MARKDOWN_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Args)]
pub(crate) struct OfflineImportArgs {
    /// Versioned JSON manifest; Markdown paths are resolved relative to it.
    #[arg(long, value_name = "FILE")]
    pub(crate) manifest: PathBuf,
    /// Exercise all reads, validation, constraints, and SQL, then roll back.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Emit the stable import report as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportManifest {
    schema_version: String,
    source: String,
    owner_display_name: String,
    #[serde(default)]
    default_author: Option<ImportAuthor>,
    #[serde(default)]
    categories: Vec<ManifestCategory>,
    posts: Vec<ManifestPost>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportAuthor {
    id: String,
    display_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestCategory {
    slug: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestPost {
    source_id: String,
    title: String,
    slug: String,
    markdown_path: PathBuf,
    #[serde(default)]
    content_sha256: Option<String>,
    created_at: DateTime<Utc>,
    #[serde(default)]
    author: Option<ImportAuthor>,
    primary_category: String,
    #[serde(default)]
    human_reviewed: bool,
    #[serde(default)]
    legacy_paths: Vec<ManifestLegacyPath>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestLegacyPath {
    path: String,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
}

pub(crate) fn run(
    repository: &SqliteRepository,
    owner_user_id: Uuid,
    site_id: Uuid,
    article_route_root: &str,
    args: OfflineImportArgs,
) -> Result<()> {
    let batch = load_batch(&args.manifest)?;
    let report = repository
        .import_offline_batch_with_reserved_roots(
            owner_user_id,
            site_id,
            batch,
            &[article_route_root],
            args.dry_run,
        )
        .map_err(anyhow::Error::msg)
        .with_context(|| {
            if args.dry_run {
                "offline import dry run failed"
            } else {
                "offline import failed; the complete batch was rolled back"
            }
        })?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let action = if args.dry_run {
            "dry run passed"
        } else {
            "offline import complete"
        };
        println!(
            "{action}: posts={} imported, {} unchanged · categories={} created, {} reused · aliases={} · owner-display-updated={}",
            report.posts_imported,
            report.posts_unchanged,
            report.categories_created,
            report.categories_reused,
            report.aliases_created,
            report.owner_display_name_updated,
        );
        for post in report.posts {
            println!(
                "{:?}\t{}\t{}",
                post.status, post.canonical_path, post.source_id
            );
        }
    }
    Ok(())
}

fn load_batch(manifest_path: &Path) -> Result<OfflineImportBatch> {
    let manifest_bytes =
        read_bounded_regular_file(manifest_path, MAX_MANIFEST_BYTES, "offline import manifest")?;
    let manifest: ImportManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("invalid import manifest {}", manifest_path.display()))?;
    ensure!(
        manifest.schema_version == MANIFEST_SCHEMA,
        "offline import requires schemaVersion {MANIFEST_SCHEMA}"
    );
    let manifest_directory = manifest_parent(manifest_path);
    let canonical_directory = manifest_directory.canonicalize().with_context(|| {
        format!(
            "failed to resolve manifest directory {}",
            manifest_directory.display()
        )
    })?;
    let ImportManifest {
        source,
        owner_display_name,
        default_author,
        categories,
        posts,
        ..
    } = manifest;
    let categories = categories
        .into_iter()
        .map(|category| OfflineImportCategory {
            slug: category.slug,
            title: category.title,
            description: category.description,
        })
        .collect();
    let mut total_markdown_bytes = 0_u64;
    let posts = posts
        .into_iter()
        .map(|post| {
            let author = post
                .author
                .or_else(|| default_author.clone())
                .with_context(|| {
                    format!(
                        "post sourceId '{}' needs author or manifest defaultAuthor",
                        post.source_id
                    )
                })?;
            let markdown = read_manifest_markdown(&canonical_directory, &post.markdown_path)
                .with_context(|| format!("failed to load post sourceId '{}'", post.source_id))?;
            if let Some(expected) = post.content_sha256.as_deref() {
                let expected = expected.strip_prefix("sha256:").unwrap_or(expected);
                ensure!(
                    expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "post sourceId '{}' has an invalid contentSha256",
                    post.source_id
                );
                let actual = format!("{:x}", Sha256::digest(markdown.as_bytes()));
                ensure!(
                    actual.eq_ignore_ascii_case(expected),
                    "post sourceId '{}' contentSha256 does not match {}",
                    post.source_id,
                    post.markdown_path.display()
                );
            }
            total_markdown_bytes = total_markdown_bytes
                .checked_add(u64::try_from(markdown.len()).unwrap_or(u64::MAX))
                .context("offline import Markdown size overflow")?;
            ensure!(
                total_markdown_bytes <= MAX_BATCH_MARKDOWN_BYTES,
                "offline import Markdown exceeds 256 MiB in total"
            );
            let aliases = post
                .legacy_paths
                .into_iter()
                .map(|alias| OfflineImportAlias {
                    path: alias.path,
                    created_at: alias.created_at.unwrap_or(post.created_at),
                })
                .collect();
            Ok(OfflineImportPost {
                source_id: post.source_id,
                title: post.title,
                slug: post.slug,
                source_markdown: markdown,
                created_at: post.created_at,
                author_id: author.id,
                author_display_name: author.display_name,
                primary_category: post.primary_category,
                human_reviewed: post.human_reviewed,
                aliases,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(OfflineImportBatch {
        source,
        owner_display_name,
        categories,
        posts,
    })
}

fn manifest_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn read_manifest_markdown(base: &Path, relative_path: &Path) -> Result<String> {
    ensure!(
        !relative_path.as_os_str().is_empty()
            && relative_path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "markdownPath must be a normalized relative path without '.' or '..': {}",
        relative_path.display()
    );
    let candidate = base.join(relative_path);
    let canonical_candidate = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve Markdown input {}", candidate.display()))?;
    ensure!(
        canonical_candidate.starts_with(base),
        "Markdown input escapes the manifest directory: {}",
        relative_path.display()
    );
    let bytes = read_bounded_regular_file(&candidate, MAX_MARKDOWN_BYTES, "Markdown input")?;
    String::from_utf8(bytes).context("Markdown input must be UTF-8")
}

fn read_bounded_regular_file(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>> {
    read_bounded_regular_file_after_inspect(path, limit, label, || {})
}

fn read_bounded_regular_file_after_inspect(
    path: &Path,
    limit: u64,
    label: &str,
    after_inspect: impl FnOnce(),
) -> Result<Vec<u8>> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label} {}", path.display()))?;
    ensure!(
        path_metadata.file_type().is_file(),
        "{label} must be a regular non-symlink file: {}",
        path.display()
    );
    ensure!(
        path_metadata.len() <= limit,
        "{label} exceeds {limit} bytes"
    );
    after_inspect();

    let mut file =
        File::open(path).with_context(|| format!("failed to open {label} {}", path.display()))?;
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {label} {}", path.display()))?;
    let current_path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to re-inspect {label} {}", path.display()))?;
    ensure_stable_regular_file(
        path,
        label,
        limit,
        &path_metadata,
        &opened_metadata,
        &current_path_metadata,
    )?;

    let mut bytes =
        Vec::with_capacity(usize::try_from(opened_metadata.len().min(limit)).unwrap_or(0));
    (&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {label} {}", path.display()))?;
    ensure!(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= limit,
        "{label} grew beyond {limit} bytes while being read"
    );

    let opened_after_read = file
        .metadata()
        .with_context(|| format!("failed to re-inspect opened {label} {}", path.display()))?;
    let path_after_read = fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed to re-inspect {label} {} after reading",
            path.display()
        )
    })?;
    ensure_stable_regular_file(
        path,
        label,
        limit,
        &opened_metadata,
        &opened_after_read,
        &path_after_read,
    )?;
    Ok(bytes)
}

fn ensure_stable_regular_file(
    path: &Path,
    label: &str,
    limit: u64,
    expected: &Metadata,
    opened: &Metadata,
    current_path: &Metadata,
) -> Result<()> {
    ensure!(
        expected.file_type().is_file()
            && opened.is_file()
            && current_path.file_type().is_file()
            && same_file_identity(expected, opened)
            && same_file_identity(opened, current_path)
            && expected.len() == opened.len()
            && opened.len() == current_path.len()
            && opened.len() <= limit,
        "{label} must stay the same regular non-symlink file and not exceed {limit} bytes: {}",
        path.display()
    );
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(left: &Metadata, right: &Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(markdown_path: &str) -> String {
        format!(
            r#"{{
  "schemaVersion": "{MANIFEST_SCHEMA}",
  "source": "legacy-static-site",
  "ownerDisplayName": "me",
  "defaultAuthor": {{"id": "me", "displayName": "me"}},
  "categories": [{{"slug": "ontology", "title": "Ontology"}}],
  "posts": [{{
    "sourceId": "ontology:intro",
    "title": "Intro",
    "slug": "intro",
    "markdownPath": "{markdown_path}",
    "createdAt": "2020-01-02T03:04:05Z",
    "primaryCategory": "ontology",
    "legacyPaths": [{{"path": "topics/ontology/intro.html"}}]
  }}]
}}"#
        )
    }

    #[test]
    fn manifest_materializes_relative_markdown_and_alias_time() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("posts")).unwrap();
        fs::write(temporary.path().join("posts/intro.md"), "# Intro\n").unwrap();
        let manifest_path = temporary.path().join("import.json");
        fs::write(&manifest_path, manifest("posts/intro.md")).unwrap();

        let batch = load_batch(&manifest_path).unwrap();
        assert_eq!(batch.owner_display_name, "me");
        assert_eq!(batch.posts[0].source_markdown, "# Intro\n");
        assert_eq!(
            batch.posts[0].aliases[0].created_at,
            batch.posts[0].created_at
        );
    }

    #[test]
    fn basename_manifest_uses_the_current_working_directory() {
        let parent = manifest_parent(Path::new("import.json"));

        assert_eq!(parent, Path::new("."));
        assert_eq!(
            parent.canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    #[test]
    fn traversal_and_unknown_manifest_fields_are_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("outside.md"), "outside").unwrap();
        let manifest_path = temporary.path().join("import.json");
        fs::write(&manifest_path, manifest("../outside.md")).unwrap();
        assert!(
            load_batch(&manifest_path)
                .unwrap_err()
                .to_string()
                .contains("sourceId")
        );

        let invalid = manifest("outside.md").replacen(
            "\"source\": \"legacy-static-site\",",
            "\"source\": \"legacy-static-site\", \"surprise\": true,",
            1,
        );
        fs::write(&manifest_path, invalid).unwrap();
        assert!(load_batch(&manifest_path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn markdown_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("real.md"), "hello").unwrap();
        symlink("real.md", temporary.path().join("link.md")).unwrap();
        let manifest_path = temporary.path().join("import.json");
        fs::write(&manifest_path, manifest("link.md")).unwrap();
        assert!(
            load_batch(&manifest_path)
                .unwrap_err()
                .to_string()
                .contains("sourceId")
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacement_between_inspection_and_open_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("manifest.json");
        let original = temporary.path().join("original.json");
        fs::write(&path, "original").unwrap();

        let error = read_bounded_regular_file_after_inspect(
            &path,
            MAX_MANIFEST_BYTES,
            "offline import manifest",
            || {
                fs::rename(&path, &original).unwrap();
                fs::write(&path, "replacement").unwrap();
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("same regular non-symlink file"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_swap_between_inspection_and_open_is_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("post.md");
        let original = temporary.path().join("original.md");
        fs::write(&path, "original").unwrap();

        let error = read_bounded_regular_file_after_inspect(
            &path,
            MAX_MARKDOWN_BYTES,
            "Markdown input",
            || {
                fs::rename(&path, &original).unwrap();
                symlink(&original, &path).unwrap();
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("same regular non-symlink file"));
    }
}
