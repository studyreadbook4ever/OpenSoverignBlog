use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand, ValueEnum};
use osb_kernel::{
    NewDocument, ProposedRevision, PublicAuthorship, PublicAuthorshipKind, RevisionActor,
    RevisionActorKind,
};
use osb_storage_sqlite::SqliteRepository;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_MARKDOWN_BYTES: u64 = 10 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const CONFIG_SCHEMA: &str = "open-soverign-blog/2";

#[derive(Debug, Args)]
pub(crate) struct LocalArgs {
    /// Trusted semantic deployment config. Delivery-only configs are rejected.
    #[arg(long, env = "OSB_CONFIG", default_value = "config.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    action: LocalAction,
}

#[derive(Debug, Deserialize)]
struct LocalDeploymentBoundary {
    schema_version: String,
    semantic: LocalSemanticBoundary,
    deployment: LocalWriteBoundary,
}

#[derive(Debug, Deserialize)]
struct LocalSemanticBoundary {
    intent: String,
}

#[derive(Debug, Deserialize)]
struct LocalWriteBoundary {
    delivery_only: bool,
}

#[derive(Debug, Subcommand)]
enum LocalAction {
    /// Complete the primary blog's one-time metadata setup without remote auth.
    Setup {
        /// Public handle used in /@handle routes.
        #[arg(long)]
        handle: String,
        /// Public blog title.
        #[arg(long)]
        title: String,
        /// Optional public description.
        #[arg(long)]
        description: Option<String>,
        /// Emit stable JSON for automation.
        #[arg(long)]
        json: bool,
    },
    /// List primary-site documents and their immutable revision identifiers.
    List {
        /// Maximum number of documents to return.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=500))]
        limit: u16,
        /// Emit stable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Create or revise a Markdown document and publish the exact new revision.
    Publish {
        /// Existing document UUID. Omit it to create a document.
        #[arg(long)]
        document_id: Option<Uuid>,
        /// New title. Required when creating; otherwise retains the current title.
        #[arg(long)]
        title: Option<String>,
        /// New URL slug. Required when creating; otherwise retains the current slug.
        #[arg(long)]
        slug: Option<String>,
        /// UTF-8 Markdown regular file, or `-` to read at most 10 MiB from stdin.
        #[arg(long, value_name = "FILE")]
        markdown: PathBuf,
        /// Portable public authorship disclosure.
        #[arg(long, value_enum, default_value_t = AuthorshipChoice::Human)]
        authorship: AuthorshipChoice,
        /// Model/tool/import source for non-human authorship.
        #[arg(long)]
        generator: Option<String>,
        /// Record that a human reviewed non-human-authored content.
        #[arg(long)]
        human_reviewed: bool,
        /// Emit the resulting published document as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AuthorshipChoice {
    Human,
    AiGenerated,
    AiAssisted,
    Imported,
}

impl AuthorshipChoice {
    const fn kind(self) -> PublicAuthorshipKind {
        match self {
            Self::Human => PublicAuthorshipKind::Human,
            Self::AiGenerated => PublicAuthorshipKind::AiGenerated,
            Self::AiAssisted => PublicAuthorshipKind::AiAssisted,
            Self::Imported => PublicAuthorshipKind::Imported,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentListEntry {
    id: Uuid,
    status: osb_kernel::DocumentStatus,
    title: String,
    slug: String,
    current_revision_id: Uuid,
    published_revision_id: Option<Uuid>,
}

pub(crate) fn run(database: PathBuf, args: LocalArgs) -> Result<()> {
    ensure_local_writes_allowed(&args.config)?;
    let repository = SqliteRepository::open(&database)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("failed to open local database {}", database.display()))?;
    let control = repository
        .get_admin_control_plane()
        .map_err(anyhow::Error::msg)
        .context(
            "the primary site is not initialized; start the writable server once, then stop it before running local maintenance",
        )?;

    match args.action {
        LocalAction::Setup {
            handle,
            title,
            description,
            json,
        } => {
            let current = repository
                .get_site_by_id(control.primary_site_id)
                .map_err(anyhow::Error::msg)?;
            let site = repository
                .complete_primary_owner_setup(
                    control.owner_user_id,
                    &handle,
                    &title,
                    description.as_deref(),
                    current.theme_profile,
                )
                .map_err(anyhow::Error::msg)
                .context("failed to complete the primary blog setup")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&site)?);
            } else {
                println!("primary blog configured: @{} · {}", site.handle, site.title);
            }
            Ok(())
        }
        LocalAction::List { limit, json } => {
            let documents = repository
                .list_documents_in_owned_site(
                    control.owner_user_id,
                    control.primary_site_id,
                    usize::from(limit),
                )
                .map_err(anyhow::Error::msg)?;
            let entries = documents
                .into_iter()
                .map(|document| DocumentListEntry {
                    id: document.id,
                    status: document.status,
                    title: document.revision.title,
                    slug: document.revision.slug,
                    current_revision_id: document.current_revision_id,
                    published_revision_id: document.published_revision_id,
                })
                .collect::<Vec<_>>();
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("no documents in the primary blog");
            } else {
                println!("DOCUMENT ID\tSTATUS\tSLUG\tTITLE");
                for entry in entries {
                    println!(
                        "{}\t{:?}\t{}\t{}",
                        entry.id, entry.status, entry.slug, entry.title
                    );
                }
            }
            Ok(())
        }
        LocalAction::Publish {
            document_id,
            title,
            slug,
            markdown,
            authorship,
            generator,
            human_reviewed,
            json,
        } => {
            let markdown = read_markdown(&markdown)?;
            let authorship = resolve_authorship(authorship, generator, human_reviewed)?;
            let actor = RevisionActor {
                kind: RevisionActorKind::System,
                id: "local-cli".into(),
                display_name: Some("Server-local administrator".into()),
            };
            let published = if let Some(document_id) = document_id {
                let current = repository
                    .get_document_in_owned_site(
                        control.owner_user_id,
                        control.primary_site_id,
                        document_id,
                    )
                    .map_err(anyhow::Error::msg)
                    .context("the requested primary-site document was not found")?;
                let revision = repository
                    .revise_document_in_owned_site(
                        control.owner_user_id,
                        control.primary_site_id,
                        ProposedRevision {
                            document_id,
                            base_revision_id: current.current_revision_id,
                            title: title.unwrap_or(current.revision.title),
                            slug: slug.unwrap_or(current.revision.slug),
                            source_markdown: markdown,
                            embeds: current.revision.embeds,
                            intent: current.revision.intent,
                            ontology: current.revision.ontology,
                            authorship,
                            actor,
                            idempotency_key: None,
                        },
                    )
                    .map_err(anyhow::Error::msg)
                    .context("failed to append the local revision")?;
                repository
                    .publish_document_in_owned_site(
                        control.owner_user_id,
                        control.primary_site_id,
                        document_id,
                        revision.id,
                    )
                    .map_err(anyhow::Error::msg)?
            } else {
                let title = title.context("--title is required when creating a document")?;
                let slug = slug.context("--slug is required when creating a document")?;
                let document = repository
                    .create_document_in_owned_site(
                        control.owner_user_id,
                        NewDocument {
                            site_id: control.primary_site_id,
                            title,
                            slug,
                            source_markdown: markdown,
                            embeds: Vec::new(),
                            intent: None,
                            ontology: None,
                            authorship,
                            actor,
                        },
                    )
                    .map_err(anyhow::Error::msg)
                    .context("failed to create the local document")?;
                repository
                    .publish_document_in_owned_site(
                        control.owner_user_id,
                        control.primary_site_id,
                        document.id,
                        document.current_revision_id,
                    )
                    .map_err(anyhow::Error::msg)?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&published)?);
            } else {
                println!(
                    "published: {} · document={} · revision={}",
                    published.revision.title,
                    published.id,
                    published
                        .published_revision_id
                        .expect("the repository returned a published document")
                );
            }
            Ok(())
        }
    }
}

fn ensure_local_writes_allowed(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed to inspect local-maintenance config {}",
            path.display()
        )
    })?;
    ensure!(
        metadata.file_type().is_file(),
        "local-maintenance config must be a regular non-symlink file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_CONFIG_BYTES,
        "local-maintenance config exceeds 256 KiB"
    );
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read local-maintenance config {}", path.display()))?;
    ensure!(
        u64::try_from(source.len()).unwrap_or(u64::MAX) <= MAX_CONFIG_BYTES,
        "local-maintenance config grew beyond 256 KiB while being read"
    );
    let boundary: LocalDeploymentBoundary = toml::from_str(&source)
        .with_context(|| format!("invalid local-maintenance config {}", path.display()))?;
    ensure!(
        boundary.schema_version == CONFIG_SCHEMA,
        "local maintenance requires config schema {CONFIG_SCHEMA}"
    );
    ensure!(
        matches!(boundary.semantic.intent.as_str(), "personal" | "community"),
        "local maintenance is forbidden for semantic intent {:?}",
        boundary.semantic.intent
    );
    ensure!(
        !boundary.deployment.delivery_only,
        "local maintenance is forbidden for a delivery-only deployment"
    );
    Ok(())
}

fn resolve_authorship(
    choice: AuthorshipChoice,
    generator: Option<String>,
    human_reviewed: bool,
) -> Result<PublicAuthorship> {
    if choice == AuthorshipChoice::Human {
        ensure!(
            generator.is_none(),
            "--generator is only valid for non-human authorship"
        );
        ensure!(
            !human_reviewed,
            "--human-reviewed is only valid for non-human authorship"
        );
    } else {
        ensure!(
            generator
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty()),
            "non-human authorship requires --generator"
        );
    }
    let value = PublicAuthorship {
        kind: choice.kind(),
        generator,
        human_reviewed,
    };
    ensure!(
        value.generator.as_ref().is_none_or(
            |generator| generator.len() <= 300 && !generator.chars().any(char::is_control)
        ),
        "--generator must contain at most 300 printable characters"
    );
    Ok(value)
}

fn read_markdown(path: &Path) -> Result<String> {
    let bytes = if path == Path::new("-") {
        let mut bytes = Vec::new();
        io::stdin()
            .take(MAX_MARKDOWN_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("failed to read Markdown from stdin")?;
        ensure!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_MARKDOWN_BYTES,
            "Markdown from stdin exceeds 10 MiB"
        );
        bytes
    } else {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect Markdown input {}", path.display()))?;
        ensure!(
            metadata.file_type().is_file(),
            "Markdown input must be a regular non-symlink file: {}",
            path.display()
        );
        ensure!(
            metadata.len() <= MAX_MARKDOWN_BYTES,
            "Markdown exceeds 10 MiB"
        );
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read Markdown input {}", path.display()))?;
        ensure!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_MARKDOWN_BYTES,
            "Markdown grew beyond 10 MiB while being read"
        );
        bytes
    };
    String::from_utf8(bytes).context("Markdown must be UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use osb_kernel::{EmbedReference, IntentLayer, OntologySidecar, OntologyStatement};
    use osb_storage_sqlite::{AdminAuthMode, PrimaryOwnerBootstrap, ThemeProfile};
    use std::collections::BTreeMap;
    use url::Url;

    fn repository(path: &Path) -> SqliteRepository {
        let repository = SqliteRepository::open(path).unwrap();
        let site_id = Uuid::now_v7();
        repository
            .provision_primary_owner_site(
                &PrimaryOwnerBootstrap {
                    site_id,
                    site_handle: "unconfigured-blog".into(),
                    site_title: "My blog".into(),
                    site_description: None,
                    owner_display_name: "Owner".into(),
                    theme_profile: ThemeProfile::Forest,
                },
                AdminAuthMode::Disabled,
                &[7; 32],
            )
            .unwrap();
        repository
    }

    fn writable_config(root: &Path) -> PathBuf {
        let path = root.join("config.toml");
        fs::write(
            &path,
            r#"schema_version = "open-soverign-blog/2"

[semantic]
intent = "personal"

[deployment]
delivery_only = false
"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn local_setup_preserves_the_installed_theme() {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("blog.db");
        let repository = repository(&database);
        drop(repository);

        run(
            database.clone(),
            LocalArgs {
                config: writable_config(temporary.path()),
                action: LocalAction::Setup {
                    handle: "my-notes".into(),
                    title: "My Notes".into(),
                    description: Some("Owned here".into()),
                    json: false,
                },
            },
        )
        .unwrap();

        let repository = SqliteRepository::open(database).unwrap();
        let control = repository.get_admin_control_plane().unwrap();
        let site = repository.get_site_by_id(control.primary_site_id).unwrap();
        assert_eq!(site.handle, "my-notes");
        assert_eq!(site.theme_profile, ThemeProfile::Forest);
        assert!(control.setup_complete);
    }

    #[test]
    fn local_publish_creates_and_revises_the_exact_document() {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("blog.db");
        let repository = repository(&database);
        let control = repository.get_admin_control_plane().unwrap();
        drop(repository);
        let markdown = temporary.path().join("post.md");
        fs::write(&markdown, "# First\n").unwrap();

        run(
            database.clone(),
            LocalArgs {
                config: writable_config(temporary.path()),
                action: LocalAction::Publish {
                    document_id: None,
                    title: Some("First".into()),
                    slug: Some("first".into()),
                    markdown: markdown.clone(),
                    authorship: AuthorshipChoice::Human,
                    generator: None,
                    human_reviewed: false,
                    json: false,
                },
            },
        )
        .unwrap();
        let repository = SqliteRepository::open(&database).unwrap();
        let first = repository
            .list_documents_in_owned_site(control.owner_user_id, control.primary_site_id, 10)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(first.published_revision_id, Some(first.current_revision_id));
        let preserved_embeds = vec![EmbedReference {
            id: "video".into(),
            provider: "youtube".into(),
            resource_id: "dQw4w9WgXcQ".into(),
            canonical_url: Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap(),
            title: "Preserved video".into(),
            consent_purpose_ids: vec!["external_media".into()],
        }];
        let preserved_intent = IntentLayer {
            format: "html".into(),
            source_html: "<p>Preserved intent</p>".into(),
            renderer_hints: BTreeMap::from([("density".into(), "compact".into())]),
            provenance: None,
        };
        let preserved_ontology = OntologySidecar {
            schema: "urn:test:ontology:v1".into(),
            statements: vec![OntologyStatement {
                subject: "document".into(),
                predicate: "test:preserved".into(),
                object: serde_json::json!(true),
                evidence: Some("local CLI regression".into()),
                confirmed_by_author: true,
            }],
        };
        let enriched = repository
            .revise_document_in_owned_site(
                control.owner_user_id,
                control.primary_site_id,
                ProposedRevision {
                    document_id: first.id,
                    base_revision_id: first.current_revision_id,
                    title: first.revision.title.clone(),
                    slug: first.revision.slug.clone(),
                    source_markdown: first.revision.source_markdown.clone(),
                    embeds: preserved_embeds.clone(),
                    intent: Some(preserved_intent.clone()),
                    ontology: Some(preserved_ontology.clone()),
                    authorship: first.revision.authorship.clone(),
                    actor: first.revision.actor.clone(),
                    idempotency_key: None,
                },
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                control.owner_user_id,
                control.primary_site_id,
                first.id,
                enriched.id,
            )
            .unwrap();
        let first_revision = enriched.id;
        fs::write(&markdown, "# Revised\n").unwrap();
        drop(repository);

        run(
            database.clone(),
            LocalArgs {
                config: writable_config(temporary.path()),
                action: LocalAction::Publish {
                    document_id: Some(first.id),
                    title: None,
                    slug: None,
                    markdown,
                    authorship: AuthorshipChoice::AiAssisted,
                    generator: Some("local-test-agent".into()),
                    human_reviewed: true,
                    json: false,
                },
            },
        )
        .unwrap();
        let repository = SqliteRepository::open(database).unwrap();
        let revised = repository
            .get_document_in_owned_site(control.owner_user_id, control.primary_site_id, first.id)
            .unwrap();
        assert_ne!(revised.current_revision_id, first_revision);
        assert_eq!(
            revised.published_revision_id,
            Some(revised.current_revision_id)
        );
        assert_eq!(revised.revision.source_markdown, "# Revised\n");
        assert_eq!(
            revised.revision.authorship.kind,
            PublicAuthorshipKind::AiAssisted
        );
        assert_eq!(revised.revision.embeds, preserved_embeds);
        assert_eq!(revised.revision.intent, Some(preserved_intent));
        assert_eq!(revised.revision.ontology, Some(preserved_ontology));
    }

    #[cfg(unix)]
    #[test]
    fn markdown_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.md");
        let link = temporary.path().join("link.md");
        fs::write(&source, "hello").unwrap();
        symlink(source, &link).unwrap();
        assert!(
            read_markdown(&link)
                .unwrap_err()
                .to_string()
                .contains("non-symlink")
        );
    }

    #[test]
    fn delivery_config_is_rejected_before_local_sqlite_is_opened() {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("delivery.toml");
        fs::write(
            &config,
            r#"schema_version = "open-soverign-blog/2"

[semantic]
intent = "delivery"

[deployment]
delivery_only = true
"#,
        )
        .unwrap();
        let missing_database = temporary.path().join("must-not-be-created.db");
        let error = run(
            missing_database.clone(),
            LocalArgs {
                config,
                action: LocalAction::List {
                    limit: 1,
                    json: false,
                },
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("forbidden"));
        assert!(!missing_database.exists());
    }
}
