#[cfg(not(target_os = "linux"))]
compile_error!("osb-server currently supports Linux deployments only");

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{any, get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use osb_assets_fs::{AssetError, AssetStore};
use osb_feature_code_runner_client::{
    CodeRunnerClient, QueuedRun, RemoteRunnerClient, RunLimits, RunSubmissionResult, RunnerError,
    SubmissionContext, TerminalRun,
};
use osb_feature_seo::SeoPolicy;
use osb_kernel::{
    AI2AI_SPEC_VERSION, Ai2AiEnvelope, AiSummary, ContentRepository, IntentLayer, NewDocument,
    OntologySidecar, ProposedRevision, PublicAuthorship, PublicAuthorshipKind, RepositoryError,
    RevisionActor, RevisionActorKind,
};
use osb_renderer::{PublishArtifact, ViewMode, render_revision, summarize_markdown};
use osb_storage_sqlite::{
    AdminAuthMode as StoredAdminAuthMode, PrimaryOwnerBootstrap, SqliteDurabilityProfile,
    SqliteRepository, ThemeProfile,
};
use percent_encoding::percent_decode_str;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};
use tracing::info;
use tracing_subscriber::EnvFilter;
use url::Url;
use uuid::Uuid;

mod admin_auth;
mod admin_tree;
mod admission;
mod ai_summary;
mod backup;
mod cache;
mod community;
mod config;
mod feature_registry;
mod installation;
mod references;
mod social_embeds;
mod version;

use admin_auth::AdminAuthRuntime;
use ai_summary::AiSummaryService;
use backup::BackupService;
use cache::SemanticCache;
use config::{AdminAuthMode, AuthMode, DatabaseProfile, RuntimeConfig};
use feature_registry::{FeatureRegistry, ModuleDescriptor, ModuleStatus};
use installation::InstallationRuntime;
use references::ReferencesPage;
use version::VersionService;

#[cfg(test)]
const DEFAULT_SITE_ID: &str = "00000000-0000-7000-8000-000000000001";
#[cfg(test)]
const TEST_SOURCE_WEB_INDEX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../web/index.html");

const SECURITY_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; style-src-elem 'self'; style-src-attr 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-src https://www.youtube-nocookie.com; base-uri 'self'; form-action 'self'; frame-ancestors 'self'; object-src 'none'";
const BUILD_WEB_DIST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../web/dist");
const PASSWORD_WORKER_LIMIT: usize = 4;
const CACHE_FILL_LIMIT: usize = 64;
const PRIMARY_CATEGORY_LOOKUP_LIMIT: usize = 16;
const LEGACY_ALIAS_LOOKUP_LIMIT: usize = 8;
const CATEGORY_NAMESPACE_SCAN_LIMIT: usize = 50_000;
const CATEGORY_NAMESPACE_PAGE_SIZE: usize = 500;
const SITEMAP_URL_LIMIT: usize = 50_000;
const PUBLIC_HTML_CACHE: &str = "public, max-age=0, s-maxage=60, stale-while-revalidate=300";

fn ensure_same_admin_module_rotation(
    persisted: StoredAdminAuthMode,
    requested: StoredAdminAuthMode,
) -> Result<()> {
    if persisted != requested {
        anyhow::bail!(
            "OSB_ADMIN_AUTH_ROTATE only rotates a key or provider binding within the persisted '{}' administrator module; changing auth mode to '{}' requires a new installation contract and rebootstrap",
            persisted.as_str(),
            requested.as_str()
        );
    }
    Ok(())
}

fn ensure_article_category_namespace(
    repository: &SqliteRepository,
    site_id: Uuid,
    article_base_path: &str,
) -> Result<()> {
    let article_root = article_base_path
        .trim_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    match persisted_category_by_slug(repository, site_id, article_root) {
        Ok(category) => anyhow::bail!(
            "configured article base path '{}' starts with persisted category '{}' ({})",
            article_base_path,
            category.slug,
            category.id
        ),
        // A route segment rejected by the category validator (including the
        // default `blog` article root) cannot be created as a category, so it
        // cannot collide with the category namespace.
        Err(RepositoryError::NotFound | RepositoryError::Validation(_)) => Ok(()),
        Err(error) => Err(anyhow::Error::msg(error.to_string()))
            .context("failed to validate the article/category route namespace"),
    }
}

/// Reads persisted category identity without applying the current create-time
/// slug validator. This keeps upgraded databases inspectable when a newly
/// reserved application route existed as a category in an older release.
fn persisted_category_by_slug(
    repository: &SqliteRepository,
    site_id: Uuid,
    slug: &str,
) -> Result<osb_storage_sqlite::CategoryRecord, RepositoryError> {
    let mut offset = 0;
    loop {
        let page = repository.list_category_metadata_page(
            site_id,
            true,
            offset,
            CATEGORY_NAMESPACE_PAGE_SIZE,
        )?;
        if let Some(category) = page.iter().find(|category| category.slug == slug) {
            return repository.get_category_by_id(site_id, category.id);
        }
        if page.len() < CATEGORY_NAMESPACE_PAGE_SIZE {
            return Err(RepositoryError::NotFound);
        }
        offset += page.len();
        if offset >= CATEGORY_NAMESPACE_SCAN_LIMIT {
            return Err(RepositoryError::Storage(format!(
                "category namespace exceeds the {CATEGORY_NAMESPACE_SCAN_LIMIT} entry startup validation limit"
            )));
        }
    }
}

fn ensure_references_namespace(
    repository: &SqliteRepository,
    site_id: Uuid,
    references_enabled: bool,
) -> Result<()> {
    if !references_enabled {
        return Ok(());
    }
    match persisted_category_by_slug(repository, site_id, "references") {
        Ok(category) => anyhow::bail!(
            "global references page cannot claim '/references': persisted category '{}' ({}) already uses that route; disable [references] or migrate the category first",
            category.slug,
            category.id
        ),
        Err(RepositoryError::NotFound) => {}
        Err(error) => {
            return Err(anyhow::Error::msg(error.to_string()))
                .context("failed to inspect the persisted category namespace for /references");
        }
    }
    match repository.published_noncanonical_route_exists(site_id, "references") {
        Ok(true) => anyhow::bail!(
            "global references page cannot claim '/references': a published legacy alias already uses that route; disable [references] or migrate the alias first"
        ),
        Ok(false) => Ok(()),
        Err(error) => Err(anyhow::Error::msg(error.to_string())).context(
            "failed to inspect the persisted noncanonical publication namespace for /references",
        ),
    }
}

#[derive(Clone)]
struct AppState {
    repository: Arc<SqliteRepository>,
    site_id: Uuid,
    seo_policy: Arc<SeoPolicy>,
    #[cfg(test)]
    test_owner_bearer_hash: Option<[u8; 32]>,
    mcp_token_hash: Option<[u8; 32]>,
    admin_auth: AdminAuthRuntime,
    features: Arc<FeatureRegistry>,
    ai_summary: Option<AiSummaryService>,
    runner: Option<Arc<RemoteRunnerClient>>,
    runner_jobs: Arc<tokio::sync::Mutex<HashMap<Uuid, QueuedRun>>>,
    assets: Arc<AssetStore>,
    cache: Option<SemanticCache>,
    cache_signing_key: Arc<[u8; 32]>,
    cache_fill_slots: Arc<tokio::sync::Semaphore>,
    primary_category_slots: Arc<tokio::sync::Semaphore>,
    legacy_alias_slots: Arc<tokio::sync::Semaphore>,
    backup: Option<BackupService>,
    registration_open: bool,
    local_auth_enabled: bool,
    oauth_requested: bool,
    comments_enabled: bool,
    collaboration_enabled: bool,
    custom_css_enabled: bool,
    custom_css_file: Arc<std::path::PathBuf>,
    references: Option<Arc<ReferencesPage>>,
    agent_discovery_enabled: bool,
    delivery_only: bool,
    secure_session_cookie: bool,
    member_auth_admission: community::MemberAuthAdmission,
    ai_summary_admission: community::AiSummaryAdmission,
    password_workers: Arc<tokio::sync::Semaphore>,
    version: VersionService,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationPrincipal {
    HumanOwner,
    McpAgent,
}

impl MutationPrincipal {
    fn revision_actor(self) -> RevisionActor {
        match self {
            Self::HumanOwner => RevisionActor {
                kind: RevisionActorKind::Human,
                id: "owner".into(),
                display_name: None,
            },
            Self::McpAgent => RevisionActor {
                kind: RevisionActorKind::Agent,
                id: "osb-mcp".into(),
                display_name: None,
            },
        }
    }

    fn resolve_authorship(
        self,
        authorship: Option<PublicAuthorship>,
    ) -> Result<PublicAuthorship, ApiError> {
        match (self, authorship) {
            (Self::McpAgent, None) => Err(ApiError::BadRequest(
                "MCP create and revise requests require explicit public authorship".into(),
            )),
            (_, Some(authorship)) => Ok(authorship),
            (Self::HumanOwner, None) => Ok(PublicAuthorship::default()),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = RuntimeConfig::load()?;
    let installation = InstallationRuntime::load(&config)?;
    let initial_theme = installation
        .as_ref()
        .map(InstallationRuntime::initial_theme_profile)
        .transpose()?
        .unwrap_or(ThemeProfile::Paper);
    let release_check_enabled = installation.as_ref().is_none_or(|installation| {
        installation.is_dlc_enabled("org.open-soverign-blog.release-check")
    });
    let version = VersionService::start_from_environment(release_check_enabled)?;
    let redis_enabled = config.redis.is_some();
    let cache_signing_key = config.cache_signing_key.unwrap_or_else(|| {
        let mut key = [0_u8; 32];
        OsRng.fill_bytes(&mut key);
        if redis_enabled {
            tracing::warn!(
                "OSB_CACHE_SIGNING_KEY is absent; cache integrity is process-local and horizontally scaled application replicas will not share hits"
            );
        }
        key
    });
    let bind = config.bind;
    let seo_policy = SeoPolicy {
        public_url: config.public_url,
        article_base_path: config.article_base_path,
        no_index: config.no_index,
    };
    seo_policy
        .validate()
        .map_err(anyhow::Error::msg)
        .context("SEO/URL policy is invalid")?;
    let cache = match config.redis.clone() {
        Some(settings) => Some(
            SemanticCache::connect(settings)
                .await
                .context("failed to initialize the selected semantic Redis cache")?,
        ),
        None => {
            tracing::info!("Redis is disabled; serving public reads from the authoritative origin");
            None
        }
    };
    let site_id = config.site_id;
    let delivery_only = config.delivery_only;
    let admin_auth_rotate = config.admin_auth_rotate;
    let operations = config.operations.clone();
    let local_auth_enabled = matches!(config.auth_mode, AuthMode::Local | AuthMode::LocalAndOauth);
    let oauth_requested = matches!(config.auth_mode, AuthMode::Oauth | AuthMode::LocalAndOauth);
    info!(
        intent = ?config.deployment_intent,
        member_auth = ?config.auth_mode,
        admin_auth = ?config.admin_auth.mode,
        redis_topology = ?config.redis.as_ref().map(|settings| settings.topology),
        database_profile = ?config.operations.database_profile,
        managed_backups = config.operations.managed_backups,
        backup_directory = %config.operations.backup_directory.display(),
        backup_interval_minutes = config.operations.backup_interval_minutes,
        backup_retention = config.operations.backup_retention,
        "resolved semantic deployment intent"
    );
    let database = config.database;
    if !delivery_only && let Some(parent) = database.parent() {
        std::fs::create_dir_all(parent).context("failed to create the data directory")?;
    }
    let repository = Arc::new(
        if delivery_only {
            SqliteRepository::open_read_only(&database)
        } else {
            SqliteRepository::open(&database)
        }
        .map_err(anyhow::Error::msg)
        .context("failed to open SQLite")?,
    );
    let admin_auth = AdminAuthRuntime::from_settings(&config.admin_auth)
        .context("administrator authentication configuration is invalid")?;
    if !delivery_only {
        repository
            .apply_durability_profile(match config.operations.database_profile {
                DatabaseProfile::Durable => SqliteDurabilityProfile::Durable,
                DatabaseProfile::Balanced => SqliteDurabilityProfile::Balanced,
                DatabaseProfile::Fast => SqliteDurabilityProfile::Fast,
            })
            .map_err(anyhow::Error::msg)
            .context("failed to apply the semantic SQLite durability profile")?;
    }
    if !delivery_only {
        let compact = site_id.simple().to_string();
        let bootstrap = PrimaryOwnerBootstrap {
            site_id,
            site_handle: format!("blog-{}", &compact[..12]),
            site_title: "My blog".into(),
            site_description: Some("This blog is owned by this OpenSoverignBlog instance.".into()),
            owner_display_name: "Owner".into(),
            theme_profile: initial_theme,
        };
        match admin_auth.mode() {
            AdminAuthMode::AccessKey | AdminAuthMode::External => {
                let stored_mode = match admin_auth.mode() {
                    AdminAuthMode::AccessKey => StoredAdminAuthMode::AccessKey,
                    AdminAuthMode::External => StoredAdminAuthMode::External,
                    AdminAuthMode::Disabled => unreachable!("matched active mode"),
                };
                let binding_fingerprint = admin_auth.binding_fingerprint();
                if admin_auth_rotate {
                    match repository.get_admin_control_plane() {
                        Ok(control) => {
                            ensure_same_admin_module_rotation(control.auth_mode, stored_mode)?;
                            let previous_epoch = control.auth_epoch;
                            let rotated = repository
                                .rotate_admin_control_plane(
                                    site_id,
                                    stored_mode,
                                    &binding_fingerprint,
                                )
                                .map_err(anyhow::Error::msg)
                                .context("failed to rotate administrator authentication")?;
                            if rotated.auth_epoch != previous_epoch {
                                tracing::warn!(
                                    previous_epoch,
                                    auth_epoch = rotated.auth_epoch,
                                    mode = rotated.auth_mode.as_str(),
                                    "rotated administrator authentication and revoked prior administrator sessions"
                                );
                            }
                        }
                        Err(RepositoryError::NotFound) => {
                            repository
                                .provision_primary_owner_site(
                                    &bootstrap,
                                    stored_mode,
                                    &binding_fingerprint,
                                )
                                .map_err(anyhow::Error::msg)
                                .context("failed to provision the primary owner")?;
                        }
                        Err(error) => return Err(anyhow::Error::msg(error.to_string())),
                    }
                } else {
                    repository
                        .provision_primary_owner_site(
                            &bootstrap,
                            stored_mode,
                            &binding_fingerprint,
                        )
                        .map_err(anyhow::Error::msg)
                        .context(
                            "failed to provision/reconcile the primary owner; use OSB_ADMIN_AUTH_ROTATE=true only for a same-module key/provider-binding change, while an auth-mode change requires a new installation contract and rebootstrap",
                        )?;
                }
            }
            AdminAuthMode::Disabled => match repository.get_admin_control_plane() {
                Ok(control) => {
                    if admin_auth_rotate {
                        ensure_same_admin_module_rotation(
                            control.auth_mode,
                            StoredAdminAuthMode::Disabled,
                        )?;
                        let rotated = repository
                            .rotate_admin_control_plane(
                                control.primary_site_id,
                                StoredAdminAuthMode::Disabled,
                                &admin_auth.binding_fingerprint(),
                            )
                            .map_err(anyhow::Error::msg)
                            .context("failed to disable administrator authentication")?;
                        if rotated.auth_epoch != control.auth_epoch {
                            tracing::warn!(
                                previous_epoch = control.auth_epoch,
                                auth_epoch = rotated.auth_epoch,
                                "disabled administrator authentication and revoked prior administrator sessions"
                            );
                        }
                    } else {
                        repository
                            .reconcile_admin_control_plane(
                                control.primary_site_id,
                                StoredAdminAuthMode::Disabled,
                                &admin_auth.binding_fingerprint(),
                            )
                            .map(|_| ())
                            .map_err(anyhow::Error::msg)
                            .context(
                                "refusing to start with disabled admin auth while the persisted control plane uses another module; changing auth mode requires a new installation contract and rebootstrap",
                            )?;
                    }
                }
                Err(RepositoryError::NotFound) => {
                    repository
                        .provision_primary_owner_site(
                            &bootstrap,
                            StoredAdminAuthMode::Disabled,
                            &admin_auth.binding_fingerprint(),
                        )
                        .map_err(anyhow::Error::msg)
                        .context(
                            "failed to provision the server-local primary site while remote administration is disabled",
                        )?;
                }
                Err(error) => return Err(anyhow::Error::msg(error.to_string())),
            },
        }
    }
    ensure_article_category_namespace(&repository, site_id, &seo_policy.article_base_path)
        .context("article/category route namespace is invalid")?;
    ensure_references_namespace(&repository, site_id, config.references.is_some())
        .context("global references route namespace is invalid")?;
    if delivery_only && !config.blob_directory.join("sha256").is_dir() {
        anyhow::bail!(
            "delivery-only blob store must already contain the sha256 namespace: {}",
            config.blob_directory.display()
        );
    }
    let blob_directory = config.blob_directory.clone();
    let assets = Arc::new(
        AssetStore::open(&blob_directory)
            .map_err(anyhow::Error::msg)
            .context("failed to open the first-party asset store")?,
    );
    let backup = if operations.managed_backups && !delivery_only {
        Some(BackupService::start(
            Arc::clone(&repository),
            blob_directory,
            operations,
        ))
    } else {
        None
    };
    let mcp_token_hash = config
        .mcp_token
        .map(|value| Sha256::digest(value.as_bytes()).into());
    let requested_features = installation
        .as_ref()
        .map(|installation| {
            installation
                .enabled_dlc_ids()
                .filter_map(feature_registry::runtime_feature_for_dlc)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| config.requested_features.clone());
    if let Some(installation) = &installation {
        tracing::info!(
            installation_id = installation.installation_id(),
            "runtime features are sourced from the verified DLC lock"
        );
    }
    config::validate_browser_secret_transport(&seo_policy.public_url, &requested_features)
        .context("resolved feature transport policy is invalid")?;
    let mut features = FeatureRegistry::from_requested(&requested_features)
        .map_err(anyhow::Error::msg)
        .context("configured features are invalid")?;
    if config.collaboration_enabled {
        features
            .activate_composed(
                "rbac",
                "persisted owner/editor memberships are enabled for collaborative Studio access",
            )
            .map_err(anyhow::Error::msg)?;
    }
    if config.comments_enabled {
        features
            .activate_composed(
                "comments",
                "authenticated comments use persistent publication scoping, bounded validation, and sanitized rendering",
            )
            .map_err(anyhow::Error::msg)?;
    }
    if config.admin_auth.mode == AdminAuthMode::External {
        features
            .activate_composed(
                "external_auth",
                "OIDC authorization code flow uses discovery, PKCE S256, state, nonce, issuer/audience verification, and an exact owner subject binding",
            )
            .map_err(anyhow::Error::msg)?;
    }
    let runner = if features.is_requested("code_runner") {
        match config.runner {
            Some(settings) => {
                let client = RemoteRunnerClient::new(settings.transport, settings.profiles)
                    .map_err(anyhow::Error::msg)
                    .context("runner client configuration is invalid")?;
                match client.readiness().await {
                    Ok(readiness) if readiness.ready => {
                        features
                            .set_runtime_status(
                                "code_runner",
                                ModuleStatus::Active,
                                true,
                                format!(
                                    "isolated broker is ready with {} approved immutable profile(s)",
                                    readiness.approved_profiles.len()
                                ),
                            )
                            .map_err(anyhow::Error::msg)?;
                    }
                    Ok(_) => {
                        features
                            .set_runtime_status(
                                "code_runner",
                                ModuleStatus::Degraded,
                                false,
                                "runner broker is reachable but no approved immutable profile is ready",
                            )
                            .map_err(anyhow::Error::msg)?;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "optional code runner is degraded");
                        features
                            .set_runtime_status(
                                "code_runner",
                                ModuleStatus::Degraded,
                                false,
                                "runner broker readiness check failed; execution remains disabled",
                            )
                            .map_err(anyhow::Error::msg)?;
                    }
                }
                Some(Arc::new(client))
            }
            None => None,
        }
    } else {
        None
    };
    let ai_summary = if features.is_active("ai_summary") {
        Some(
            AiSummaryService::new()
                .map_err(anyhow::Error::msg)
                .context("failed to initialize the optional AI summary provider client")?,
        )
    } else {
        None
    };
    let state = AppState {
        repository,
        site_id,
        seo_policy: Arc::new(seo_policy),
        #[cfg(test)]
        test_owner_bearer_hash: None,
        mcp_token_hash,
        admin_auth,
        features: Arc::new(features),
        ai_summary,
        runner,
        runner_jobs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        assets,
        cache,
        cache_signing_key: Arc::new(cache_signing_key),
        cache_fill_slots: Arc::new(tokio::sync::Semaphore::new(CACHE_FILL_LIMIT)),
        primary_category_slots: Arc::new(tokio::sync::Semaphore::new(
            PRIMARY_CATEGORY_LOOKUP_LIMIT,
        )),
        legacy_alias_slots: Arc::new(tokio::sync::Semaphore::new(LEGACY_ALIAS_LOOKUP_LIMIT)),
        backup,
        registration_open: config.registration_open,
        local_auth_enabled,
        oauth_requested,
        comments_enabled: config.comments_enabled,
        collaboration_enabled: config.collaboration_enabled,
        custom_css_enabled: config.custom_css_enabled,
        custom_css_file: Arc::new(config.custom_css_file),
        references: config.references.map(ReferencesPage::new).map(Arc::new),
        agent_discovery_enabled: config.agent_discovery_enabled,
        delivery_only,
        secure_session_cookie: config.secure_session_cookie,
        member_auth_admission: community::MemberAuthAdmission::new(),
        ai_summary_admission: community::AiSummaryAdmission::new(),
        password_workers: Arc::new(tokio::sync::Semaphore::new(PASSWORD_WORKER_LIMIT)),
        version,
    };
    let app = app(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(%bind, "OpenSoverignBlog is listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    info!("shutdown signal received; draining connections");
}

fn app(state: AppState) -> Router {
    let article_route = state
        .seo_policy
        .article_route_pattern()
        .expect("validated SEO policy");
    let spa_state = state.clone();
    let spa_fallback =
        Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap| {
            let state = spa_state.clone();
            async move { spa_index_fallback(state, method, uri, headers).await }
        });
    let mutation_routes = Router::new()
        .route("/api/v1/admin/tree", get(admin_tree::get_admin_tree))
        .route("/api/v1/admin/documents", get(list_admin_documents))
        .route("/api/v1/admin/documents/{id}", get(get_admin_document))
        .route(
            "/api/v1/admin/documents/{id}/revisions",
            get(list_admin_revisions),
        )
        .route("/api/v1/posts", post(create_post))
        .route("/api/v1/documents/{id}/revisions", post(propose_revision))
        .route("/api/v1/documents/{id}/publish", post(publish_revision))
        .route("/api/v1/ai2ai/proposals", post(ai2ai_proposal))
        .route("/api/v1/code-runner/runs", post(submit_code_run))
        .route("/api/v1/code-runner/runs/{id}", get(poll_code_run))
        .route("/api/v1/assets", post(upload_asset))
        // Reject from request parts before Axum buffers and deserializes JSON.
        .route_layer(middleware::from_fn_with_state(state.clone(), admin_guard))
        .route_layer(middleware::from_fn(private_no_store));
    let references_routes = if state.references.is_some() {
        Router::new()
            .route("/references", get(public_references))
            .route(
                "/references/",
                get(public_references_trailing_slash_redirect),
            )
    } else {
        Router::new()
    };
    Router::new()
        .route("/", get(spa_home))
        .route("/index.html", get(spa_home))
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/healthz", get(health))
        .route("/api/v1/version", get(public_version))
        .route("/UNLICENSE", get(unlicense))
        .route("/openapi/openapi.yaml", get(openapi_contract))
        .route("/.well-known/open-soverign-blog.json", get(ai2ai_discovery))
        .route("/.well-known/agent-card.json", get(a2a_unavailable))
        .route("/api/v1/capabilities", get(capabilities))
        .route("/api/v1/references", get(references::get))
        .route("/api/v1/code-runner/profiles", get(code_runner_profiles))
        .route("/api/v1/posts", get(list_posts))
        .route("/api/v1/posts/{slug}", get(get_post))
        .route("/api/v1/posts/{category}/{slug}", get(get_categorized_post))
        .route("/api/v1/posts/{slug}/source.md", get(get_markdown_source))
        .route(
            "/api/v1/posts/{category}/{slug}/source.md",
            get(get_categorized_markdown_source),
        )
        .route("/media/{digest}", get(get_asset))
        .merge(admin_auth::routes(state.clone()))
        .merge(community::routes(state.clone()))
        .merge(mutation_routes)
        .route("/@{handle}", get(public_community_blog))
        .route(
            "/@{handle}/{category}/{slug}",
            get(public_community_category_post),
        )
        .route("/@{handle}/{slug}", get(public_community_post))
        .merge(references_routes)
        .route(&article_route, get(public_post))
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap))
        .route("/custom.css", get(custom_css))
        .route("/agent.txt", get(agent_txt_redirect))
        .route("/agents.txt", get(agents_txt))
        .route("/llms.txt", get(llms_txt))
        .route("/api", any(api_not_found))
        .route("/api/{*path}", any(api_not_found))
        .nest_service("/docs", ServeDir::new("docs"))
        .nest_service("/providers", ServeDir::new("providers"))
        .nest_service("/schemas", ServeDir::new("schemas"))
        .route_service("/AI2AI.md", ServeFile::new("AI2AI.md"))
        .fallback_service(ServeDir::new(web_dist_path()).fallback(spa_fallback))
        // Keep the Redis derivative cache inside the response-hardening layers. A
        // cache hit returns before its inner service runs, so placing it outside
        // these layers would accidentally omit CSP and the other security headers.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            semantic_cache_middleware,
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(SECURITY_CSP),
        ))
        .layer(DefaultBodyLimit::max(12 * 1024 * 1024))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static(
                "camera=(), microphone=(), geolocation=(), payment=(), usb=(), browsing-topics=()",
            ),
        ))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        // Never put URI queries in spans: OIDC callbacks carry short-lived
        // authorization codes and state in the query string.
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<Body>| {
                tracing::debug_span!(
                    "http_request",
                    method = %request.method(),
                    path = %request.uri().path(),
                    version = ?request.version(),
                )
            }),
        )
        .with_state(state)
}

async fn livez() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "alive", "version": env!("CARGO_PKG_VERSION")}))
}

async fn readyz(State(state): State<AppState>) -> Response {
    let Some(cache) = &state.cache else {
        return Json(serde_json::json!({
            "status": "ready",
            "version": env!("CARGO_PKG_VERSION"),
            "cache": "disabled"
        }))
        .into_response();
    };
    match cache.ping().await {
        Ok(()) => Json(serde_json::json!({
            "status": "ready",
            "version": env!("CARGO_PKG_VERSION")
        }))
        .into_response(),
        Err(error) => {
            tracing::warn!(%error, "readiness cache check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "status": "not_ready",
                    "dependency": "redis",
                    "reason": "dependency_unavailable"
                })),
            )
                .into_response()
        }
    }
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let cache = public_cache_snapshot(state.cache.as_ref()).await;
    let backups = match &state.backup {
        Some(backup) => backup.snapshot().await,
        None => serde_json::json!({
            "state": if state.delivery_only { "not_applicable" } else { "externally_managed" }
        }),
    };
    let cache_healthy = matches!(
        cache.get("state").and_then(serde_json::Value::as_str),
        Some("active" | "disabled")
    );
    let backup_degraded =
        backups.get("state") == Some(&serde_json::Value::String("degraded".into()));
    let status = if cache_healthy && !backup_degraded {
        "ok"
    } else {
        "degraded"
    };
    Json(serde_json::json!({
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
        "dependencies": { "cache": cache, "backups": backups },
        "dataBoundary": {
            "authoritative": ["sqlite", "content_addressed_blobs"],
            "redisRole": if state.cache.is_some() {
                "discardable_public_derivative_cache"
            } else {
                "disabled_by_installation"
            }
        }
    }))
}

async fn public_cache_snapshot(cache: Option<&SemanticCache>) -> serde_json::Value {
    let Some(cache) = cache else {
        return serde_json::json!({"state": "disabled", "provider": "none", "required": false});
    };
    let snapshot = cache.snapshot().await;
    serde_json::json!({
        "provider": snapshot.provider,
        "role": snapshot.role,
        "state": snapshot.state,
        "required": snapshot.required
    })
}

async fn public_version(State(state): State<AppState>) -> Json<version::PublicVersionStatus> {
    Json(state.version.public_status().await)
}

async fn unlicense(headers: HeaderMap) -> Result<Response, ApiError> {
    public_cached_response(
        Method::GET,
        &headers,
        include_bytes!("../../../UNLICENSE").to_vec(),
        "text/plain; charset=utf-8",
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedPublicResponse {
    schema_version: String,
    headers: BTreeMap<String, String>,
    body_base64: String,
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CacheSignaturePayload<'a> {
    route_hash: &'a str,
    generation: &'a str,
    schema_version: &'a str,
    headers: &'a BTreeMap<String, String>,
    body_base64: &'a str,
}

#[derive(Debug, Clone, Copy)]
struct PrimaryCategoryCacheableResponse;

const MAX_PUBLIC_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHED_ENTRY_BYTES: usize = 86 * 1024 * 1024;

async fn semantic_cache_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let mutates_public = mutation_changes_public(&method, &path);
    let primary_category_path = primary_category_public_path(&state, &path);
    let cacheable = method == Method::GET
        && public_cache_path(&state, &path)
        && (!primary_category_path || request_accepts_html(request.headers()));
    let Some(cache) = state.cache.clone() else {
        let response = next.run(request).await;
        return if primary_category_path {
            vary_on_accept(response)
        } else {
            response
        };
    };

    if !cacheable {
        let mut response = next.run(request).await;
        if primary_category_path {
            response = vary_on_accept(response);
        }
        if mutates_public && let Err(error) = cache.complete_mutation().await {
            tracing::warn!(%error, "public cache invalidation degraded; cache reads are suspended until generation repair");
        }
        return response;
    }

    let route_hash = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "open-soverign-blog-http-cache/4 {} {} {}",
                semantic_cache_variant(&state),
                method,
                request.uri()
            )
            .as_bytes()
        )
    );
    let if_none_match = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let lookup = match cache.lookup(&route_hash).await {
        Ok(lookup) => Some(lookup),
        Err(error) => {
            tracing::warn!(%error, "Redis lookup failed; serving the authoritative origin");
            None
        }
    };
    if let Some(bytes) = lookup
        .as_ref()
        .and_then(|lookup| lookup.value.as_ref())
        .filter(|bytes| bytes.len() <= MAX_CACHED_ENTRY_BYTES)
    {
        match serde_json::from_slice::<CachedPublicResponse>(bytes) {
            Ok(cached) if cached.schema_version == "open-soverign-blog-http-cache/4" => {
                if let Some(response) = cached_response(
                    cached,
                    if_none_match.as_deref(),
                    state.cache_signing_key.as_ref(),
                    &route_hash,
                    &lookup.as_ref().expect("cache bytes require a lookup").epoch,
                ) {
                    cache.record_verified_hit();
                    return response;
                }
                tracing::warn!(route_hash, "discarding a malformed Redis cache entry");
            }
            Ok(_) | Err(_) => {
                tracing::warn!(
                    route_hash,
                    "discarding an invalid or obsolete Redis cache entry"
                );
            }
        }
    }
    if lookup.is_some() {
        cache.record_miss();
    }

    let mut response = next.run(request).await;
    if primary_category_path {
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Accept"));
        if response
            .extensions()
            .get::<PrimaryCategoryCacheableResponse>()
            .is_none()
        {
            return response;
        }
    }
    if response.status() != StatusCode::OK {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_PUBLIC_RESPONSE_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(%error, "failed to buffer a cacheable public response");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut cached_headers = BTreeMap::new();
    for name in [
        header::CONTENT_TYPE,
        header::CACHE_CONTROL,
        header::ETAG,
        header::CONTENT_LANGUAGE,
        header::VARY,
    ] {
        if let Some(value) = parts
            .headers
            .get(&name)
            .and_then(|value| value.to_str().ok())
        {
            cached_headers.insert(name.as_str().to_owned(), value.to_owned());
        }
    }
    if let Some(lookup) = lookup {
        let mut cached = CachedPublicResponse {
            schema_version: "open-soverign-blog-http-cache/4".into(),
            headers: cached_headers,
            body_base64: BASE64_STANDARD.encode(&bytes),
            signature: String::new(),
        };
        cached.signature = sign_cached_response(
            &cached,
            state.cache_signing_key.as_ref(),
            &route_hash,
            &lookup.epoch,
        );
        if let Ok(encoded) = serde_json::to_vec(&cached) {
            if let Ok(permit) = Arc::clone(&state.cache_fill_slots).try_acquire_owned() {
                let cache = cache.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = cache.store(&lookup.epoch, &route_hash, &encoded).await {
                        tracing::warn!(%error, "Redis cache fill failed; origin response was still served");
                    }
                });
            } else {
                tracing::debug!(
                    "Redis cache fill queue is saturated; serving origin without a fill"
                );
            }
        }
    }
    Response::from_parts(parts, Body::from(bytes))
}

fn begin_public_mutation(state: &AppState) -> Option<cache::CacheMutationGuard> {
    state.cache.as_ref().map(SemanticCache::begin_mutation)
}

fn cached_response(
    cached: CachedPublicResponse,
    if_none_match: Option<&str>,
    signing_key: &[u8; 32],
    route_hash: &str,
    generation: &str,
) -> Option<Response> {
    if !verify_cached_response(&cached, signing_key, route_hash, generation) {
        return None;
    }
    let not_modified = cached
        .headers
        .get(header::ETAG.as_str())
        .is_some_and(|etag| {
            if_none_match.is_some_and(|candidates| {
                candidates
                    .split(',')
                    .map(str::trim)
                    .any(|candidate| candidate == etag || candidate == "*")
            })
        });
    let mut builder = Response::builder().status(if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    });
    // Redis is a performance dependency, not a response-policy authority. Even
    // if its data is corrupted, a cache hit may restore only this narrow public
    // metadata allowlist; security and request headers are applied outside.
    for name in [
        header::CONTENT_TYPE,
        header::CACHE_CONTROL,
        header::ETAG,
        header::CONTENT_LANGUAGE,
        header::VARY,
    ] {
        if let Some(value) = cached.headers.get(name.as_str()) {
            builder = builder.header(name, HeaderValue::try_from(value).ok()?);
        }
    }
    let body = if not_modified {
        Body::empty()
    } else {
        let encoded_limit = MAX_PUBLIC_RESPONSE_BYTES.div_ceil(3) * 4;
        if cached.body_base64.len() > encoded_limit {
            return None;
        }
        let decoded = BASE64_STANDARD.decode(cached.body_base64).ok()?;
        if decoded.len() > MAX_PUBLIC_RESPONSE_BYTES {
            return None;
        }
        Body::from(decoded)
    };
    builder.body(body).ok()
}

fn sign_cached_response(
    cached: &CachedPublicResponse,
    signing_key: &[u8; 32],
    route_hash: &str,
    generation: &str,
) -> String {
    let payload = CacheSignaturePayload {
        route_hash,
        generation,
        schema_version: &cached.schema_version,
        headers: &cached.headers,
        body_base64: &cached.body_base64,
    };
    let encoded = serde_json::to_vec(&payload).expect("cache signature payload is serializable");
    BASE64_STANDARD.encode(hmac_sha256(signing_key, &encoded))
}

fn verify_cached_response(
    cached: &CachedPublicResponse,
    signing_key: &[u8; 32],
    route_hash: &str,
    generation: &str,
) -> bool {
    let Ok(provided) = BASE64_STANDARD.decode(&cached.signature) else {
        return false;
    };
    let expected = sign_cached_response(cached, signing_key, route_hash, generation);
    let Ok(expected) = BASE64_STANDARD.decode(expected) else {
        return false;
    };
    provided.len() == expected.len() && bool::from(provided.ct_eq(&expected))
}

fn hmac_sha256(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for (index, key_byte) in key.iter().enumerate() {
        inner_pad[index] ^= key_byte;
        outer_pad[index] ^= key_byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn public_cache_path(state: &AppState, path: &str) -> bool {
    path.starts_with("/@")
        || primary_category_public_path(state, path)
        || path == "/api/v1/home"
        || path == "/api/v1/references"
        || (state.references.is_some() && matches!(path, "/references" | "/references/"))
        || path == "/api/v1/feed"
        || path == "/api/v1/blogs"
        || path.starts_with("/api/v1/blogs/")
        || path == "/api/v1/primary/categories"
        || path.starts_with("/api/v1/primary/categories/")
        || path.starts_with("/api/v1/posts/")
        || path == "/api/v1/posts"
        || path == "/robots.txt"
        || path == "/sitemap.xml"
        || path == "/agent.txt"
        || path == "/agents.txt"
        || path == "/llms.txt"
        || path.starts_with(&format!(
            "/{}/",
            state.seo_policy.article_base_path.trim_matches('/')
        ))
}

fn primary_category_public_path(state: &AppState, path: &str) -> bool {
    let Some(path) = path.strip_prefix('/') else {
        return false;
    };
    let path = path.strip_suffix('/').unwrap_or(path);
    let mut segments = path.split('/');
    let Some(category) = segments.next() else {
        return false;
    };
    let post = segments.next();
    if segments.next().is_some()
        || post.is_some_and(|segment| segment.is_empty() || segment.len() > 720)
    {
        return false;
    }
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
    let article_root = state
        .seo_policy
        .article_base_path
        .trim_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    if RESERVED.contains(&category)
        || (state.references.is_some() && category == "references")
        || category == article_root
        || !(1..=40).contains(&category.len())
        || category.starts_with('-')
        || category.ends_with('-')
    {
        return false;
    }
    category
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn request_accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .map(|value| {
            value.to_str().ok().is_some_and(|value| {
                value.split(',').any(|item| {
                    let media_range = item.split(';').next().map(str::trim).unwrap_or_default();
                    media_range.eq_ignore_ascii_case("text/html") || media_range == "*/*"
                })
            })
        })
        .unwrap_or(true)
}

/// Separates cache entries produced under different operator intent. Redis is
/// deliberately reusable across releases, but a restart that changes no-index,
/// route, or feature settings must never resurrect a response from the previous
/// configuration. Owner CSS is excluded from Redis caching because its file can
/// change without a process restart.
fn semantic_cache_variant(state: &AppState) -> String {
    let intent = serde_json::json!({
        "schema": "open-soverign-blog-cache-intent/1",
        "publicUrl": state.seo_policy.public_url.as_str(),
        "articleBasePath": state.seo_policy.article_base_path,
        "noIndex": state.seo_policy.no_index,
        "features": state.features.active_ids(),
        "registrationOpen": state.registration_open,
        "localAuth": state.local_auth_enabled,
        "oauthRequested": state.oauth_requested,
        "administratorAuth": state.admin_auth.mode().as_str(),
        "comments": state.comments_enabled,
        "collaboration": state.collaboration_enabled,
        "customCss": state.custom_css_enabled,
        "references": state.references.as_ref().map(|page| page.cache_identity()),
        "agentDiscovery": state.agent_discovery_enabled,
        "deliveryOnly": state.delivery_only,
    });
    let encoded = serde_json::to_vec(&intent).expect("cache intent is serializable");
    format!("{:x}", Sha256::digest(encoded))
}

fn mutation_changes_public(method: &Method, path: &str) -> bool {
    if !matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return false;
    }
    path == "/api/v1/blogs"
        || path == "/api/v1/posts"
        || path == "/api/v1/admin/home/pins"
        || path == "/api/v1/studio/settings"
        || path == "/api/v1/studio/categories"
        || (path.starts_with("/api/v1/studio/categories/")
            && (path.ends_with("/archive") || matches!(*method, Method::PUT)))
        || path.ends_with("/publish")
        || (path.starts_with("/api/v1/posts/") && path.ends_with("/comments"))
}

async fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "not_found",
            "message": "the API route was not found"
        })),
    )
        .into_response()
}

async fn spa_home(State(state): State<AppState>, method: Method) -> Response {
    serve_spa_index(method, &state.seo_policy).await
}

async fn spa_index_fallback(
    state: AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let path = uri.path();
    let reserved = [
        "/.well-known",
        "/AI2AI.md",
        "/api",
        "/assets",
        "/custom.css",
        "/docs",
        "/media",
        "/openapi",
        "/providers",
        "/robots.txt",
        "/agent.txt",
        "/agents.txt",
        "/llms.txt",
        "/schemas",
        "/sitemap.xml",
        "/vendor",
    ]
    .iter()
    .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")));
    let references_client_route =
        state.references.is_some() && matches!(path, "/references" | "/references/");
    let known_client_route = path == "/"
        || path == "/login"
        || path == "/onboarding"
        || references_client_route
        || path == "/studio"
        || path.starts_with("/studio/")
        || path.starts_with("/@");
    let accepts_html = request_accepts_html(&headers);
    let primary_category_path = primary_category_public_path(&state, path);
    if !matches!(method, Method::GET | Method::HEAD) || reserved {
        return StatusCode::NOT_FOUND.into_response();
    }
    if primary_category_path {
        let Ok(primary_permit) = Arc::clone(&state.primary_category_slots).try_acquire_owned()
        else {
            return primary_category_lookup_saturated_response();
        };
        match primary_category_post_from_uri(&state, method.clone(), &uri, &headers, accepts_html)
            .await
        {
            Ok(Some(response)) => return mark_primary_category_cacheable(response),
            Ok(None) => {}
            Err(error) => return error.into_response(),
        }
        if accepts_html {
            match primary_category_landing_from_uri(&state, method.clone(), &uri, &headers).await {
                Ok(Some(response)) => return mark_primary_category_cacheable(response),
                Ok(None) => {}
                Err(error) => return error.into_response(),
            }
        }
        drop(primary_permit);
        match primary_legacy_alias_redirect(&state, &uri).await {
            Ok(Some(response)) => return response,
            Ok(None) => {}
            Err(error) => return error.into_response(),
        }
        if !accepts_html {
            return vary_on_accept(StatusCode::NOT_FOUND.into_response());
        }
        return vary_on_accept(serve_spa_index(method, &state.seo_policy).await);
    }
    if !known_client_route {
        match primary_legacy_alias_redirect(&state, &uri).await {
            Ok(Some(response)) => return response,
            Ok(None) => {}
            Err(error) => return error.into_response(),
        }
    }
    if !known_client_route && !accepts_html {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_spa_index(method, &state.seo_policy).await
}

fn primary_category_lookup_saturated_response() -> Response {
    let mut response = ApiError::ServiceUnavailable(
        "primary category lookup capacity is exhausted; retry shortly".into(),
    )
    .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    vary_on_accept(response)
}

fn vary_on_accept(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept"));
    response
}

fn mark_primary_category_cacheable(mut response: Response) -> Response {
    response = vary_on_accept(response);
    response
        .extensions_mut()
        .insert(PrimaryCategoryCacheableResponse);
    response
}

async fn primary_category_landing_from_uri(
    state: &AppState,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<Option<Response>, ApiError> {
    let path = uri.path().trim_end_matches('/');
    let Some(segment) = path.strip_prefix('/') else {
        return Ok(None);
    };
    if segment.is_empty() || segment.contains('/') {
        return Ok(None);
    }
    let requested_category = percent_decode_str(segment)
        .decode_utf8()
        .map(String::from)
        .map_err(|_| ApiError::BadRequest("public path is not valid UTF-8".into()))?;
    if requested_category.contains(['/', '\\']) || requested_category.chars().any(char::is_control)
    {
        return Ok(None);
    }
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let result = repository_task(move || {
        let site = repository.get_site_by_id(site_id)?;
        let owner = repository.get_user_by_id(site.owner_user_id)?;
        let category = repository.get_category_by_slug(site.id, &requested_category)?;
        let posts = repository.list_published_in_category(site.id, category.id, 500)?;
        Ok((site, owner, category, posts))
    })
    .await;
    let (site, owner, category, posts) = match result {
        Ok(value) => value,
        Err(ApiError::Repository(RepositoryError::NotFound)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let canonical = category_public_url(&state.seo_policy, None, &category.slug, None)?;
    if segment != category.slug {
        return Ok(Some(public_permanent_redirect(canonical.as_str())));
    }
    Ok(Some(
        render_category_landing_document(
            state, &site, &owner, &category, &posts, &canonical, method, headers,
        )
        .await?,
    ))
}

async fn primary_category_post_from_uri(
    state: &AppState,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    render_canonical: bool,
) -> Result<Option<Response>, ApiError> {
    let path = uri.path().trim_end_matches('/');
    let Some(path) = path.strip_prefix('/') else {
        return Ok(None);
    };
    let raw_segments = path.split('/').collect::<Vec<_>>();
    if raw_segments.len() != 2 || raw_segments.iter().any(|segment| segment.is_empty()) {
        return Ok(None);
    }
    let segments = raw_segments
        .into_iter()
        .map(|segment| percent_decode_str(segment).decode_utf8().map(String::from))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::BadRequest("public path is not valid UTF-8".into()))?;
    if segments
        .iter()
        .any(|segment| segment.contains(['/', '\\']) || segment.chars().any(char::is_control))
    {
        return Ok(None);
    }
    let requested_category = segments[0].clone();
    let requested_slug = segments[1].clone();
    let lookup_path = format!("{requested_category}/{requested_slug}");
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let result = repository_task(move || {
        let site = repository.get_site_by_id(site_id)?;
        let owner = repository.get_user_by_id(site.owner_user_id)?;
        let document = repository.get_published_by_slug(site_id, &lookup_path)?;
        let category = repository
            .get_published_category(site_id, document.id)?
            .ok_or(RepositoryError::NotFound)?;
        Ok((site, owner, document, category))
    })
    .await;
    let (site, owner, document, category) = match result {
        Ok(value) => value,
        Err(ApiError::Repository(RepositoryError::NotFound)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let view = view_from_uri(uri)?;
    let canonical = category_public_url(
        &state.seo_policy,
        None,
        &category.slug,
        Some(&document.revision.slug),
    )?;
    if requested_category != category.slug || requested_slug != document.revision.slug {
        return Ok(Some(public_permanent_redirect(
            public_projection_url(canonical, view).as_str(),
        )));
    }
    if !render_canonical {
        return Ok(None);
    }
    Ok(Some(
        render_community_post_document(
            state,
            &site,
            &owner,
            &document,
            Some(&category),
            &canonical,
            view,
            method,
            headers,
        )
        .await?,
    ))
}

/// Resolves persisted non-canonical routes before the SPA fallback. Unlike the
/// primary category renderer, this accepts deeply nested legacy paths (for
/// example `topics/algorithms/grover.html`) and always redirects to the
/// absolute current category URL.
async fn primary_legacy_alias_redirect(
    state: &AppState,
    uri: &Uri,
) -> Result<Option<Response>, ApiError> {
    let raw_path = uri.path().trim_end_matches('/');
    let Some(raw_path) = raw_path.strip_prefix('/') else {
        return Ok(None);
    };
    if raw_path.is_empty() || raw_path.len() > 8_192 {
        return Ok(None);
    }
    let raw_segments = raw_path.split('/').collect::<Vec<_>>();
    if raw_segments.len() > 32 || raw_segments.iter().any(|segment| segment.is_empty()) {
        return Ok(None);
    }
    let segments = raw_segments
        .into_iter()
        .map(|segment| percent_decode_str(segment).decode_utf8().map(String::from))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ApiError::BadRequest("public path is not valid UTF-8".into()))?;
    if segments.iter().any(|segment| {
        segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.len() > 720
            || segment.contains(['/', '\\'])
            || segment.chars().any(char::is_control)
    }) {
        return Ok(None);
    }
    let lookup_path = segments.join("/");
    if lookup_path.len() > 2_048 {
        return Ok(None);
    }
    let Ok(_permit) = Arc::clone(&state.legacy_alias_slots).try_acquire_owned() else {
        // Deep legacy aliases are a compatibility path, not a reason to queue
        // unbounded blocking SQLite work under a miss flood.
        return Ok(None);
    };
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let lookup = lookup_path.clone();
    let result = repository_task(move || {
        let document = repository.get_published_by_slug(site_id, &lookup)?;
        let category = repository
            .get_published_category(site_id, document.id)?
            .ok_or(RepositoryError::NotFound)?;
        Ok((document.revision.slug, category.slug))
    })
    .await;
    let (canonical_slug, category_slug) = match result {
        Ok(value) => value,
        Err(ApiError::Repository(RepositoryError::NotFound)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let canonical_path = format!("{category_slug}/{canonical_slug}");
    if lookup_path == canonical_path {
        return Ok(None);
    }
    let canonical = category_public_url(
        &state.seo_policy,
        None,
        &category_slug,
        Some(&canonical_slug),
    )?;
    Ok(Some(public_legacy_moved_redirect(canonical.as_str())))
}

fn view_from_uri(uri: &Uri) -> Result<ViewMode, ApiError> {
    let view = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .find_map(|(key, value)| (key == "view").then_some(value.into_owned()));
    match view.as_deref() {
        None | Some("intent") => Ok(ViewMode::Intent),
        Some("markdown") => Ok(ViewMode::Markdown),
        Some("markdown_source" | "source") => Ok(ViewMode::MarkdownSource),
        Some(_) => Err(ApiError::BadRequest("unknown view".into())),
    }
}

async fn serve_spa_index(method: Method, policy: &SeoPolicy) -> Response {
    match tokio::fs::read_to_string(web_index_path()).await {
        Ok(mut shell) => {
            if let Err(error) = inject_spa_base_path(&mut shell, policy, true) {
                tracing::error!(%error, "SPA index base path could not be configured");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            let body = if method == Method::HEAD {
                Body::empty()
            } else {
                Body::from(shell)
            };
            let mut response = Response::new(body);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=0, must-revalidate"),
            );
            response
        }
        Err(error) => {
            tracing::error!(%error, "SPA index is unavailable");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

async fn serve_spa_not_found(method: Method, policy: &SeoPolicy) -> Response {
    let mut response = serve_spa_index(method, policy).await;
    if response.status() == StatusCode::OK {
        *response.status_mut() = StatusCode::NOT_FOUND;
    }
    response
}

fn inject_spa_base_path(
    shell: &mut String,
    policy: &SeoPolicy,
    include_noindex: bool,
) -> Result<(), &'static str> {
    let path = policy.public_url.path().trim_end_matches('/');
    let application_path = if path.is_empty() { "/" } else { path };
    let document_base = if application_path == "/" {
        "/".to_owned()
    } else {
        format!("{application_path}/")
    };
    let base_marker = "<base href=\"/\" />";
    let meta_marker = "<meta name=\"osb-base-path\" content=\"/\" />";
    if !shell.contains(base_marker) || !shell.contains(meta_marker) {
        return Err("SPA index is missing its base-path markers");
    }
    shell.replace_range(
        shell.find(base_marker).expect("marker checked")
            ..shell.find(base_marker).expect("marker checked") + base_marker.len(),
        &format!("<base href=\"{}\" />", escape_attribute(&document_base)),
    );
    shell.replace_range(
        shell.find(meta_marker).expect("marker checked")
            ..shell.find(meta_marker).expect("marker checked") + meta_marker.len(),
        &format!(
            "<meta name=\"osb-base-path\" content=\"{}\" />",
            escape_attribute(application_path)
        ),
    );
    if include_noindex && policy.no_index {
        let head_end = shell
            .find("</head>")
            .ok_or("SPA index has no closing head element")?;
        shell.insert_str(
            head_end,
            "<meta name=\"robots\" content=\"noindex,nofollow\">",
        );
    }
    Ok(())
}

async fn openapi_contract(State(state): State<AppState>) -> Response {
    // OpenAPI appends Paths keys, which already start with `/`, directly to this
    // value. Keeping a trailing slash here would produce `//api/...` URLs.
    let server_url = state.seo_policy.public_url.as_str().trim_end_matches('/');
    let contract = include_str!("../../../openapi/openapi.yaml").replacen(
        "  - url: /\n",
        &format!("  - url: \"{}\"\n", server_url.replace('"', "%22")),
        1,
    );
    let mut response = contract.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/yaml; charset=utf-8"),
    );
    response
}

async fn custom_css(State(state): State<AppState>) -> Response {
    if !state.custom_css_enabled {
        let mut response = StatusCode::NO_CONTENT.into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/css; charset=utf-8"),
        );
        return response;
    }
    match tokio::fs::read(state.custom_css_file.as_ref()).await {
        Ok(bytes) if bytes.len() <= 256 * 1024 => {
            let mut response = bytes.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/css; charset=utf-8"),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=0, must-revalidate"),
            );
            response
        }
        Ok(_) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "owner CSS exceeds the 256 KiB operational limit",
        )
            .into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::SERVICE_UNAVAILABLE,
            "owner CSS is enabled but its configured file is missing",
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to read owner CSS");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn agent_txt_redirect(State(state): State<AppState>) -> Result<Response, ApiError> {
    if !state.agent_discovery_enabled {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let target = absolute_public_url(&state.seo_policy, "/agents.txt")?;
    Ok(Redirect::permanent(&target).into_response())
}

async fn agents_txt(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !state.agent_discovery_enabled {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let references = match state.references.as_ref() {
        Some(_) => format!(
            "- Global references and policies: {}\n",
            absolute_public_url(&state.seo_policy, "/references")?
        ),
        None => String::new(),
    };
    let source = format!(
        "# OpenSoverignBlog agent compatibility index\n\nThis file is a compatibility pointer, not a claim of protocol conformance.\n\n- Authoritative machine manifest: {manifest}\n- Capabilities and runtime state: {capabilities}\n- OpenAPI: {openapi}\n- Human and agent safety contract: {instructions}\n- Reader-oriented LLM index: {llms}\n{references}",
        manifest = absolute_public_url(&state.seo_policy, "/.well-known/open-soverign-blog.json")?,
        capabilities = absolute_public_url(&state.seo_policy, "/api/v1/capabilities")?,
        openapi = absolute_public_url(&state.seo_policy, "/openapi/openapi.yaml")?,
        instructions = absolute_public_url(&state.seo_policy, "/AI2AI.md")?,
        llms = absolute_public_url(&state.seo_policy, "/llms.txt")?,
        references = references,
    );
    public_cached_response(
        Method::GET,
        &headers,
        source.into_bytes(),
        "text/markdown; charset=utf-8",
    )
}

async fn llms_txt(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    if !state.agent_discovery_enabled {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let cache_note = if state.cache.is_some() {
        "Redis accelerates public derivatives; SQLite and first-party blobs remain authoritative."
    } else {
        "Redis is disabled for this installation; SQLite and first-party blobs remain authoritative."
    };
    let references = match state.references.as_ref() {
        Some(_) => format!(
            "- [Global references and policies]({})\n",
            absolute_public_url(&state.seo_policy, "/references")?
        ),
        None => String::new(),
    };
    let source = format!(
        "# OpenSoverignBlog\n\n> A self-owned on-premise Markdown publishing engine.\n\n## Public reading\n\n- [Published feed]({feed})\n- [Blogs]({blogs})\n- [Sitemap]({sitemap})\n{references}\n## Agent integration\n\n- [Machine manifest]({manifest})\n- [Capabilities]({capabilities})\n- [OpenAPI]({openapi})\n- [AI2AI safety contract]({instructions})\n\n{cache_note}\n",
        feed = absolute_public_url(&state.seo_policy, "/api/v1/feed")?,
        blogs = absolute_public_url(&state.seo_policy, "/api/v1/blogs")?,
        sitemap = absolute_public_url(&state.seo_policy, "/sitemap.xml")?,
        references = references,
        manifest = absolute_public_url(&state.seo_policy, "/.well-known/open-soverign-blog.json")?,
        capabilities = absolute_public_url(&state.seo_policy, "/api/v1/capabilities")?,
        openapi = absolute_public_url(&state.seo_policy, "/openapi/openapi.yaml")?,
        instructions = absolute_public_url(&state.seo_policy, "/AI2AI.md")?,
        cache_note = cache_note,
    );
    public_cached_response(
        Method::GET,
        &headers,
        source.into_bytes(),
        "text/markdown; charset=utf-8",
    )
}

async fn ai2ai_discovery(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cache = public_cache_snapshot(state.cache.as_ref()).await;
    let comments_href =
        absolute_public_url(&state.seo_policy, "/api/v1/posts/__post_id__/comments")?
            .replace("__post_id__", "{postId}");
    let admin_available =
        !state.delivery_only && state.admin_auth.mode() != AdminAuthMode::Disabled;
    let admin_transport = "session";
    Ok(Json(serde_json::json!({
        "specVersion": "1.0",
        "name": "OpenSoverignBlog",
        "ai2aiVersion": AI2AI_SPEC_VERSION,
        "openapi": absolute_public_url(&state.seo_policy, "/openapi/openapi.yaml")?,
        "humanInstructions": absolute_public_url(&state.seo_policy, "/AI2AI.md")?,
        "endpoints": {
            "capabilities": endpoint_descriptor(absolute_public_url(&state.seo_policy, "/api/v1/capabilities")?, &["GET"], "none", false, true),
            "feed": endpoint_descriptor(absolute_public_url(&state.seo_policy, "/api/v1/feed")?, &["GET"], "none", false, true),
            "blogs": endpoint_descriptor(absolute_public_url(&state.seo_policy, "/api/v1/blogs")?, &["GET"], "none", false, true),
            "publishedContent": endpoint_descriptor(absolute_public_url(&state.seo_policy, "/api/v1/posts")?, &["GET"], "none", false, true),
            "comments": endpoint_descriptor(comments_href.clone(), &["GET"], "none", false, state.comments_enabled),
            "commentSubmission": endpoint_descriptor(comments_href, &["POST"], "session", true, state.comments_enabled && state.local_auth_enabled && !state.delivery_only),
            "proposeRevision": endpoint_descriptor(absolute_public_url(&state.seo_policy, "/api/v1/ai2ai/proposals")?, &["POST"], admin_transport, true, admin_available && state.features.is_active("ai_authorship")),
            "uploadFirstPartyAsset": endpoint_descriptor(absolute_public_url(&state.seo_policy, "/api/v1/assets")?, &["POST"], admin_transport, true, admin_available),
            "runnerProfiles": endpoint_descriptor(absolute_public_url(&state.seo_policy, "/api/v1/code-runner/profiles")?, &["GET"], "none", false, state.features.is_active("code_runner") && state.runner.is_some())
        },
        "schemas": {
            "content": absolute_public_url(&state.seo_policy, "/schemas/content-envelope.v1.schema.json")?,
            "ai2ai": absolute_public_url(&state.seo_policy, "/schemas/ai2ai-envelope.v1.schema.json")?,
            "plugin": absolute_public_url(&state.seo_policy, "/schemas/plugin-manifest.v1.schema.json")?,
            "consentPolicy": absolute_public_url(&state.seo_policy, "/schemas/consent-policy.v1.schema.json")?,
            "adDisclosure": absolute_public_url(&state.seo_policy, "/schemas/ad-disclosure.v1.schema.json")?
        },
        "invariants": {
            "markdownRequired": true,
            "ontologyOptional": true,
            "intentHtmlUntrusted": true,
            "passiveThirdPartyNetworkBlocked": true,
            "directDatabaseWrites": false,
            "publishingRequiresSeparateCapability": true
        },
        "features": state.features.active_ids(),
        "modules": state.features.modules(),
        "operatorIntent": {
            "localAuth": state.local_auth_enabled,
            "oauthRequested": state.oauth_requested,
            "administratorAuth": state.admin_auth.mode().as_str(),
            "comments": state.comments_enabled,
            "collaboration": state.collaboration_enabled,
            "customCss": state.custom_css_enabled,
            "agentDiscovery": state.agent_discovery_enabled,
            "deliveryOnly": state.delivery_only
        },
        "dependencies": {
            "cache": cache,
            "sourceOfTruth": ["sqlite", "content_addressed_blobs"]
        },
        "externalProtocols": {
            "a2a": {
                "version": "1.0",
                "status": "adapter_not_enabled",
                "documentation": absolute_public_url(&state.seo_policy, "/docs/ai2ai/A2A-ADAPTER.md")?
            }
        }
    })))
}

fn endpoint_descriptor(
    href: String,
    methods: &'static [&'static str],
    auth: &'static str,
    mutating: bool,
    available: bool,
) -> serde_json::Value {
    serde_json::json!({
        "href": href,
        "methods": methods,
        "auth": auth,
        "mutating": mutating,
        "available": available
    })
}

fn absolute_public_url(policy: &SeoPolicy, path: &str) -> Result<String, ApiError> {
    policy
        .public_route_url(path)
        .map(|url| url.to_string())
        .map_err(|error| ApiError::Internal(format!("public route URL is invalid: {error}")))
}

async fn a2a_unavailable() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "a2a_adapter_not_enabled",
            "message": "Install and configure the optional A2A adapter before publishing an Agent Card."
        })),
    )
        .into_response()
}

async fn capabilities(State(state): State<AppState>) -> Json<Capabilities> {
    let unavailable_by_default = state
        .features
        .modules()
        .iter()
        .filter(|module| !module.operational)
        .map(|module| module.id)
        .collect();
    let mut mutation_mechanisms = Vec::new();
    if !state.delivery_only
        && (state.local_auth_enabled || state.admin_auth.mode() != AdminAuthMode::Disabled)
    {
        mutation_mechanisms.push("session");
    }
    let mut auth_methods = Vec::new();
    if !state.delivery_only {
        match state.admin_auth.mode() {
            AdminAuthMode::AccessKey => auth_methods.push(AuthMethodDescriptor {
                id: "admin-access-key".into(),
                kind: "access_key".into(),
                flow: "secret_exchange".into(),
                audience: "admin".into(),
                label: "관리자 접근 키".into(),
                action_href: "/api/v1/auth/access-key/session".into(),
                provider: None,
            }),
            AdminAuthMode::External => auth_methods.push(AuthMethodDescriptor {
                id: "admin-external".into(),
                kind: "external".into(),
                flow: "redirect".into(),
                audience: "admin".into(),
                label: state
                    .admin_auth
                    .external_label()
                    .unwrap_or("외부 계정으로 계속하기")
                    .into(),
                action_href: "/api/v1/auth/external/start".into(),
                provider: state.admin_auth.external_adapter().map(str::to_owned),
            }),
            AdminAuthMode::Disabled => {}
        }
    }
    let has_admin_session =
        !state.delivery_only && state.admin_auth.mode() != AdminAuthMode::Disabled;
    Json(Capabilities {
        version: "2.0",
        public_access: "anonymous_read",
        studio_access: if state.delivery_only || (!state.local_auth_enabled && !has_admin_session) {
            "disabled"
        } else if state.local_auth_enabled {
            "members"
        } else {
            "admin_only"
        },
        auth: AuthCapabilities {
            status: if auth_methods.is_empty() {
                "disabled"
            } else {
                "ready"
            },
            methods: auth_methods,
        },
        views: vec!["intent", "markdown", "markdown_source"],
        features: state.features.active_ids(),
        modules: state.features.modules().to_vec(),
        unavailable_by_default,
        mutation_mechanisms,
        references: state.references.as_ref().map(|page| ReferencesDescriptor {
            href: "/references",
            label: page.label().to_owned(),
        }),
        mutation_mode: if state.delivery_only {
            "read_only"
        } else if state.local_auth_enabled || has_admin_session {
            "authenticated_members"
        } else {
            "read_only"
        },
    })
}

async fn code_runner_profiles(
    State(state): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let runner = active_runner(&state)?;
    let profiles = runner
        .profiles()
        .profiles()
        .map(|profile| serde_json::to_value(profile).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ApiError::Internal)?;
    Ok(Json(profiles))
}

async fn submit_code_run(
    State(state): State<AppState>,
    Json(input): Json<CodeRunRequest>,
) -> Result<(StatusCode, Json<RunnerApiResponse>), ApiError> {
    let runner = active_runner(&state)?;
    let context = SubmissionContext::new(state.site_id, "owner")
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let result = runner
        .submit(
            &context,
            &input.profile_id,
            &input.source,
            RunLimits::default(),
        )
        .await
        .map_err(map_runner_error)?;
    let status = if matches!(result, RunSubmissionResult::Queued(_)) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(store_runner_result(&state, result).await?)))
}

async fn poll_code_run(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> Result<Json<RunnerApiResponse>, ApiError> {
    let runner = active_runner(&state)?;
    let queued = state
        .runner_jobs
        .lock()
        .await
        .get(&job_id)
        .cloned()
        .ok_or(ApiError::BadRequest("unknown or expired runner job".into()))?;
    let result = runner.poll(&queued).await.map_err(map_runner_error)?;
    Ok(Json(store_runner_result(&state, result).await?))
}

fn active_runner(state: &AppState) -> Result<&RemoteRunnerClient, ApiError> {
    if !state.features.is_active("code_runner") {
        return Err(ApiError::ServiceUnavailable(
            "code runner is not configured and ready".into(),
        ));
    }
    state
        .runner
        .as_deref()
        .ok_or_else(|| ApiError::ServiceUnavailable("code runner client is unavailable".into()))
}

async fn store_runner_result(
    state: &AppState,
    result: RunSubmissionResult,
) -> Result<RunnerApiResponse, ApiError> {
    match result {
        RunSubmissionResult::Queued(queued) => {
            let mut jobs = state.runner_jobs.lock().await;
            let now = chrono::Utc::now();
            jobs.retain(|_, candidate| candidate.expires_at() > now);
            if jobs.len() >= 1024 && !jobs.contains_key(&queued.job_id()) {
                return Err(ApiError::ServiceUnavailable(
                    "runner job tracking capacity is full".into(),
                ));
            }
            let response = RunnerApiResponse::Queued {
                job_id: queued.job_id(),
                request_id: queued.request_id(),
                poll_after_ms: queued.poll_after_ms(),
            };
            jobs.insert(queued.job_id(), queued);
            Ok(response)
        }
        RunSubmissionResult::Terminal(terminal) => {
            state.runner_jobs.lock().await.remove(&terminal.job_id);
            Ok(RunnerApiResponse::Terminal { result: terminal })
        }
    }
}

fn map_runner_error(error: RunnerError) -> ApiError {
    match error {
        RunnerError::UnapprovedProfile
        | RunnerError::InvalidRequest
        | RunnerError::LimitsExceeded
        | RunnerError::RequestExpired => ApiError::BadRequest(error.to_string()),
        RunnerError::Unavailable | RunnerError::ProfileNotReady | RunnerError::Timeout => {
            ApiError::ServiceUnavailable(error.to_string())
        }
        _ => ApiError::Upstream(error.to_string()),
    }
}

async fn list_posts(State(state): State<AppState>) -> Result<Json<Vec<PostSummary>>, ApiError> {
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let documents = repository_task(move || {
        repository
            .list_published(site_id, 100)?
            .into_iter()
            .map(|document| {
                let category = repository.get_published_category(site_id, document.id)?;
                let route_path = category
                    .map(|category| format!("{}/{}", category.slug, document.revision.slug))
                    .unwrap_or_else(|| document.revision.slug.clone());
                Ok((document, route_path))
            })
            .collect::<Result<Vec<_>, RepositoryError>>()
    })
    .await?;
    let posts = documents
        .into_iter()
        .map(|(document, route_path)| {
            Ok(PostSummary {
                id: document.id,
                title: document.revision.title,
                slug: document.revision.slug,
                api_href: machine_post_href(&state.seo_policy, &route_path, false)?,
                source_href: machine_post_href(&state.seo_policy, &route_path, true)?,
                route_path,
                updated_at: document.updated_at.to_rfc3339(),
                has_intent_view: document.revision.intent.is_some(),
                has_ontology: document.revision.ontology.is_some(),
                authorship: document.revision.authorship,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Json(posts))
}

fn machine_post_href(
    policy: &SeoPolicy,
    route_path: &str,
    markdown_source: bool,
) -> Result<String, ApiError> {
    policy
        .validate()
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    let route_segments = route_path.split('/').collect::<Vec<_>>();
    if !(1..=2).contains(&route_segments.len())
        || route_segments.iter().any(|segment| {
            segment.is_empty() || segment.contains('\\') || segment.chars().any(char::is_control)
        })
    {
        return Err(ApiError::Internal(
            "persisted post route contains an unsafe path".into(),
        ));
    }
    let mut url = policy.public_url.clone();
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| ApiError::Internal("public URL cannot own path segments".into()))?;
    segments.pop_if_empty();
    segments.extend(["api", "v1", "posts"]);
    segments.extend(route_segments);
    if markdown_source {
        segments.push("source.md");
    }
    drop(segments);
    Ok(url.to_string())
}

async fn list_admin_documents(
    State(state): State<AppState>,
) -> Result<Json<Vec<osb_kernel::DocumentSnapshot>>, ApiError> {
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    Ok(Json(
        repository_task(move || repository.list_documents(site_id, 500)).await?,
    ))
}

async fn get_admin_document(
    State(state): State<AppState>,
    Path(document_id): Path<Uuid>,
) -> Result<Json<osb_kernel::DocumentSnapshot>, ApiError> {
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let document = repository_task(move || {
        let document = repository.get_document(document_id)?;
        if document.site_id != site_id {
            return Err(RepositoryError::NotFound);
        }
        Ok(document)
    })
    .await?;
    Ok(Json(document))
}

async fn list_admin_revisions(
    State(state): State<AppState>,
    Path(document_id): Path<Uuid>,
) -> Result<Json<Vec<osb_kernel::RevisionSnapshot>>, ApiError> {
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let revisions = repository_task(move || {
        let document = repository.get_document(document_id)?;
        if document.site_id != site_id {
            return Err(RepositoryError::NotFound);
        }
        repository.list_revisions(document_id, 1_000)
    })
    .await?;
    Ok(Json(revisions))
}

async fn get_post(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<ViewQuery>,
) -> Result<Json<PostView>, ApiError> {
    let view = query.view.unwrap_or(ViewMode::Intent);
    get_post_by_route(&state, slug, view).await
}

async fn get_categorized_post(
    State(state): State<AppState>,
    Path((category, slug)): Path<(String, String)>,
    Query(query): Query<ViewQuery>,
) -> Result<Json<PostView>, ApiError> {
    let view = query.view.unwrap_or(ViewMode::Intent);
    get_post_by_route(&state, format!("{category}/{slug}"), view).await
}

async fn get_post_by_route(
    state: &AppState,
    route_path: String,
    view: ViewMode,
) -> Result<Json<PostView>, ApiError> {
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let lookup_slug = route_path.clone();
    let (document, artifact) = repository_task(move || {
        let document = repository.get_published_by_slug(site_id, &lookup_slug)?;
        let artifact = render_revision(&document.revision, view);
        Ok((document, artifact))
    })
    .await?;
    let ai_summary = document.revision.publishable_ai_summary().cloned();
    Ok(Json(PostView {
        id: document.id,
        title: document.revision.title,
        canonical_slug: document.revision.slug,
        requested_slug: route_path,
        revision_id: document.revision.id,
        markdown: document.revision.source_markdown,
        embeds: document.revision.embeds,
        artifact,
        ontology: document.revision.ontology,
        ai_summary,
        authorship: document.revision.authorship,
    }))
}

async fn get_markdown_source(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Response, ApiError> {
    get_markdown_source_by_route(&state, slug).await
}

async fn get_categorized_markdown_source(
    State(state): State<AppState>,
    Path((category, slug)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    get_markdown_source_by_route(&state, format!("{category}/{slug}")).await
}

async fn get_markdown_source_by_route(
    state: &AppState,
    route_path: String,
) -> Result<Response, ApiError> {
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let document =
        repository_task(move || repository.get_published_by_slug(site_id, &route_path)).await?;
    let mut response = document.revision.source_markdown.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/markdown; charset=utf-8"),
    );
    Ok(response)
}

async fn upload_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<AssetUploadResponse>), ApiError> {
    let filename = headers
        .get("x-osb-filename")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("asset")
        .to_owned();
    let claimed_media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let assets = Arc::clone(&state.assets);
    let record = tokio::task::spawn_blocking(move || {
        assets.put(&body, &filename, claimed_media_type.as_deref())
    })
    .await
    .map_err(|error| ApiError::Internal(format!("asset worker failed: {error}")))??;
    let url = absolute_public_url(&state.seo_policy, &format!("/media/{}", record.digest))?;
    Ok((
        StatusCode::CREATED,
        Json(AssetUploadResponse { record, url }),
    ))
}

async fn get_asset(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> Result<Response, ApiError> {
    let assets = Arc::clone(&state.assets);
    let stored = tokio::task::spawn_blocking(move || assets.get(&digest))
        .await
        .map_err(|error| ApiError::Internal(format!("asset worker failed: {error}")))??;
    let mut response = Response::new(Body::from(stored.bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&stored.record.media_type)
            .map_err(|error| ApiError::Internal(error.to_string()))?,
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"sha256:{}\"", stored.record.digest))
            .map_err(|error| ApiError::Internal(error.to_string()))?,
    );
    Ok(response)
}

async fn create_post(
    State(state): State<AppState>,
    Extension(principal): Extension<MutationPrincipal>,
    Json(input): Json<CreatePostRequest>,
) -> Result<(StatusCode, Json<osb_kernel::DocumentSnapshot>), ApiError> {
    if state.features.is_active("social_embeds") {
        social_embeds::validate_official_embeds(&input.embeds).map_err(ApiError::BadRequest)?;
    }
    let authorship = principal.resolve_authorship(input.authorship)?;
    let repository = Arc::clone(&state.repository);
    let new_document = NewDocument {
        site_id: state.site_id,
        title: input.title,
        slug: input.slug,
        source_markdown: input.source_markdown,
        embeds: input.embeds,
        intent: input.intent,
        ontology: input.ontology,
        ai_summary: input.ai_summary,
        authorship,
        actor: principal.revision_actor(),
    };
    let _cache_mutation = begin_public_mutation(&state);
    let document = repository_task(move || {
        repository.ensure_legacy_site(new_document.site_id)?;
        repository.create_document(new_document)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(document)))
}

async fn propose_revision(
    State(state): State<AppState>,
    Extension(principal): Extension<MutationPrincipal>,
    Path(document_id): Path<Uuid>,
    Json(input): Json<ProposeRevisionRequest>,
) -> Result<(StatusCode, Json<osb_kernel::RevisionSnapshot>), ApiError> {
    if state.features.is_active("social_embeds") {
        social_embeds::validate_official_embeds(&input.embeds).map_err(ApiError::BadRequest)?;
    }
    let authorship = principal.resolve_authorship(input.authorship)?;
    let input = ProposedRevision {
        document_id,
        base_revision_id: input.base_revision_id,
        title: input.title,
        slug: input.slug,
        source_markdown: input.source_markdown,
        embeds: input.embeds,
        intent: input.intent,
        ontology: input.ontology,
        ai_summary: input.ai_summary,
        authorship,
        actor: principal.revision_actor(),
        idempotency_key: input.idempotency_key,
    };
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let revision = repository_task(move || {
        if repository.get_document(document_id)?.site_id != site_id {
            return Err(RepositoryError::NotFound);
        }
        repository.append_revision(input)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(revision)))
}

async fn ai2ai_proposal(
    State(state): State<AppState>,
    Json(envelope): Json<Ai2AiEnvelope>,
) -> Result<(StatusCode, Json<osb_kernel::RevisionSnapshot>), ApiError> {
    if !state.features.is_active("ai_authorship") {
        return Err(RepositoryError::NotFound.into());
    }
    envelope
        .validate()
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    if state.features.is_active("social_embeds") {
        social_embeds::validate_official_embeds(&envelope.proposal.embeds)
            .map_err(ApiError::BadRequest)?;
    }
    let document_id = envelope.proposal.document_id;
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let revision = repository_task(move || {
        if repository.get_document(document_id)?.site_id != site_id {
            return Err(RepositoryError::NotFound);
        }
        repository.append_ai_proposal(envelope)
    })
    .await?;
    Ok((StatusCode::ACCEPTED, Json(revision)))
}

async fn publish_revision(
    State(state): State<AppState>,
    Path(document_id): Path<Uuid>,
    Json(input): Json<PublishRequest>,
) -> Result<Json<osb_kernel::DocumentSnapshot>, ApiError> {
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let _cache_mutation = begin_public_mutation(&state);
    Ok(Json(
        repository_task(move || {
            if repository.get_document(document_id)?.site_id != site_id {
                return Err(RepositoryError::NotFound);
            }
            repository.publish(document_id, input.revision_id)
        })
        .await?,
    ))
}

async fn public_community_blog(
    State(state): State<AppState>,
    Path(handle): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let repository = Arc::clone(&state.repository);
    let lookup_handle = handle.clone();
    let result = repository_task(move || {
        let site = repository.get_site_by_handle(&lookup_handle)?;
        let owner = repository.get_user_by_id(site.owner_user_id)?;
        let posts = repository
            .list_published(site.id, 500)?
            .into_iter()
            .map(|post| {
                let category = repository.get_published_category(site.id, post.id)?;
                Ok((post, category))
            })
            .collect::<Result<Vec<_>, RepositoryError>>()?;
        Ok((site, owner, posts))
    })
    .await;
    let (site, owner, posts) = match result {
        Ok(value) => value,
        Err(ApiError::Repository(RepositoryError::NotFound)) => {
            return Ok(serve_spa_not_found(method, &state.seo_policy).await);
        }
        Err(error) => return Err(error),
    };
    let canonical = community_public_url(&state.seo_policy, &site.handle, None)?;
    if handle != site.handle {
        return Ok(public_permanent_redirect(canonical.as_str()));
    }

    let description = site
        .description
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}의 공개 블로그", owner.display_name));
    let page_title = format!("{} (@{}) · OpenSoverignBlog", site.title, site.handle);
    let mut head = if state.features.is_active("seo") {
        community_meta_head(
            &page_title,
            &description,
            &canonical,
            "website",
            state.seo_policy.no_index,
            None,
        )
    } else {
        basic_page_head(&page_title)
    };
    head.push_str(&community_custom_css_head(&state, &site)?);
    let mut archive = String::new();
    for (index, (post, category)) in posts.iter().enumerate() {
        let post_url = if let Some(category) = category {
            category_public_url(
                &state.seo_policy,
                (site.id != state.site_id).then_some(site.handle.as_str()),
                &category.slug,
                Some(&post.revision.slug),
            )?
        } else {
            community_public_url(&state.seo_policy, &site.handle, Some(&post.revision.slug))?
        };
        let excerpt = summarize_markdown(&post.revision.source_markdown, 220);
        archive.push_str(&format!(
            "<article class=\"blog-list-item\"><span class=\"post-order\" aria-hidden=\"true\">{:02}</span>\
             <div><div class=\"post-card-meta\"><time datetime=\"{}\">{}</time>{}</div>\
             <h3><a href=\"{}\">{}</a></h3><p>{}</p></div>\
             <span class=\"list-arrow\" aria-hidden=\"true\">↗</span></article>",
            index + 1,
            escape_attribute(&post.revision.created_at.to_rfc3339()),
            escape_xml(&post.revision.created_at.format("%Y. %m. %d.").to_string()),
            authorship_badge(&post.revision.authorship),
            escape_attribute(post_url.as_str()),
            escape_xml(&post.revision.title),
            escape_xml(&excerpt),
        ));
    }
    if archive.is_empty() {
        archive.push_str(
            "<section class=\"empty-state\"><h2>아직 발행된 글이 없습니다.</h2></section>",
        );
    }
    let root = format!(
        "<main class=\"route-main\" id=\"main-content\"><div class=\"osb-site-frame\"><div class=\"blog-page osb-site-theme\" data-site-id=\"{}\" data-theme=\"{}\">\
         <section class=\"blog-profile\"><span class=\"blog-monogram\" aria-hidden=\"true\">{}</span><div><p class=\"blog-handle\">@{}</p>\
         <h1>{}</h1><p>{}</p><div class=\"blog-owner\"><span><strong>{}</strong>\
         <small>이 블로그의 작성자</small></span></div></div></section>\
         <section class=\"blog-posts\" aria-labelledby=\"blog-posts-title\"><div class=\"section-heading\">\
         <div><p class=\"eyebrow\">Archive</p><h2 id=\"blog-posts-title\">모든 글</h2></div>\
         <span class=\"result-count\">{}개</span></div><div class=\"blog-list\">{archive}</div>\
         </section></div></div></main>",
        site.id,
        site.theme_profile.as_str(),
        escape_xml(&display_initials(&site.title)),
        escape_xml(&site.handle),
        escape_xml(&site.title),
        escape_xml(&description),
        escape_xml(&owner.display_name),
        posts.len(),
    );
    render_spa_document(method, &headers, &state.seo_policy, &head, &root).await
}

#[allow(clippy::too_many_arguments)]
async fn render_category_landing_document(
    state: &AppState,
    site: &osb_storage_sqlite::SiteRecord,
    owner: &osb_storage_sqlite::UserRecord,
    category: &osb_storage_sqlite::CategoryRecord,
    posts: &[osb_kernel::DocumentSnapshot],
    canonical: &Url,
    method: Method,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let primary = site.id == state.site_id;
    let description = category
        .description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}의 글을 한곳에 모았습니다.", category.title));
    let page_title = format!("{} · {}", category.title, site.title);
    let mut head = if state.features.is_active("seo") {
        community_meta_head(
            &page_title,
            &description,
            canonical,
            "website",
            state.seo_policy.no_index,
            None,
        )
    } else {
        basic_page_head(&page_title)
    };
    head.push_str(&community_custom_css_head(state, site)?);

    let blog_url = if primary {
        state.seo_policy.public_url.clone()
    } else {
        community_public_url(&state.seo_policy, &site.handle, None)?
    };
    let mut archive = String::new();
    for (index, post) in posts.iter().enumerate() {
        let post_url = category_public_url(
            &state.seo_policy,
            (!primary).then_some(site.handle.as_str()),
            &category.slug,
            Some(&post.revision.slug),
        )?;
        let excerpt = summarize_markdown(&post.revision.source_markdown, 220);
        archive.push_str(&format!(
            "<article class=\"blog-list-item\"><span class=\"post-order\" aria-hidden=\"true\">{:02}</span>\
             <div><div class=\"post-card-meta\"><time datetime=\"{}\">{}</time>{}</div>\
             <h3><a href=\"{}\">{}</a></h3><p>{}</p></div>\
             <span class=\"list-arrow\" aria-hidden=\"true\">↗</span></article>",
            index + 1,
            escape_attribute(&post.revision.created_at.to_rfc3339()),
            escape_xml(&post.revision.created_at.format("%Y. %m. %d.").to_string()),
            authorship_badge(&post.revision.authorship),
            escape_attribute(post_url.as_str()),
            escape_xml(&post.revision.title),
            escape_xml(&excerpt),
        ));
    }
    if archive.is_empty() {
        archive.push_str(
            "<section class=\"empty-state\"><h2>아직 발행된 글이 없습니다.</h2></section>",
        );
    }
    let root = format!(
        "<main class=\"route-main\" id=\"main-content\"><div class=\"osb-site-frame\"><div class=\"blog-page osb-site-theme\" data-custom-css=\"{}\" data-site-id=\"{}\" data-theme=\"{}\">\
         <header class=\"blog-profile\"><span class=\"blog-monogram\" aria-hidden=\"true\">{}</span><div>\
         <p class=\"blog-handle\"><a href=\"{}\">@{}</a><span aria-hidden=\"true\"> / </span>{}</p>\
         <h1>{}</h1><p>{}</p><div class=\"blog-owner\"><span class=\"avatar\" aria-hidden=\"true\">{}</span>\
         <span><strong>{}</strong><small>{}의 카테고리</small></span></div></div></header>\
         <section class=\"blog-posts\" aria-labelledby=\"category-posts-title\"><div class=\"section-heading\">\
         <div><p class=\"eyebrow\">Category archive</p><h2 id=\"category-posts-title\">{}의 글</h2></div>\
         <span class=\"result-count\">{}개</span></div><div class=\"blog-list\">{archive}</div>\
         </section></div></div></main>",
        if state.custom_css_enabled && site.custom_css.is_some() {
            "enabled"
        } else {
            "disabled"
        },
        site.id,
        category
            .theme_profile
            .unwrap_or(site.theme_profile)
            .as_str(),
        escape_xml(&display_initials(&category.title)),
        escape_attribute(blog_url.as_str()),
        escape_xml(&site.handle),
        escape_xml(&category.slug),
        escape_xml(&category.title),
        escape_xml(&description),
        escape_xml(&display_initials(&owner.display_name)),
        escape_xml(&owner.display_name),
        escape_xml(&site.title),
        escape_xml(&category.title),
        posts.len(),
    );
    render_spa_document(method, headers, &state.seo_policy, &head, &root).await
}

async fn public_community_category_post(
    State(state): State<AppState>,
    Path((handle, category_slug, slug)): Path<(String, String, String)>,
    Query(query): Query<ViewQuery>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let repository = Arc::clone(&state.repository);
    let lookup_handle = handle.clone();
    let lookup_path = format!("{category_slug}/{slug}");
    let result = repository_task(move || {
        let site = repository.get_site_by_handle(&lookup_handle)?;
        let owner = repository.get_user_by_id(site.owner_user_id)?;
        let document = repository.get_published_by_slug(site.id, &lookup_path)?;
        let category = repository
            .get_published_category(site.id, document.id)?
            .ok_or(RepositoryError::NotFound)?;
        Ok((site, owner, document, category))
    })
    .await;
    let (site, owner, document, category) = match result {
        Ok(value) => value,
        Err(ApiError::Repository(RepositoryError::NotFound)) => {
            return Ok(serve_spa_not_found(method, &state.seo_policy).await);
        }
        Err(error) => return Err(error),
    };
    let view = query.view.unwrap_or(ViewMode::Intent);
    let canonical = category_public_url(
        &state.seo_policy,
        (site.id != state.site_id).then_some(site.handle.as_str()),
        &category.slug,
        Some(&document.revision.slug),
    )?;
    if site.id == state.site_id
        || handle != site.handle
        || category_slug != category.slug
        || slug != document.revision.slug
    {
        return Ok(public_permanent_redirect(
            public_projection_url(canonical, view).as_str(),
        ));
    }
    render_community_post_document(
        &state,
        &site,
        &owner,
        &document,
        Some(&category),
        &canonical,
        view,
        method,
        &headers,
    )
    .await
}

async fn public_community_post(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    Query(query): Query<ViewQuery>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let repository = Arc::clone(&state.repository);
    let primary_site_id = state.site_id;
    let lookup_handle = handle.clone();
    let lookup_slug = slug.clone();
    let view = query.view.unwrap_or(ViewMode::Intent);
    let result = repository_task(move || {
        let site = repository.get_site_by_handle(&lookup_handle)?;
        let owner = repository.get_user_by_id(site.owner_user_id)?;
        let document = repository.get_published_by_slug(site.id, &lookup_slug)?;
        let category = repository.get_published_category(site.id, document.id)?;
        let primary_uses_community_route =
            site.id == primary_site_id && has_provisioned_primary(&repository, primary_site_id)?;
        Ok((
            site,
            owner,
            document,
            category,
            primary_uses_community_route,
        ))
    })
    .await;
    let (site, owner, document, category, primary_uses_community_route) = match result {
        Ok(value) => value,
        Err(ApiError::Repository(RepositoryError::NotFound)) => {
            let repository = Arc::clone(&state.repository);
            let category_handle = handle.clone();
            let category_slug = slug.clone();
            let category_landing = repository_task(move || {
                let site = repository.get_site_by_handle(&category_handle)?;
                let owner = repository.get_user_by_id(site.owner_user_id)?;
                let category = repository.get_category_by_slug(site.id, &category_slug)?;
                let posts = repository.list_published_in_category(site.id, category.id, 500)?;
                Ok((site, owner, category, posts))
            })
            .await;
            match category_landing {
                Ok((site, owner, category, posts)) => {
                    // `/@handle/:segment` is intentionally shared by uncategorized
                    // posts and category landing pages. A category wins only after
                    // the published root-post lookup above fails.
                    let canonical = category_public_url(
                        &state.seo_policy,
                        (site.id != state.site_id).then_some(site.handle.as_str()),
                        &category.slug,
                        None,
                    )?;
                    if site.id == state.site_id || handle != site.handle || slug != category.slug {
                        return Ok(public_permanent_redirect(canonical.as_str()));
                    }
                    return render_category_landing_document(
                        &state, &site, &owner, &category, &posts, &canonical, method, &headers,
                    )
                    .await;
                }
                Err(ApiError::Repository(RepositoryError::NotFound)) => {}
                Err(error) => return Err(error),
            }
            let repository = Arc::clone(&state.repository);
            let leaf_handle = handle.clone();
            let leaf_slug = slug.clone();
            let leaf = repository_task(move || {
                let site = repository.get_site_by_handle(&leaf_handle)?;
                let document = repository
                    .get_unique_published_by_leaf_slug(site.id, &leaf_slug)?
                    .ok_or(RepositoryError::NotFound)?;
                let category = repository
                    .get_published_category(site.id, document.id)?
                    .ok_or(RepositoryError::NotFound)?;
                Ok((site, document, category))
            })
            .await;
            match leaf {
                Ok((site, document, category)) => {
                    let canonical = category_public_url(
                        &state.seo_policy,
                        (site.id != state.site_id).then_some(site.handle.as_str()),
                        &category.slug,
                        Some(&document.revision.slug),
                    )?;
                    return Ok(public_permanent_redirect(
                        public_projection_url(canonical, view).as_str(),
                    ));
                }
                Err(ApiError::Repository(RepositoryError::NotFound)) => {}
                Err(error) => return Err(error),
            }
            return Ok(serve_spa_not_found(method, &state.seo_policy).await);
        }
        Err(error) => return Err(error),
    };
    if let Some(category) = category {
        let canonical = category_public_url(
            &state.seo_policy,
            (site.id != state.site_id).then_some(site.handle.as_str()),
            &category.slug,
            Some(&document.revision.slug),
        )?;
        return Ok(public_permanent_redirect(
            public_projection_url(canonical, view).as_str(),
        ));
    }
    // Databases that predate the explicit installation control plane retain the
    // configured legacy article route. A provisioned primary site has a real,
    // owner-selected handle, so its community URL is the canonical identity.
    if site.id == state.site_id && !primary_uses_community_route {
        let canonical = state
            .seo_policy
            .canonical_article_url(&document.revision.slug)
            .map_err(|error| ApiError::Internal(error.to_string()))?;
        let location = public_projection_url(canonical, view);
        return Ok(public_permanent_redirect(location.as_str()));
    }
    let canonical = community_public_url(
        &state.seo_policy,
        &site.handle,
        Some(&document.revision.slug),
    )?;
    if handle != site.handle || slug != document.revision.slug {
        let location = public_projection_url(canonical, view);
        return Ok(public_permanent_redirect(location.as_str()));
    }

    render_community_post_document(
        &state, &site, &owner, &document, None, &canonical, view, method, &headers,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn render_community_post_document(
    state: &AppState,
    site: &osb_storage_sqlite::SiteRecord,
    owner: &osb_storage_sqlite::UserRecord,
    document: &osb_kernel::DocumentSnapshot,
    category: Option<&osb_storage_sqlite::CategoryRecord>,
    canonical: &Url,
    view: ViewMode,
    method: Method,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let artifact = render_revision(&document.revision, view);
    let description = summarize_markdown(&document.revision.source_markdown, 180);
    let page_title = format!("{} · {}", document.revision.title, site.title);
    let mut head = if state.features.is_active("seo") {
        community_meta_head(
            &page_title,
            &description,
            canonical,
            "article",
            state.seo_policy.no_index,
            Some(document.revision.created_at.to_rfc3339().as_str()),
        )
    } else {
        basic_page_head(&page_title)
    };
    head.push_str(&community_custom_css_head(state, site)?);
    let intent_current = if view == ViewMode::Intent {
        " aria-current=\"page\""
    } else {
        ""
    };
    let source_current = if view == ViewMode::MarkdownSource {
        " aria-current=\"page\""
    } else {
        ""
    };
    let root = format!(
        "<main class=\"route-main\" id=\"main-content\"><div class=\"osb-site-frame\"><div class=\"article-page osb-site-theme\" data-site-id=\"{}\" data-theme=\"{}\">\
         <article class=\"article-shell\"><header class=\"article-header\"><div class=\"article-kicker\">\
         <a href=\"{}\">@{}</a><span aria-hidden=\"true\">/</span>\
         <time datetime=\"{}\">{}</time>{}</div><h1>{}</h1><p class=\"article-deck\">{}</p>\
         <div class=\"article-author-row\"><div><strong>{}</strong><span>글쓴이</span></div></div>\
         <nav class=\"projection-switcher\" aria-label=\"콘텐츠 보기 방식\">\
         <a href=\"{}?view=intent\"{intent_current}>작성자 보기</a>\
         <a href=\"{}?view=markdown_source\"{source_current}>.md 원문</a></nav></header>\
         <div class=\"article-content\" data-revision=\"{}\">{}</div></article></div></div></main>",
        site.id,
        category
            .and_then(|category| category.theme_profile)
            .unwrap_or(site.theme_profile)
            .as_str(),
        escape_attribute(community_public_url(&state.seo_policy, &site.handle, None)?.as_str()),
        escape_xml(&site.handle),
        escape_attribute(&document.revision.created_at.to_rfc3339()),
        escape_xml(
            &document
                .revision
                .created_at
                .format("%Y. %m. %d.")
                .to_string()
        ),
        authorship_badge(&document.revision.authorship),
        escape_xml(&document.revision.title),
        escape_xml(&description),
        escape_xml(&owner.display_name),
        escape_attribute(canonical.as_str()),
        escape_attribute(canonical.as_str()),
        document.revision.id,
        artifact.html,
    );
    render_spa_document(method, headers, &state.seo_policy, &head, &root).await
}

fn basic_page_head(title: &str) -> String {
    format!("<title>{}</title>", escape_xml(title))
}

fn display_initials(value: &str) -> String {
    let words = value.split_whitespace().collect::<Vec<_>>();
    if words.len() > 1 {
        words
            .iter()
            .take(2)
            .filter_map(|word| word.chars().next())
            .collect::<String>()
            .to_uppercase()
    } else {
        value
            .trim()
            .chars()
            .take(2)
            .collect::<String>()
            .to_uppercase()
    }
}

fn authorship_badge(authorship: &PublicAuthorship) -> String {
    let (class_name, mut label) = match authorship.kind {
        PublicAuthorshipKind::Human => ("human", "사람이 작성".to_owned()),
        PublicAuthorshipKind::AiGenerated => ("ai_generated", "AI 생성".to_owned()),
        PublicAuthorshipKind::AiAssisted => ("ai_assisted", "AI 보조".to_owned()),
        PublicAuthorshipKind::Imported => ("imported", "가져온 글".to_owned()),
    };
    if matches!(
        authorship.kind,
        PublicAuthorshipKind::AiGenerated | PublicAuthorshipKind::AiAssisted
    ) && let Some(generator) = authorship
        .generator
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        label.push_str(" · ");
        label.push_str(generator);
    }
    if authorship.human_reviewed && authorship.kind != PublicAuthorshipKind::Human {
        label.push_str(" · 사람 검토");
    }
    format!(
        "<span class=\"authorship-badge authorship-{}\">{}</span>",
        class_name,
        escape_xml(&label)
    )
}

fn community_meta_head(
    title: &str,
    description: &str,
    canonical: &Url,
    open_graph_type: &str,
    no_index: bool,
    published_at: Option<&str>,
) -> String {
    let title_text = escape_xml(title);
    let title_attribute = escape_attribute(title);
    let description = escape_attribute(description);
    let canonical = escape_attribute(canonical.as_str());
    let robots = if no_index {
        "<meta name=\"robots\" content=\"noindex,nofollow\">"
    } else {
        ""
    };
    let published = published_at
        .map(|value| {
            format!(
                "<meta property=\"article:published_time\" content=\"{}\">",
                escape_attribute(value)
            )
        })
        .unwrap_or_default();
    format!(
        "<title>{title_text}</title><meta name=\"description\" content=\"{description}\">\
         <link rel=\"canonical\" href=\"{canonical}\"><meta property=\"og:locale\" content=\"ko_KR\">\
         <meta property=\"og:site_name\" content=\"OpenSoverignBlog\">\
         <meta property=\"og:type\" content=\"{open_graph_type}\"><meta property=\"og:title\" content=\"{title_attribute}\">\
         <meta property=\"og:description\" content=\"{description}\"><meta property=\"og:url\" content=\"{canonical}\">\
         <meta name=\"twitter:card\" content=\"summary\"><meta name=\"twitter:title\" content=\"{title_attribute}\">\
         <meta name=\"twitter:description\" content=\"{description}\">{published}{robots}"
    )
}

fn community_custom_css_head(
    state: &AppState,
    site: &osb_storage_sqlite::SiteRecord,
) -> Result<String, ApiError> {
    if !state.custom_css_enabled || site.custom_css.is_none() {
        return Ok(String::new());
    }
    let url = absolute_public_url(
        &state.seo_policy,
        &format!("/api/v1/blogs/{}/custom.css", site.handle),
    )?;
    Ok(format!(
        "<link rel=\"stylesheet\" data-osb-blog-custom-css href=\"{}\">",
        escape_attribute(&url)
    ))
}

fn community_public_url(
    policy: &SeoPolicy,
    handle: &str,
    slug: Option<&str>,
) -> Result<Url, ApiError> {
    policy
        .validate()
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    if handle.is_empty()
        || handle.contains(['/', '\\'])
        || handle.chars().any(char::is_control)
        || slug.is_some_and(|value| {
            value.is_empty() || value.contains(['/', '\\']) || value.chars().any(char::is_control)
        })
    {
        return Err(ApiError::Internal(
            "persisted community route contains an unsafe segment".into(),
        ));
    }
    let mut url = policy.public_url.clone();
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| ApiError::Internal("public URL cannot own path segments".into()))?;
    segments.pop_if_empty();
    segments.push(&format!("@{handle}"));
    if let Some(slug) = slug {
        segments.push(slug);
    }
    drop(segments);
    Ok(url)
}

fn category_public_url(
    policy: &SeoPolicy,
    handle: Option<&str>,
    category: &str,
    slug: Option<&str>,
) -> Result<Url, ApiError> {
    policy
        .validate()
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    let safe_segment = |value: &str| {
        !value.is_empty() && !value.contains(['/', '\\']) && !value.chars().any(char::is_control)
    };
    if !safe_segment(category)
        || handle.is_some_and(|value| !safe_segment(value))
        || slug.is_some_and(|value| !safe_segment(value))
    {
        return Err(ApiError::Internal(
            "persisted category route contains an unsafe segment".into(),
        ));
    }
    let mut url = policy.public_url.clone();
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| ApiError::Internal("public URL cannot own path segments".into()))?;
    segments.pop_if_empty();
    if let Some(handle) = handle {
        segments.push(&format!("@{handle}"));
    }
    segments.push(category);
    if let Some(slug) = slug {
        segments.push(slug);
    }
    drop(segments);
    Ok(url)
}

fn provisioned_primary_handle(
    repository: &SqliteRepository,
    primary_site_id: Uuid,
) -> Result<Option<String>, RepositoryError> {
    if !has_provisioned_primary(repository, primary_site_id)? {
        return Ok(None);
    }
    Ok(Some(repository.get_site_by_id(primary_site_id)?.handle))
}

fn has_provisioned_primary(
    repository: &SqliteRepository,
    primary_site_id: Uuid,
) -> Result<bool, RepositoryError> {
    match repository.get_admin_control_plane() {
        Ok(control) if control.primary_site_id == primary_site_id => Ok(true),
        Ok(_) => Err(RepositoryError::Storage(
            "administrator control plane points at a different primary site".into(),
        )),
        Err(RepositoryError::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

fn public_projection_url(mut url: Url, view: ViewMode) -> Url {
    let view = match view {
        ViewMode::Intent => return url,
        ViewMode::Markdown => "markdown",
        ViewMode::MarkdownSource => "markdown_source",
    };
    url.query_pairs_mut().append_pair("view", view);
    url
}

async fn render_spa_document(
    method: Method,
    request_headers: &HeaderMap,
    policy: &SeoPolicy,
    head: &str,
    root: &str,
) -> Result<Response, ApiError> {
    let mut shell = tokio::fs::read_to_string(web_index_path())
        .await
        .map_err(|error| ApiError::Internal(format!("SPA index is unavailable: {error}")))?;
    inject_spa_base_path(&mut shell, policy, false)
        .map_err(|error| ApiError::Internal(error.into()))?;
    if let Some(title_start) = shell.find("<title")
        && let Some(title_end) = shell[title_start..].find("</title>")
    {
        shell.replace_range(title_start..title_start + title_end + "</title>".len(), "");
    }
    let head_end = shell
        .find("</head>")
        .ok_or_else(|| ApiError::Internal("SPA index has no closing head element".into()))?;
    shell.insert_str(head_end, head);
    let root_marker = "<div id=\"root\"></div>";
    let root_start = shell
        .find(root_marker)
        .ok_or_else(|| ApiError::Internal("SPA index has no empty root element".into()))?;
    shell.replace_range(
        root_start..root_start + root_marker.len(),
        &format!("<div id=\"root\">{root}</div>"),
    );
    public_cached_response(
        method,
        request_headers,
        shell.into_bytes(),
        "text/html; charset=utf-8",
    )
}

fn web_dist_path() -> PathBuf {
    std::env::var_os("OSB_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(BUILD_WEB_DIST))
}

#[cfg(not(test))]
fn web_index_path() -> PathBuf {
    web_dist_path().join("index.html")
}

#[cfg(test)]
fn web_index_path() -> PathBuf {
    PathBuf::from(TEST_SOURCE_WEB_INDEX)
}

fn public_cached_response(
    method: Method,
    request_headers: &HeaderMap,
    bytes: Vec<u8>,
    content_type: &'static str,
) -> Result<Response, ApiError> {
    let etag = format!("\"sha256:{:x}\"", Sha256::digest(&bytes));
    let not_modified = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|values| {
            values
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == etag || candidate == "*")
        });
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(bytes)
        };
        let mut response = Response::new(body);
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        response
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(PUBLIC_HTML_CACHE),
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(|error| ApiError::Internal(error.to_string()))?,
    );
    Ok(response)
}

fn public_permanent_redirect(location: &str) -> Response {
    let mut response = Redirect::permanent(location).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(PUBLIC_HTML_CACHE),
    );
    response
}

fn public_legacy_moved_redirect(location: &str) -> Response {
    let mut response = public_permanent_redirect(location);
    *response.status_mut() = StatusCode::MOVED_PERMANENTLY;
    response
}

async fn public_post(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<ViewQuery>,
) -> Result<Response, ApiError> {
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let lookup_slug = slug.clone();
    let view = query.view.unwrap_or(ViewMode::Intent);
    let result = repository_task(move || {
        let document = repository.get_published_by_slug(site_id, &lookup_slug)?;
        let category = repository.get_published_category(site_id, document.id)?;
        let primary_handle = provisioned_primary_handle(&repository, site_id)?;
        Ok((document, category, primary_handle))
    })
    .await;
    let (document, category, primary_handle) = match result {
        Ok(value) => value,
        Err(ApiError::Repository(RepositoryError::NotFound)) => {
            let repository = Arc::clone(&state.repository);
            let leaf_slug = slug.clone();
            repository_task(move || {
                let document = repository
                    .get_unique_published_by_leaf_slug(site_id, &leaf_slug)?
                    .ok_or(RepositoryError::NotFound)?;
                let category = repository
                    .get_published_category(site_id, document.id)?
                    .ok_or(RepositoryError::NotFound)?;
                let primary_handle = provisioned_primary_handle(&repository, site_id)?;
                Ok((document, Some(category), primary_handle))
            })
            .await?
        }
        Err(error) => return Err(error),
    };
    if let Some(category) = category {
        let canonical = category_public_url(
            &state.seo_policy,
            None,
            &category.slug,
            Some(&document.revision.slug),
        )?;
        return Ok(public_permanent_redirect(
            public_projection_url(canonical, view).as_str(),
        ));
    }
    if let Some(handle) = primary_handle {
        let canonical =
            community_public_url(&state.seo_policy, &handle, Some(&document.revision.slug))?;
        let location = public_projection_url(canonical, view);
        return Ok(public_permanent_redirect(location.as_str()));
    }
    if slug != document.revision.slug {
        let canonical = state
            .seo_policy
            .canonical_article_url(&document.revision.slug)
            .map_err(|error| ApiError::Internal(error.to_string()))?;
        let location = public_projection_url(canonical, view);
        return Ok(public_permanent_redirect(location.as_str()));
    }
    let artifact = render_revision(&document.revision, view);
    let page_title = document.revision.title.clone();
    let canonical = state
        .seo_policy
        .canonical_article_url(&document.revision.slug)
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    let description = summarize_markdown(&document.revision.source_markdown, 180);
    let seo_head = if state.features.is_active("seo") {
        community_meta_head(
            &page_title,
            &description,
            &canonical,
            "article",
            state.seo_policy.no_index,
            Some(document.revision.created_at.to_rfc3339().as_str()),
        )
    } else {
        basic_page_head(&page_title)
    };
    let intent_selected = if view == ViewMode::Intent {
        " aria-current=\"page\""
    } else {
        ""
    };
    let markdown_selected = if view == ViewMode::MarkdownSource {
        " aria-current=\"page\""
    } else {
        ""
    };
    let content_css = absolute_public_url(&state.seo_policy, "/assets/osb-content.css")?;
    let custom_css = absolute_public_url(&state.seo_policy, "/custom.css")?;
    let katex_css = absolute_public_url(&state.seo_policy, "/vendor/katex/katex.min.css")?;
    let katex_js = absolute_public_url(&state.seo_policy, "/vendor/katex/katex.min.js")?;
    let content_js = absolute_public_url(&state.seo_policy, "/assets/osb-content.js")?;
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         {seo_head}\
         <link rel=\"stylesheet\" href=\"{content_css}\">\
         <link rel=\"stylesheet\" href=\"{custom_css}\">\
         <link rel=\"stylesheet\" href=\"{katex_css}\">\
         <script defer src=\"{katex_js}\"></script>\
         <script defer src=\"{content_js}\"></script></head>\
         <body>{authorship}<header class=\"osb-view-switcher\">\
         <a href=\"?view=intent\"{intent_selected}>Author intent</a>\
         <a href=\"?view=markdown_source\"{markdown_selected}>Markdown</a></header>\
         <main><article data-revision=\"{}\">{}</article></main></body></html>",
        document.revision.id,
        artifact.html,
        authorship = authorship_badge(&document.revision.authorship),
    );
    let mut response = Html(body).into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(SECURITY_CSP),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

async fn public_references(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some(page) = state.references.as_ref() else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let canonical = state
        .seo_policy
        .public_route_url("/references")
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    let description = summarize_markdown(page.source_markdown(), 180);
    let head = if state.features.is_active("seo") {
        community_meta_head(
            page.label(),
            &description,
            &canonical,
            "website",
            state.seo_policy.no_index,
            None,
        )
    } else {
        basic_page_head(page.label())
    };
    let root = format!(
        "<main class=\"route-main\" id=\"main-content\"><div class=\"osb-site-frame references-page\">\
         <article class=\"article-shell\"><header class=\"article-header references-header\">\
         <p class=\"eyebrow\">Global references</p><h1>{}</h1>\
         <p class=\"article-deck\">{}</p></header><div class=\"article-content\">{}</div>\
         <details class=\"artifact-proof\"><summary>문서 무결성 정보</summary><div>\
         <span>{}</span><code>{}</code></div></details></article></div></main>",
        escape_xml(page.label()),
        escape_xml(&description),
        page.artifact_html(),
        escape_xml(osb_renderer::RENDERER_VERSION),
        escape_xml(page.source_hash()),
    );
    render_spa_document(method, &headers, &state.seo_policy, &head, &root).await
}

async fn public_references_trailing_slash_redirect(
    State(state): State<AppState>,
) -> Result<Response, ApiError> {
    let canonical = state
        .seo_policy
        .public_route_url("/references")
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    Ok(Redirect::permanent(canonical.as_str()).into_response())
}

async fn robots(State(state): State<AppState>, method: Method, headers: HeaderMap) -> Response {
    if !state.features.is_active("seo") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let body = if state.seo_policy.no_index {
        // Crawlers must be allowed to fetch pages in order to observe their
        // noindex metadata. Indexing and crawling are different policies.
        "User-agent: *\nAllow: /\n".to_owned()
    } else {
        let sitemap = state
            .seo_policy
            .public_resource_url("sitemap.xml")
            .expect("the SEO policy was validated at startup");
        format!("User-agent: *\nAllow: /\nSitemap: {sitemap}\n")
    };
    public_cached_response(
        method,
        &headers,
        body.into_bytes(),
        "text/plain; charset=utf-8",
    )
    .unwrap_or_else(IntoResponse::into_response)
}

async fn sitemap(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !state.features.is_active("seo") || state.seo_policy.no_index {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let (primary_posts, primary_handle, community_posts) = repository_task(move || {
        let primary_posts = repository
            .list_published(site_id, SITEMAP_URL_LIMIT.min(500))?
            .into_iter()
            .map(|post| {
                let category = repository.get_published_category(site_id, post.id)?;
                Ok((post, category))
            })
            .collect::<Result<Vec<_>, RepositoryError>>()?;
        let primary_handle = provisioned_primary_handle(&repository, site_id)?;
        let mut remaining = SITEMAP_URL_LIMIT.saturating_sub(primary_posts.len());
        let mut community_posts = Vec::new();
        for site in repository.list_sites(500)? {
            if remaining == 0 {
                break;
            }
            // This site is emitted separately through either its provisioned
            // community route or the retained legacy article route.
            if site.id == site_id {
                continue;
            }
            let posts = repository.list_published(site.id, remaining.min(500))?;
            remaining = remaining.saturating_sub(posts.len());
            for post in posts {
                let category = repository.get_published_category(site.id, post.id)?;
                community_posts.push((site.handle.clone(), post, category));
            }
        }
        Ok((primary_posts, primary_handle, community_posts))
    })
    .await?;
    let mut urls = BTreeMap::new();
    for (post, category) in primary_posts {
        let url = if let Some(category) = category {
            category_public_url(
                &state.seo_policy,
                None,
                &category.slug,
                Some(&post.revision.slug),
            )?
        } else if let Some(handle) = primary_handle.as_deref() {
            community_public_url(&state.seo_policy, handle, Some(&post.revision.slug))?
        } else {
            state
                .seo_policy
                .canonical_article_url(&post.revision.slug)
                .map_err(|error| ApiError::Internal(error.to_string()))?
        };
        urls.insert(url.to_string(), post.updated_at);
    }
    for (handle, post, category) in community_posts {
        let url = if let Some(category) = category {
            category_public_url(
                &state.seo_policy,
                Some(&handle),
                &category.slug,
                Some(&post.revision.slug),
            )?
        } else {
            community_public_url(&state.seo_policy, &handle, Some(&post.revision.slug))?
        };
        urls.insert(url.to_string(), post.updated_at);
    }
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">",
    );
    if state.references.is_some() {
        xml.push_str("<url><loc>");
        xml.push_str(&escape_xml(&absolute_public_url(
            &state.seo_policy,
            "/references",
        )?));
        xml.push_str("</loc></url>");
    }
    for (url, updated_at) in urls {
        xml.push_str("<url><loc>");
        xml.push_str(&escape_xml(&url));
        xml.push_str("</loc><lastmod>");
        xml.push_str(&updated_at.to_rfc3339());
        xml.push_str("</lastmod></url>");
    }
    xml.push_str("</urlset>");
    public_cached_response(
        method,
        &headers,
        xml.into_bytes(),
        "application/xml; charset=utf-8",
    )
}

async fn repository_task<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, RepositoryError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| ApiError::Internal(format!("repository worker failed: {error}")))?
        .map_err(ApiError::from)
}

async fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
    method: &Method,
    path: &str,
) -> Result<MutationPrincipal, ApiError> {
    if state.delivery_only {
        return Err(ApiError::ReadOnly);
    }
    if state.admin_auth.mode() == AdminAuthMode::Disabled {
        #[cfg(not(test))]
        return Err(ApiError::ReadOnly);
        #[cfg(test)]
        if state.test_owner_bearer_hash.is_none() {
            return Err(ApiError::ReadOnly);
        }
    }
    if state.admin_auth.mode() != AdminAuthMode::Disabled
        && let Some(token_hash) = community::session_hash_from_headers(headers)
    {
        let repository = Arc::clone(&state.repository);
        let authenticated = tokio::task::spawn_blocking(move || {
            repository.get_primary_owner_session(&token_hash).is_ok()
        })
        .await
        .map_err(|error| ApiError::Internal(format!("session worker failed: {error}")))?;
        if authenticated {
            return Ok(MutationPrincipal::HumanOwner);
        }
    }
    if let Some(provided) = bearer_token_hash(headers) {
        #[cfg(test)]
        if state.admin_auth.mode() == AdminAuthMode::Disabled
            && let Some(expected) = state.test_owner_bearer_hash
            && bool::from(provided.ct_eq(&expected))
        {
            return Ok(MutationPrincipal::HumanOwner);
        }
        if state.admin_auth.mode() != AdminAuthMode::Disabled
            && mcp_content_route(method, path)
            && let Some(expected) = state.mcp_token_hash
            && bool::from(provided.ct_eq(&expected))
        {
            return Ok(MutationPrincipal::McpAgent);
        }
    }
    Err(ApiError::Unauthorized)
}

fn bearer_token_hash(headers: &HeaderMap) -> Option<[u8; 32]> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|provided| Sha256::digest(provided.as_bytes()).into())
}

fn mcp_content_route(method: &Method, path: &str) -> bool {
    match *method {
        Method::GET => {
            path == "/api/v1/admin/documents"
                || uuid_path(path, "/api/v1/admin/documents/", "")
                || uuid_path(path, "/api/v1/admin/documents/", "/revisions")
        }
        Method::POST => {
            path == "/api/v1/posts"
                || uuid_path(path, "/api/v1/documents/", "/revisions")
                || uuid_path(path, "/api/v1/documents/", "/publish")
        }
        _ => false,
    }
}

fn uuid_path(path: &str, prefix: &str, suffix: &str) -> bool {
    let Some(segment) = path
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(suffix))
    else {
        return false;
    };
    !segment.is_empty() && !segment.contains('/') && Uuid::parse_str(segment).is_ok()
}

async fn admin_guard(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if !matches!(
        request.method(),
        &Method::GET | &Method::HEAD | &Method::OPTIONS
    ) && !admin_auth::request_origin_is_valid(&state, request.headers())
    {
        return ApiError::Unauthorized.into_response();
    }
    match require_admin(
        &state,
        request.headers(),
        request.method(),
        request.uri().path(),
    )
    .await
    {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

async fn private_no_store(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn escape_attribute(value: &str) -> String {
    escape_xml(value).replace('\'', "&#39;")
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    version: &'static str,
    public_access: &'static str,
    studio_access: &'static str,
    auth: AuthCapabilities,
    views: Vec<&'static str>,
    features: Vec<String>,
    modules: Vec<ModuleDescriptor>,
    unavailable_by_default: Vec<&'static str>,
    mutation_mechanisms: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    references: Option<ReferencesDescriptor>,
    mutation_mode: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferencesDescriptor {
    href: &'static str,
    label: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthCapabilities {
    status: &'static str,
    methods: Vec<AuthMethodDescriptor>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthMethodDescriptor {
    id: String,
    kind: String,
    flow: String,
    audience: String,
    label: String,
    action_href: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PostSummary {
    id: Uuid,
    title: String,
    /// The leaf slug remains stable for clients that render their own routes.
    slug: String,
    /// The stored public lookup path, including an immutable category prefix.
    route_path: String,
    api_href: String,
    source_href: String,
    updated_at: String,
    has_intent_view: bool,
    has_ontology: bool,
    authorship: PublicAuthorship,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PostView {
    id: Uuid,
    title: String,
    canonical_slug: String,
    requested_slug: String,
    revision_id: Uuid,
    markdown: String,
    embeds: Vec<osb_kernel::EmbedReference>,
    artifact: PublishArtifact,
    #[serde(skip_serializing_if = "Option::is_none")]
    ontology: Option<OntologySidecar>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_summary: Option<AiSummary>,
    authorship: PublicAuthorship,
}

#[derive(Debug, Deserialize)]
struct ViewQuery {
    #[serde(default, deserialize_with = "deserialize_view")]
    view: Option<ViewMode>,
}

fn deserialize_view<'de, D>(deserializer: D) -> Result<Option<ViewMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| match value.as_str() {
            "intent" => Ok(ViewMode::Intent),
            "markdown" => Ok(ViewMode::Markdown),
            "markdown_source" | "source" => Ok(ViewMode::MarkdownSource),
            _ => Err(serde::de::Error::custom("unknown view")),
        })
        .transpose()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatePostRequest {
    title: String,
    slug: String,
    source_markdown: String,
    #[serde(default)]
    embeds: Vec<osb_kernel::EmbedReference>,
    #[serde(default)]
    intent: Option<IntentLayer>,
    #[serde(default)]
    ontology: Option<OntologySidecar>,
    #[serde(default)]
    ai_summary: Option<AiSummary>,
    #[serde(default)]
    authorship: Option<PublicAuthorship>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProposeRevisionRequest {
    base_revision_id: Uuid,
    title: String,
    slug: String,
    source_markdown: String,
    #[serde(default)]
    embeds: Vec<osb_kernel::EmbedReference>,
    #[serde(default)]
    intent: Option<IntentLayer>,
    #[serde(default)]
    ontology: Option<OntologySidecar>,
    #[serde(default)]
    ai_summary: Option<AiSummary>,
    #[serde(default)]
    authorship: Option<PublicAuthorship>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublishRequest {
    revision_id: Uuid,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AssetUploadResponse {
    record: osb_assets_fs::AssetRecord,
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodeRunRequest {
    profile_id: String,
    source: String,
}

#[derive(Debug, Serialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum RunnerApiResponse {
    Queued {
        job_id: Uuid,
        request_id: Uuid,
        poll_after_ms: u64,
    },
    Terminal {
        result: TerminalRun,
    },
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    details: BTreeMap<String, String>,
}

enum ApiError {
    Unauthorized,
    ReadOnly,
    BadRequest(String),
    Repository(RepositoryError),
    Internal(String),
    ServiceUnavailable(String),
    Upstream(String),
    Asset(AssetError),
}

impl From<RepositoryError> for ApiError {
    fn from(value: RepositoryError) -> Self {
        Self::Repository(value)
    }
}

impl From<AssetError> for ApiError {
    fn from(value: AssetError) -> Self {
        Self::Asset(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "a valid bearer credential is required".into(),
            ),
            Self::ReadOnly => (
                StatusCode::SERVICE_UNAVAILABLE,
                "read_only",
                "mutation routes are disabled until an administrator credential is configured"
                    .into(),
            ),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, "bad_request", message),
            Self::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", message),
            Self::ServiceUnavailable(message) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                message,
            ),
            Self::Upstream(message) => (StatusCode::BAD_GATEWAY, "upstream_failure", message),
            Self::Asset(error) => match error {
                AssetError::TooLarge { .. } => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "asset_too_large",
                    error.to_string(),
                ),
                AssetError::UnsafeFormat { .. }
                | AssetError::UnsupportedFormat
                | AssetError::ClaimedMediaTypeMismatch { .. } => (
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "unsupported_asset",
                    error.to_string(),
                ),
                AssetError::InvalidDigest => (
                    StatusCode::BAD_REQUEST,
                    "invalid_asset_digest",
                    error.to_string(),
                ),
                AssetError::NotFound { .. } => (
                    StatusCode::NOT_FOUND,
                    "asset_not_found",
                    "asset was not found".into(),
                ),
                AssetError::MetadataMissing { .. }
                | AssetError::IntegrityMismatch { .. }
                | AssetError::Io(_)
                | AssetError::Metadata(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "asset_storage",
                    "asset integrity or storage operation failed".into(),
                ),
            },
            Self::Repository(error) => match error {
                RepositoryError::NotFound => (
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "content was not found".into(),
                ),
                RepositoryError::DuplicateSlug => {
                    (StatusCode::CONFLICT, "duplicate_slug", error.to_string())
                }
                RepositoryError::RevisionConflict => {
                    (StatusCode::CONFLICT, "revision_conflict", error.to_string())
                }
                RepositoryError::DuplicateIdempotencyKey => (
                    StatusCode::CONFLICT,
                    "duplicate_idempotency_key",
                    error.to_string(),
                ),
                RepositoryError::Validation(_) => {
                    (StatusCode::BAD_REQUEST, "validation", error.to_string())
                }
                RepositoryError::Storage(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage",
                    "storage operation failed".into(),
                ),
            },
        };
        (
            status,
            Json(ErrorBody {
                error: code,
                message,
                details: BTreeMap::new(),
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use argon2::{
        Argon2,
        password_hash::{PasswordHasher, SaltString},
    };
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use osb_feature_code_runner_client::{
        OutputMode, ProfileRegistry, RemoteRunnerConfig, RunnerProfile,
    };
    use osb_storage_sqlite::SessionAuthMethod;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn admin_rotation_cannot_change_the_persisted_module() {
        ensure_same_admin_module_rotation(
            StoredAdminAuthMode::AccessKey,
            StoredAdminAuthMode::AccessKey,
        )
        .unwrap();

        let error = ensure_same_admin_module_rotation(
            StoredAdminAuthMode::AccessKey,
            StoredAdminAuthMode::Disabled,
        )
        .unwrap_err();
        assert!(error.to_string().contains("new installation contract"));
        assert!(error.to_string().contains("rebootstrap"));
    }

    #[test]
    fn html_acceptance_includes_http_defaults_but_not_json_only_requests() {
        let mut headers = HeaderMap::new();
        assert!(request_accepts_html(&headers));

        for value in ["text/html", "text/html; charset=utf-8", "*/*"] {
            headers.insert(header::ACCEPT, HeaderValue::from_static(value));
            assert!(request_accepts_html(&headers), "{value}");
        }

        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        assert!(!request_accepts_html(&headers));
    }

    fn test_state(token: Option<&str>) -> AppState {
        let mut features = FeatureRegistry::from_requested("seo").unwrap();
        features
            .activate_composed("rbac", "test owner memberships")
            .unwrap();
        features
            .activate_composed("comments", "test comment routes")
            .unwrap();
        AppState {
            repository: Arc::new(SqliteRepository::open_in_memory().unwrap()),
            site_id: Uuid::parse_str(DEFAULT_SITE_ID).unwrap(),
            seo_policy: Arc::new(SeoPolicy {
                public_url: Url::parse("https://blog.example/").unwrap(),
                article_base_path: "blog".into(),
                no_index: false,
            }),
            test_owner_bearer_hash: token.map(|value| Sha256::digest(value.as_bytes()).into()),
            mcp_token_hash: None,
            admin_auth: AdminAuthRuntime::Disabled,
            features: Arc::new(features),
            ai_summary: None,
            runner: None,
            runner_jobs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            assets: Arc::new(
                AssetStore::open(
                    std::env::temp_dir().join(format!("osb-test-assets-{}", Uuid::now_v7())),
                )
                .unwrap(),
            ),
            cache: None,
            cache_signing_key: Arc::new([0x5a; 32]),
            cache_fill_slots: Arc::new(tokio::sync::Semaphore::new(CACHE_FILL_LIMIT)),
            primary_category_slots: Arc::new(tokio::sync::Semaphore::new(
                PRIMARY_CATEGORY_LOOKUP_LIMIT,
            )),
            legacy_alias_slots: Arc::new(tokio::sync::Semaphore::new(LEGACY_ALIAS_LOOKUP_LIMIT)),
            backup: None,
            registration_open: true,
            local_auth_enabled: true,
            oauth_requested: false,
            comments_enabled: true,
            collaboration_enabled: false,
            custom_css_enabled: false,
            custom_css_file: Arc::new(std::env::temp_dir().join("osb-test-custom.css")),
            references: Some(Arc::new(ReferencesPage::new(config::ReferencesSettings {
                label: "레퍼런스".into(),
                source_markdown: "## 출처\n\n테스트 참고 자료".into(),
            }))),
            agent_discovery_enabled: true,
            delivery_only: false,
            secure_session_cookie: true,
            member_auth_admission: community::MemberAuthAdmission::new(),
            ai_summary_admission: community::AiSummaryAdmission::new(),
            password_workers: Arc::new(tokio::sync::Semaphore::new(PASSWORD_WORKER_LIMIT)),
            version: VersionService::bundled_for_tests(),
        }
    }

    fn access_key_state(access_key: &str) -> AppState {
        let mut state = test_state(None);
        state.local_auth_enabled = false;
        state.registration_open = false;
        let salt = SaltString::generate(&mut OsRng);
        let phc = Argon2::default()
            .hash_password(access_key.as_bytes(), &salt)
            .unwrap()
            .to_string();
        state.admin_auth = AdminAuthRuntime::from_settings(&config::AdminAuthSettings {
            mode: AdminAuthMode::AccessKey,
            access_key_phc: Some(phc),
            external: None,
            session_days: 30,
        })
        .unwrap();
        state
            .repository
            .provision_primary_owner_site(
                &PrimaryOwnerBootstrap {
                    site_id: state.site_id,
                    site_handle: "test-blog".into(),
                    site_title: "Test blog".into(),
                    site_description: None,
                    owner_display_name: "Test owner".into(),
                    theme_profile: ThemeProfile::Paper,
                },
                StoredAdminAuthMode::AccessKey,
                &state.admin_auth.binding_fingerprint(),
            )
            .unwrap();
        state
    }

    fn file_backed_access_key_state(database: &std::path::Path) -> AppState {
        let mut state =
            access_key_state("file-backed-test-administrator-access-key-with-enough-entropy");
        let repository = Arc::new(SqliteRepository::open(database).unwrap());
        repository
            .provision_primary_owner_site(
                &PrimaryOwnerBootstrap {
                    site_id: state.site_id,
                    site_handle: "test-blog".into(),
                    site_title: "Test blog".into(),
                    site_description: None,
                    owner_display_name: "Test owner".into(),
                    theme_profile: ThemeProfile::Paper,
                },
                StoredAdminAuthMode::AccessKey,
                &state.admin_auth.binding_fingerprint(),
            )
            .unwrap();
        state.repository = repository;
        state
    }

    fn publish_primary_document(state: &AppState, slug: &str, title: &str) {
        let control = state.repository.get_admin_control_plane().unwrap();
        let owner = state
            .repository
            .get_user_by_id(control.owner_user_id)
            .unwrap();
        let document = state
            .repository
            .create_document_in_writable_site_with_category(
                control.owner_user_id,
                NewDocument {
                    site_id: state.site_id,
                    title: title.into(),
                    slug: slug.into(),
                    source_markdown: format!("# {title}"),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name),
                    },
                },
                None,
            )
            .unwrap();
        state
            .repository
            .publish_document_in_owned_site(
                control.owner_user_id,
                state.site_id,
                document.id,
                document.current_revision_id,
            )
            .unwrap();
    }

    fn ai_summary_state() -> (AppState, String) {
        let mut state = test_state(None);
        let mut features = FeatureRegistry::from_requested("seo,ai_summary").unwrap();
        features
            .activate_composed("rbac", "test owner memberships")
            .unwrap();
        features
            .activate_composed("comments", "test comment routes")
            .unwrap();
        state.features = Arc::new(features);
        state.ai_summary = Some(AiSummaryService::new().unwrap());

        let owner = state
            .repository
            .create_user(
                "ai-summary-owner@example.test",
                "ai-summary-owner",
                "AI summary owner",
                "$argon2id$test-only",
            )
            .unwrap();
        state
            .repository
            .create_site(
                owner.id,
                "ai-summary-blog",
                "AI summary blog",
                None,
                ThemeProfile::Paper,
            )
            .unwrap();
        let token = [0xa7_u8; 32];
        let token_hash: [u8; 32] = Sha256::digest(token).into();
        state
            .repository
            .create_session(
                owner.id,
                &token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        (
            state,
            format!("osb_session={}", URL_SAFE_NO_PAD.encode(token)),
        )
    }

    #[tokio::test]
    async fn ai_summary_routes_fail_closed_before_provider_credentials_are_used() {
        let disabled = app(test_state(None));
        let feature_off = disabled
            .oneshot(
                Request::get("/api/v1/studio/ai-summary/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(feature_off.status(), StatusCode::NOT_FOUND);

        let (state, cookie) = ai_summary_state();
        let router = app(state);
        let oversized = "x".repeat(ai_summary::MAXIMUM_REQUEST_BYTES + 1);

        let anonymous = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/ai-summary/generate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://blog.example")
                    .body(Body::from(oversized.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let wrong_origin = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/ai-summary/generate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://attacker.example")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(oversized.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_origin.status(), StatusCode::UNAUTHORIZED);

        let bounded = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/ai-summary/generate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://blog.example")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(oversized))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bounded.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let missing_key = router
            .oneshot(
                Request::post("/api/v1/studio/ai-summary/generate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://blog.example")
                    .header(header::COOKIE, cookie)
                    .body(Body::from(
                        r#"{"provider":"openai","model":"gpt-5.4-mini","credentialMode":"one_shot","title":"Title","sourceMarkdown":"Body"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_key.status(), StatusCode::BAD_REQUEST);
        let body = json(missing_key).await;
        assert_eq!(body["error"], "bad_request");
        assert_eq!(body["message"], "a one-shot provider key is required");
    }

    fn unavailable_external_state() -> AppState {
        let mut state = test_state(None);
        state.local_auth_enabled = false;
        state.registration_open = false;
        state.admin_auth = AdminAuthRuntime::from_settings(&config::AdminAuthSettings {
            mode: AdminAuthMode::External,
            access_key_phc: None,
            external: Some(config::ExternalAdminSettings {
                adapter: "oidc".into(),
                issuer_url: Url::parse("http://127.0.0.1:9/test-issuer").unwrap(),
                client_id: "test-client".into(),
                client_secret: None,
                owner_subject: "test-owner-subject".into(),
                label: "Test identity".into(),
            }),
            session_days: 30,
        })
        .unwrap();
        state
            .repository
            .provision_primary_owner_site(
                &PrimaryOwnerBootstrap {
                    site_id: state.site_id,
                    site_handle: "external-test-blog".into(),
                    site_title: "External test blog".into(),
                    site_description: None,
                    owner_display_name: "External owner".into(),
                    theme_profile: ThemeProfile::Paper,
                },
                StoredAdminAuthMode::External,
                &state.admin_auth.binding_fingerprint(),
            )
            .unwrap();
        state
    }

    fn test_runner_client() -> Arc<RemoteRunnerClient> {
        let transport = RemoteRunnerConfig::new(
            Url::parse("http://127.0.0.1:9/").expect("test runner URL is valid"),
        )
        .expect("loopback HTTP is allowed for a local runner");
        let profile = RunnerProfile::new(
            "rust-stable",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ["rust", "rs"],
            OutputMode::Console,
            RunLimits::default(),
            64 * 1024,
        )
        .expect("test runner profile is valid");
        let profiles = ProfileRegistry::new([profile]).expect("test registry is valid");
        Arc::new(RemoteRunnerClient::new(transport, profiles).expect("test runner client is valid"))
    }

    async fn json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn seed_community_post(
        state: &AppState,
        user_handle: &str,
        site_handle: &str,
        title: &str,
        slug: &str,
    ) -> osb_kernel::DocumentSnapshot {
        let user = state
            .repository
            .create_user(
                &format!("{user_handle}@example.test"),
                user_handle,
                &format!("{user_handle} author"),
                "$argon2id$test-only",
            )
            .unwrap();
        let site = state
            .repository
            .create_site(
                user.id,
                site_handle,
                &format!("{user_handle} blog"),
                Some("Public community notes"),
                osb_storage_sqlite::ThemeProfile::Paper,
            )
            .unwrap();
        let document = state
            .repository
            .create_document_in_owned_site(
                user.id,
                NewDocument {
                    site_id: site.id,
                    title: title.into(),
                    slug: slug.into(),
                    source_markdown: format!("# {title}\n\nCrawlable body"),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: user.id.to_string(),
                        display_name: Some(user.display_name.clone()),
                    },
                },
            )
            .unwrap();
        state
            .repository
            .publish_document_in_owned_site(
                user.id,
                site.id,
                document.id,
                document.current_revision_id,
            )
            .unwrap()
    }

    #[tokio::test]
    async fn redis_free_installations_are_ready_and_report_the_origin_path() {
        let router = app(test_state(None));
        let ready = router
            .clone()
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::OK);
        assert_eq!(json(ready).await["cache"], "disabled");

        let health = router
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let health = json(health).await;
        assert_eq!(health["status"], "ok");
        assert_eq!(health["dependencies"]["cache"]["provider"], "none");
        assert_eq!(
            health["dataBoundary"]["redisRole"],
            "disabled_by_installation"
        );
    }

    #[tokio::test]
    async fn public_version_and_unlicense_are_available_without_a_release_check() {
        let router = app(test_state(None));
        let response = router
            .clone()
            .oneshot(Request::get("/api/v1/version").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = json(response).await;
        assert_eq!(status["currentVersion"], env!("CARGO_PKG_VERSION"));
        assert_eq!(status["latestVersion"], serde_json::Value::Null);
        assert_eq!(status["status"], "disabled");
        assert_eq!(
            status["repositoryUrl"],
            "https://github.com/studyreadbook4ever/OpenSoverignBlog"
        );
        assert_eq!(status["developerUrl"], "https://eff0rtchung.kr");
        assert_eq!(status["licenseHref"], "/UNLICENSE");

        let response = router
            .oneshot(Request::get("/UNLICENSE").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(body.starts_with(b"This is free and unencumbered software"));
    }

    #[tokio::test]
    async fn curated_home_is_feature_gated_admin_only_and_has_no_recent_duplicates() {
        let inactive = app(test_state(None))
            .oneshot(Request::get("/api/v1/home").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(inactive.status(), StatusCode::NOT_FOUND);

        let mut state = access_key_state("home-curation-access-key-with-enough-entropy");
        let mut features = FeatureRegistry::from_requested("seo,home_curation").unwrap();
        features
            .activate_composed("rbac", "test owner memberships")
            .unwrap();
        features
            .activate_composed("comments", "test comment routes")
            .unwrap();
        state.features = Arc::new(features);
        let control = state.repository.get_admin_control_plane().unwrap();
        let owner = state
            .repository
            .get_user_by_id(control.owner_user_id)
            .unwrap();
        let category = state
            .repository
            .create_category(
                control.owner_user_id,
                state.site_id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "yangja".into(),
                    title: "yangja".into(),
                    description: Some("양자 컴퓨팅".into()),
                    theme_profile: None,
                },
            )
            .unwrap();
        let publish_category_post = |title: &str, slug: &str| {
            let document = state
                .repository
                .create_document_in_writable_site_with_category(
                    control.owner_user_id,
                    NewDocument {
                        site_id: state.site_id,
                        title: title.into(),
                        slug: slug.into(),
                        source_markdown: format!("# {title}"),
                        embeds: vec![],
                        intent: None,
                        ontology: None,
                        authorship: Default::default(),
                        ai_summary: None,
                        actor: RevisionActor {
                            kind: RevisionActorKind::Human,
                            id: owner.id.to_string(),
                            display_name: Some(owner.display_name.clone()),
                        },
                    },
                    Some(category.id),
                )
                .unwrap();
            state
                .repository
                .publish_document_in_owned_site(
                    control.owner_user_id,
                    state.site_id,
                    document.id,
                    document.current_revision_id,
                )
                .unwrap()
        };
        let category_first = publish_category_post("Category first", "category-first");
        let category_second = publish_category_post("Category second", "category-second");
        let first = seed_community_post(&state, "curator-a", "curator-a-blog", "Pinned", "pinned");
        let second = seed_community_post(&state, "curator-b", "curator-b-blog", "Recent", "recent");

        let raw_token = [0x42_u8; 32];
        let token_hash: [u8; 32] = Sha256::digest(raw_token).into();
        state
            .repository
            .create_primary_owner_session(
                &token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
                SessionAuthMethod::AccessKey,
                &state.admin_auth.binding_fingerprint(),
            )
            .unwrap();
        let cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(raw_token));
        let router = app(state);

        let anonymous_write = router
            .clone()
            .oneshot(
                Request::put("/api/v1/admin/home/pins")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://blog.example")
                    .body(Body::from(format!(r#"{{"documentIds":["{}"]}}"#, first.id)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous_write.status(), StatusCode::UNAUTHORIZED);

        let replace = router
            .clone()
            .oneshot(
                Request::put("/api/v1/admin/home/pins")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://blog.example")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(r#"{{"documentIds":["{}"]}}"#, first.id)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replace.status(), StatusCode::OK);

        let home = router
            .oneshot(Request::get("/api/v1/home").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(home.status(), StatusCode::OK);
        assert!(home.headers().contains_key(header::ETAG));
        let payload = json(home).await;
        assert_eq!(payload["pinnedItems"][0]["id"], first.id.to_string());
        assert_eq!(payload["recentItems"][0]["id"], second.id.to_string());
        assert_eq!(payload["recentItems"].as_array().unwrap().len(), 3);
        assert!(
            payload["recentItems"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["id"] != first.id.to_string())
        );
        assert_eq!(payload["pinnedItems"][0]["authorship"]["kind"], "human");
        assert_eq!(payload["categorySections"].as_array().unwrap().len(), 1);
        assert_eq!(payload["categorySections"][0]["category"]["slug"], "yangja");
        assert_eq!(
            payload["categorySections"][0]["category"]["description"],
            "양자 컴퓨팅"
        );
        assert_eq!(
            payload["categorySections"][0]["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                category_first.id.to_string(),
                category_second.id.to_string()
            ]
        );
    }

    #[tokio::test]
    async fn serves_the_embedded_openapi_contract_and_discovers_it() {
        let router = app(test_state(None));
        let contract = router
            .clone()
            .oneshot(
                Request::get("/openapi/openapi.yaml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(contract.status(), StatusCode::OK);
        assert_eq!(
            contract.headers()[header::CONTENT_TYPE],
            "application/yaml; charset=utf-8"
        );
        let contract = to_bytes(contract.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        assert!(contract.starts_with(b"openapi: 3.1.0\n"));
        assert!(
            contract
                .windows(b"url: \"https://blog.example\"".len())
                .any(|window| window == b"url: \"https://blog.example\"")
        );
        assert!(
            !contract
                .windows(b"url: \"https://blog.example/\"".len())
                .any(|window| window == b"url: \"https://blog.example/\"")
        );

        let discovery = router
            .oneshot(
                Request::get("/.well-known/open-soverign-blog.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            json(discovery).await["openapi"],
            "https://blog.example/openapi/openapi.yaml"
        );
    }

    #[tokio::test]
    async fn capabilities_report_the_composed_community_runtime() {
        let response = app(test_state(None))
            .oneshot(
                Request::get("/api/v1/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let capabilities = json(response).await;
        assert_eq!(capabilities["version"], "2.0");
        assert_eq!(capabilities["publicAccess"], "anonymous_read");
        assert_eq!(capabilities["studioAccess"], "members");
        assert_eq!(capabilities["auth"]["status"], "disabled");
        assert_eq!(capabilities["mutationMode"], "authenticated_members");
        assert_eq!(capabilities["references"]["href"], "/references");
        assert_eq!(capabilities["references"]["label"], "레퍼런스");
        assert_eq!(
            capabilities["mutationMechanisms"],
            serde_json::json!(["session"])
        );
        assert!(
            capabilities["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "comments")
        );
        assert!(
            capabilities["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "rbac")
        );
        assert!(
            !capabilities["unavailableByDefault"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "comments" || feature == "rbac")
        );
    }

    #[tokio::test]
    async fn global_references_are_public_cacheable_and_discoverable() {
        let mut state = test_state(None);
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/notes/").unwrap(),
            article_base_path: "blog".into(),
            no_index: false,
        });
        state.references = Some(Arc::new(ReferencesPage::new(config::ReferencesSettings {
            label: "레퍼런스".into(),
            source_markdown: "## 출처\n\n[UNLICENSE](UNLICENSE)".into(),
        })));
        let router = app(state);
        let response = router
            .clone()
            .oneshot(
                Request::get("/api/v1/references")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], PUBLIC_HTML_CACHE);
        let etag = response.headers()[header::ETAG].clone();
        let page = json(response).await;
        assert_eq!(page["label"], "레퍼런스");
        assert_eq!(page["sourceMarkdown"], "## 출처\n\n[UNLICENSE](UNLICENSE)");
        assert!(
            page["artifactHtml"]
                .as_str()
                .unwrap()
                .contains("<h2>출처</h2>")
        );
        assert!(page["sourceHash"].as_str().unwrap().starts_with("sha256:"));

        let not_modified = router
            .clone()
            .oneshot(
                Request::get("/api/v1/references")
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);

        let human = router
            .clone()
            .oneshot(
                Request::get("/references")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(human.status(), StatusCode::OK);
        let human = text(human).await;
        assert!(human.contains("<h1>레퍼런스</h1>"));
        assert!(human.contains("<h2>출처</h2>"));
        assert!(human.contains("href=\"UNLICENSE\""));
        assert!(
            human.contains(
                "<link rel=\"canonical\" href=\"https://blog.example/notes/references\">"
            )
        );

        let trailing = router
            .clone()
            .oneshot(Request::get("/references/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(trailing.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            trailing.headers()[header::LOCATION],
            "https://blog.example/notes/references"
        );

        let sitemap = router
            .clone()
            .oneshot(Request::get("/sitemap.xml").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            text(sitemap)
                .await
                .contains("<loc>https://blog.example/notes/references</loc>")
        );

        let llms = router
            .clone()
            .oneshot(Request::get("/llms.txt").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            text(llms).await.contains(
                "[Global references and policies](https://blog.example/notes/references)"
            )
        );
    }

    #[tokio::test]
    async fn disabling_global_references_removes_the_api_and_capability() {
        let mut state = test_state(None);
        state.references = None;
        let router = app(state);

        let response = router
            .clone()
            .oneshot(
                Request::get("/api/v1/references")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let human = router
            .clone()
            .oneshot(
                Request::get("/references")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(human.status(), StatusCode::OK);
        assert!(!text(human).await.contains("Global references"));

        let capabilities = router
            .oneshot(
                Request::get("/api/v1/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(json(capabilities).await.get("references").is_none());
    }

    #[tokio::test]
    async fn disabled_references_preserve_an_upgraded_references_category() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("legacy.db");
        let mut state = file_backed_access_key_state(&database);
        let control = state.repository.get_admin_control_plane().unwrap();
        let category = state
            .repository
            .create_category(
                control.owner_user_id,
                state.site_id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "legacy-references".into(),
                    title: "Legacy references archive".into(),
                    description: Some("Preserved after upgrade".into()),
                    theme_profile: None,
                },
            )
            .unwrap();
        let owner = state
            .repository
            .get_user_by_id(control.owner_user_id)
            .unwrap();
        let post = state
            .repository
            .create_document_in_writable_site_with_category(
                control.owner_user_id,
                NewDocument {
                    site_id: state.site_id,
                    title: "Preserved reference note".into(),
                    slug: "legacy-policy".into(),
                    source_markdown: "# Preserved reference note".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name),
                    },
                },
                Some(category.id),
            )
            .unwrap();
        state
            .repository
            .publish_document_in_owned_site(
                control.owner_user_id,
                state.site_id,
                post.id,
                post.current_revision_id,
            )
            .unwrap();
        let legacy_connection = rusqlite::Connection::open(&database).unwrap();
        legacy_connection
            .execute_batch("DROP TRIGGER categories_slug_immutable;")
            .unwrap();
        legacy_connection
            .execute(
                "UPDATE categories SET slug = 'references' WHERE id = ?1",
                [category.id.to_string()],
            )
            .unwrap();
        legacy_connection
            .execute(
                "UPDATE routes SET path = 'references/legacy-policy'
                 WHERE site_id = ?1 AND document_id = ?2 AND is_canonical = 1",
                [state.site_id.to_string(), post.id.to_string()],
            )
            .unwrap();
        legacy_connection
            .execute_batch(
                "CREATE TRIGGER categories_slug_immutable
                 BEFORE UPDATE OF slug ON categories
                 WHEN NEW.slug != OLD.slug
                 BEGIN
                   SELECT RAISE(ABORT, 'category slugs are immutable');
                 END;",
            )
            .unwrap();

        let collision = ensure_references_namespace(&state.repository, state.site_id, true)
            .unwrap_err()
            .to_string();
        assert!(collision.contains("persisted category 'references'"));
        assert!(ensure_references_namespace(&state.repository, state.site_id, false).is_ok());
        assert!(
            ensure_article_category_namespace(&state.repository, state.site_id, "references")
                .is_err()
        );
        let create_error = state
            .repository
            .create_category(
                control.owner_user_id,
                state.site_id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "references".into(),
                    title: "Still reserved".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap_err()
            .to_string();
        assert!(create_error.contains("reserved by the application"));

        state.references = None;
        let router = app(state);
        let landing = router
            .clone()
            .oneshot(
                Request::get("/references")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(landing.status(), StatusCode::OK);
        assert!(text(landing).await.contains("Legacy references archive"));

        let detail = router
            .clone()
            .oneshot(
                Request::get("/references/legacy-policy")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(detail.status(), StatusCode::OK);
        assert!(text(detail).await.contains("Preserved reference note"));

        let category_api = router
            .clone()
            .oneshot(
                Request::get("/api/v1/primary/categories/references")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(category_api.status(), StatusCode::OK);
        assert_eq!(json(category_api).await["category"]["slug"], "references");

        let posts_api = router
            .oneshot(
                Request::get("/api/v1/primary/categories/references/posts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(posts_api.status(), StatusCode::OK);
        assert_eq!(json(posts_api).await["items"][0]["slug"], "legacy-policy");
    }

    #[test]
    fn enabled_references_allow_a_canonical_uncategorized_post_and_reject_a_legacy_root_alias() {
        let canonical_state = access_key_state(
            "references-root-route-test-administrator-access-key-with-enough-entropy",
        );
        publish_primary_document(&canonical_state, "references", "Existing policy post");

        assert!(
            !canonical_state
                .repository
                .published_noncanonical_route_exists(canonical_state.site_id, "references")
                .unwrap()
        );
        assert!(
            ensure_references_namespace(
                &canonical_state.repository,
                canonical_state.site_id,
                true,
            )
            .is_ok()
        );

        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("legacy-alias.db");
        let legacy_state = file_backed_access_key_state(&database);
        let control = legacy_state.repository.get_admin_control_plane().unwrap();
        let created_at = chrono::DateTime::parse_from_rfc3339("2020-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        legacy_state
            .repository
            .import_offline_batch(
                control.owner_user_id,
                legacy_state.site_id,
                osb_storage_sqlite::OfflineImportBatch {
                    source: "legacy-static-site".into(),
                    owner_display_name: "me".into(),
                    categories: vec![osb_storage_sqlite::OfflineImportCategory {
                        slug: "ontology".into(),
                        title: "Ontology".into(),
                        description: None,
                    }],
                    posts: vec![osb_storage_sqlite::OfflineImportPost {
                        source_id: "ontology:policy".into(),
                        title: "Legacy policy".into(),
                        slug: "policy".into(),
                        source_markdown: "# Legacy policy".into(),
                        created_at,
                        author_id: "me".into(),
                        author_display_name: "me".into(),
                        primary_category: "ontology".into(),
                        human_reviewed: true,
                        aliases: vec![osb_storage_sqlite::OfflineImportAlias {
                            path: "legacy-policy".into(),
                            created_at,
                        }],
                    }],
                },
                false,
            )
            .unwrap();
        let legacy_connection = rusqlite::Connection::open(&database).unwrap();
        legacy_connection
            .execute(
                "UPDATE routes SET path = 'references'
                 WHERE site_id = ?1 AND path = 'legacy-policy' AND is_canonical = 0",
                [legacy_state.site_id.to_string()],
            )
            .unwrap();

        assert!(
            legacy_state
                .repository
                .published_noncanonical_route_exists(legacy_state.site_id, "references")
                .unwrap()
        );
        let collision =
            ensure_references_namespace(&legacy_state.repository, legacy_state.site_id, true)
                .unwrap_err()
                .to_string();
        assert!(collision.contains("published legacy alias"));
        assert!(
            ensure_references_namespace(&legacy_state.repository, legacy_state.site_id, false)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn disabled_references_allow_references_as_the_article_base_path() {
        let mut state = access_key_state(
            "references-article-base-test-administrator-access-key-with-enough-entropy",
        );
        state.references = None;
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/base/").unwrap(),
            article_base_path: "references".into(),
            no_index: false,
        });
        publish_primary_document(&state, "policy", "Preserved policy article");

        let router = app(state);
        let response = router
            .clone()
            .oneshot(
                Request::get("/references/policy")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://blog.example/base/@test-blog/policy"
        );
        let canonical = router
            .oneshot(
                Request::get("/@test-blog/policy")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(canonical.status(), StatusCode::OK);
        assert!(text(canonical).await.contains("Preserved policy article"));
    }

    #[tokio::test]
    async fn administrator_access_key_is_exchanged_once_for_an_owner_session() {
        let access_key = "correct-administrator-access-key-with-enough-entropy";
        let router = app(access_key_state(access_key));
        let wrong = router
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/access-key/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://blog.example")
                    .body(Body::from(
                        serde_json::json!({ "accessKey": "wrong-administrator-access-key-value" })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json(wrong).await["error"], "invalid_admin_auth");

        let cross_origin = router
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/access-key/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://attacker.example")
                    .body(Body::from(
                        serde_json::json!({ "accessKey": access_key }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cross_origin.status(), StatusCode::UNAUTHORIZED);

        let login = router
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/access-key/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://blog.example")
                    .body(Body::from(
                        serde_json::json!({ "accessKey": access_key }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert!(!cookie.contains(access_key));
        let payload = json(login).await;
        assert_eq!(payload["state"], "authenticated");
        assert_eq!(payload["instanceAdministrator"], true);
        assert!(payload["blog"].is_null());
        assert!(payload["membershipRole"].is_null());

        let onboarding = router
            .clone()
            .oneshot(
                Request::post("/api/v1/blogs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://blog.example")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(
                        serde_json::json!({
                            "handle": "chosen-blog",
                            "title": "Chosen blog",
                            "description": "Owned on premise",
                            "themePreset": "forest"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(onboarding.status(), StatusCode::CREATED);
        let blog = json(onboarding).await;
        assert_eq!(blog["handle"], "chosen-blog");
        assert_eq!(blog["theme"]["presetId"], "forest");
        assert!(
            router
                .clone()
                .oneshot(
                    Request::post("/api/v1/blogs")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::ORIGIN, "https://blog.example")
                        .header(header::COOKIE, &cookie)
                        .body(Body::from(
                            serde_json::json!({
                                "handle": "second-blog",
                                "title": "Second blog",
                                "themePreset": "paper"
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
                .is_client_error()
        );

        let studio = router
            .clone()
            .oneshot(
                Request::get("/api/v1/studio/documents")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(studio.status(), StatusCode::OK);

        let legacy_admin_route = router
            .oneshot(
                Request::get("/api/v1/admin/documents")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy_admin_route.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn program_tree_is_instance_admin_only_lazy_and_metadata_only() {
        let state = access_key_state("correct-administrator-access-key-with-enough-entropy");
        let repository = Arc::clone(&state.repository);
        let document = repository
            .create_document(NewDocument {
                site_id: state.site_id,
                title: "Tree-visible title".into(),
                slug: "tree-visible-slug".into(),
                source_markdown: "PRIVATE MARKDOWN MUST NOT LEAK".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                ai_summary: None,
                authorship: Default::default(),
                actor: RevisionActor {
                    kind: RevisionActorKind::Human,
                    id: "private-owner-identity".into(),
                    display_name: Some("Private owner name".into()),
                },
            })
            .unwrap();
        let raw_token = [0x45_u8; 32];
        let token_hash: [u8; 32] = Sha256::digest(raw_token).into();
        repository
            .create_primary_owner_session(
                &token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
                SessionAuthMethod::AccessKey,
                &state.admin_auth.binding_fingerprint(),
            )
            .unwrap();
        let cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(raw_token));
        let router = app(state);

        let root = router
            .clone()
            .oneshot(
                Request::get("/api/v1/admin/tree?limit=2")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(root.status(), StatusCode::OK);
        assert_eq!(root.headers()[header::CACHE_CONTROL], "private, no-store");
        let root = json(root).await;
        assert_eq!(root["schemaVersion"], "open-soverign-blog-admin-tree/1");
        assert_eq!(root["parentId"], "root");
        assert_eq!(root["items"].as_array().unwrap().len(), 2);
        assert!(root["nextCursor"].is_string());
        let root_cursor = root["nextCursor"].as_str().unwrap();

        let root_second_page = router
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/admin/tree?limit=2&cursor={root_cursor}"))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(root_second_page.status(), StatusCode::OK);
        let root_second_page = json(root_second_page).await;
        assert_eq!(root_second_page["items"][0]["id"], "group:configuration");
        assert_eq!(root_second_page["items"][1]["id"], "group:modules");
        assert!(root_second_page["nextCursor"].is_string());

        let forged_cursor = router
            .clone()
            .oneshot(
                Request::get("/api/v1/admin/tree?cursor=not-a-valid-cursor")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forged_cursor.status(), StatusCode::BAD_REQUEST);

        let content = router
            .clone()
            .oneshot(
                Request::get("/api/v1/admin/tree?parent=group%3Acontent")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(content.status(), StatusCode::OK);
        let content = json(content).await;
        let site_node_id = content["items"][0]["id"].as_str().unwrap();
        assert!(site_node_id.starts_with("site:"));

        let categories = router
            .clone()
            .oneshot(
                Request::get(format!(
                    "/api/v1/admin/tree?parent={}",
                    site_node_id.replace(':', "%3A")
                ))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(categories.status(), StatusCode::OK);
        let categories = json(categories).await;
        assert_eq!(categories["items"][0]["kind"], "category");
        assert_eq!(categories["items"][0]["label"], "Uncategorized");
        let uncategorized_node_id = categories["items"][0]["id"].as_str().unwrap();

        let documents = router
            .clone()
            .oneshot(
                Request::get(format!(
                    "/api/v1/admin/tree?parent={}",
                    uncategorized_node_id.replace(':', "%3A")
                ))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(documents.status(), StatusCode::OK);
        let documents = json(documents).await;
        assert_eq!(documents["items"][0]["label"], "Tree-visible title");
        assert_eq!(documents["items"][0]["slug"], "tree-visible-slug");
        let document_node_id = documents["items"][0]["id"].as_str().unwrap();
        assert_eq!(document_node_id, format!("document:{}", document.id));

        let revisions = router
            .oneshot(
                Request::get(format!(
                    "/api/v1/admin/tree?parent={}",
                    document_node_id.replace(':', "%3A")
                ))
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revisions.status(), StatusCode::OK);
        let revisions = json(revisions).await;
        assert_eq!(revisions["items"][0]["kind"], "revision");
        let encoded = serde_json::to_string(&revisions).unwrap();
        for private_value in [
            "PRIVATE MARKDOWN MUST NOT LEAK",
            "private-owner-identity",
            "Private owner name",
            "sourceMarkdown",
            "contentHash",
            "customCss",
        ] {
            assert!(!encoded.contains(private_value), "leaked {private_value}");
        }
    }

    #[tokio::test]
    async fn program_tree_rejects_non_session_bearers_and_blog_members() {
        let test_bearer = app(test_state(Some("test-owner-token")))
            .oneshot(
                Request::get("/api/v1/admin/tree")
                    .header(header::AUTHORIZATION, "Bearer test-owner-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(test_bearer.status(), StatusCode::UNAUTHORIZED);

        let mut state = access_key_state("correct-administrator-access-key-with-enough-entropy");
        let mcp_token = URL_SAFE_NO_PAD.encode([0x65; 32]);
        state.mcp_token_hash = Some(Sha256::digest(mcp_token.as_bytes()).into());
        let member = state
            .repository
            .create_user(
                "tree-member@example.test",
                "tree-member",
                "Tree member",
                "$argon2id$test-only",
            )
            .unwrap();
        let raw_member_token = [0x66_u8; 32];
        let member_token_hash: [u8; 32] = Sha256::digest(raw_member_token).into();
        state
            .repository
            .create_session(
                member.id,
                &member_token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        let router = app(state);

        let mcp = router
            .clone()
            .oneshot(
                Request::get("/api/v1/admin/tree")
                    .header(header::AUTHORIZATION, format!("Bearer {mcp_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mcp.status(), StatusCode::UNAUTHORIZED);

        let member = router
            .oneshot(
                Request::get("/api/v1/admin/tree")
                    .header(
                        header::COOKIE,
                        format!("osb_session={}", URL_SAFE_NO_PAD.encode(raw_member_token)),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(member.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn selected_admin_auth_module_ignores_the_test_only_owner_bearer() {
        let mut state = access_key_state("correct-administrator-access-key-with-enough-entropy");
        state.test_owner_bearer_hash = Some(Sha256::digest(b"test-owner-token").into());
        let response = app(state)
            .oneshot(
                Request::get("/api/v1/admin/documents")
                    .header(header::AUTHORIZATION, "Bearer test-owner-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json(response).await["error"], "unauthorized");
    }

    #[tokio::test]
    async fn anonymous_authenticated_mutation_is_rejected_before_body_extraction() {
        let body_size = 12 * 1024 * 1024 + 1;
        let response = app(test_state(None))
            .oneshot(
                Request::post("/api/v1/studio/assets")
                    .header(header::CONTENT_TYPE, "image/png")
                    .header(header::CONTENT_LENGTH, body_size)
                    .body(Body::from(vec![0_u8; body_size]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json(response).await["error"], "unauthorized");
    }

    #[tokio::test]
    async fn saturated_password_workers_do_not_reveal_account_existence() {
        let state = test_state(None);
        let salt = SaltString::generate(&mut OsRng);
        let phc = Argon2::default()
            .hash_password(b"correct horse battery staple", &salt)
            .unwrap()
            .to_string();
        state
            .repository
            .create_user("known@example.test", "known", "Known", &phc)
            .unwrap();
        let _all_workers = Arc::clone(&state.password_workers)
            .acquire_many_owned(PASSWORD_WORKER_LIMIT as u32)
            .await
            .unwrap();
        let router = app(state);

        let known = router
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"email":"known@example.test","password":"wrong but sufficiently long"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let unknown = router
            .oneshot(
                Request::post("/api/v1/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"email":"unknown@example.test","password":"wrong but sufficiently long"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(known.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(unknown.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(json(known).await, json(unknown).await);
    }

    #[tokio::test]
    async fn disabling_member_auth_rejects_preexisting_legacy_sessions() {
        let mut state = test_state(None);
        let repository = Arc::clone(&state.repository);
        let user = repository
            .create_user(
                "former-member@example.test",
                "former-member",
                "Former Member",
                "$argon2id$test-only",
            )
            .unwrap();
        repository
            .create_site(
                user.id,
                "former-member-blog",
                "Former member blog",
                None,
                ThemeProfile::Paper,
            )
            .unwrap();
        let raw_token = [0x91_u8; 32];
        let token_hash: [u8; 32] = Sha256::digest(raw_token).into();
        repository
            .create_session(
                user.id,
                &token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        state.local_auth_enabled = false;
        state.registration_open = false;
        let router = app(state);
        let cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(raw_token));

        let session = router
            .clone()
            .oneshot(
                Request::get("/api/v1/session")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json(session).await["state"], "anonymous");

        let studio = router
            .oneshot(
                Request::get("/api/v1/studio/documents")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(studio.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn runtime_admin_mode_rejects_a_session_from_another_module() {
        let mut state = access_key_state("correct-administrator-access-key-with-enough-entropy");
        let raw_token = [0x92_u8; 32];
        let token_hash: [u8; 32] = Sha256::digest(raw_token).into();
        state
            .repository
            .create_primary_owner_session(
                &token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
                SessionAuthMethod::AccessKey,
                &state.admin_auth.binding_fingerprint(),
            )
            .unwrap();

        // Model an already-running replica whose runtime module has changed
        // before the persisted control plane is reconciled or rotated.
        state.admin_auth = AdminAuthRuntime::Disabled;
        let router = app(state);
        let cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(raw_token));

        let session = router
            .clone()
            .oneshot(
                Request::get("/api/v1/session")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json(session).await["state"], "anonymous");

        let studio = router
            .oneshot(
                Request::get("/api/v1/studio/documents")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(studio.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_bearer_is_limited_to_the_six_content_route_shapes() {
        let mcp_token = URL_SAFE_NO_PAD.encode([0xa5; 32]);
        let authorization = format!("Bearer {mcp_token}");
        let mut state = access_key_state("correct-administrator-access-key-with-enough-entropy");
        state.mcp_token_hash = Some(Sha256::digest(mcp_token.as_bytes()).into());
        let router = app(state);

        let list = router
            .clone()
            .oneshot(
                Request::get("/api/v1/admin/documents")
                    .header(header::AUTHORIZATION, &authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);

        let implicit_human = router
            .clone()
            .oneshot(
                Request::post("/api/v1/posts")
                    .header(header::AUTHORIZATION, &authorization)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r##"{"title":"Missing provenance","slug":"missing-provenance","sourceMarkdown":"# Missing"}"##,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(implicit_human.status(), StatusCode::BAD_REQUEST);

        let created = router
            .clone()
            .oneshot(
                Request::post("/api/v1/posts")
                    .header(header::AUTHORIZATION, &authorization)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r##"{"title":"MCP draft","slug":"mcp-draft","sourceMarkdown":"# MCP","authorship":{"kind":"ai_generated","generator":"local/model-v1","humanReviewed":false}}"##,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = json(created).await;
        assert_eq!(created["revision"]["actor"]["kind"], "agent");
        assert_eq!(created["revision"]["actor"]["id"], "osb-mcp");
        assert_eq!(created["revision"]["authorship"]["kind"], "ai_generated");
        let document_id = created["id"].as_str().unwrap();
        let base_revision_id = created["currentRevisionId"].as_str().unwrap();

        for path in [
            format!("/api/v1/admin/documents/{document_id}"),
            format!("/api/v1/admin/documents/{document_id}/revisions"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::AUTHORIZATION, &authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let revised = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/documents/{document_id}/revisions"))
                    .header(header::AUTHORIZATION, &authorization)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "baseRevisionId": base_revision_id,
                            "title": "MCP revision",
                            "slug": "mcp-draft",
                            "sourceMarkdown": "# Revised by MCP",
                            "authorship": {
                                "kind": "ai_assisted",
                                "generator": "local/model-v1",
                                "humanReviewed": true
                            },
                            "idempotencyKey": "mcp-test-revision"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revised.status(), StatusCode::CREATED);
        let revised = json(revised).await;
        assert_eq!(revised["actor"]["kind"], "agent");
        assert_eq!(revised["actor"]["id"], "osb-mcp");
        assert_eq!(revised["authorship"]["kind"], "ai_assisted");
        let revision_id = revised["id"].as_str().unwrap().to_owned();

        let published = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/documents/{document_id}/publish"))
                    .header(header::AUTHORIZATION, &authorization)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "revisionId": revision_id }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(published.status(), StatusCode::OK);

        for (method, path) in [
            (Method::POST, "/api/v1/ai2ai/proposals"),
            (Method::POST, "/api/v1/assets"),
            (Method::POST, "/api/v1/code-runner/runs"),
            (
                Method::GET,
                "/api/v1/code-runner/runs/00000000-0000-7000-8000-000000000001",
            ),
            (Method::GET, "/api/v1/studio/settings"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header(header::AUTHORIZATION, &authorization)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
    }

    #[tokio::test]
    async fn access_key_capability_advertises_session_exchange_not_owner_bearer() {
        let response = app(access_key_state(
            "another-correct-administrator-access-key-with-entropy",
        ))
        .oneshot(
            Request::get("/api/v1/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        let body = json(response).await;
        assert_eq!(body["studioAccess"], "admin_only");
        assert_eq!(body["auth"]["status"], "ready");
        assert_eq!(body["auth"]["methods"][0]["kind"], "access_key");
        assert_eq!(
            body["auth"]["methods"][0]["actionHref"],
            "/api/v1/auth/access-key/session"
        );
        assert_eq!(body["mutationMechanisms"], serde_json::json!(["session"]));
    }

    #[tokio::test]
    async fn discovery_advertises_ai2ai_proposals_only_when_auth_and_dlc_are_active() {
        let inactive = app(access_key_state(
            "discovery-inactive-administrator-key-with-enough-entropy",
        ))
        .oneshot(
            Request::get("/.well-known/open-soverign-blog.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            json(inactive).await["endpoints"]["proposeRevision"]["available"],
            false
        );

        let mut state = access_key_state("discovery-active-administrator-key-with-enough-entropy");
        state.features = Arc::new(FeatureRegistry::from_requested("seo,ai_authorship").unwrap());
        let active = app(state)
            .oneshot(
                Request::get("/.well-known/open-soverign-blog.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            json(active).await["endpoints"]["proposeRevision"]["available"],
            true
        );
    }

    #[tokio::test]
    async fn unavailable_external_provider_does_not_break_public_reading() {
        let router = app(unavailable_external_state());
        let capabilities = router
            .clone()
            .oneshot(
                Request::get("/api/v1/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(capabilities.status(), StatusCode::OK);
        let body = json(capabilities).await;
        assert_eq!(body["studioAccess"], "admin_only");
        assert_eq!(body["auth"]["methods"][0]["kind"], "external");

        let feed = router
            .clone()
            .oneshot(Request::get("/api/v1/posts").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(feed.status(), StatusCode::OK);

        let login = router
            .oneshot(
                Request::get("/api/v1/auth/external/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(login.headers()[header::CACHE_CONTROL], "private, no-store");
    }

    #[tokio::test]
    async fn runner_discovery_requires_an_operational_feature_and_client() {
        let mut degraded = test_state(None);
        degraded.runner = Some(test_runner_client());
        let response = app(degraded)
            .oneshot(
                Request::get("/.well-known/open-soverign-blog.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            json(response).await["endpoints"]["runnerProfiles"]["available"],
            false
        );

        let mut active = test_state(None);
        let mut features = FeatureRegistry::from_requested("seo,code_runner").unwrap();
        features
            .activate_composed("rbac", "test owner memberships")
            .unwrap();
        features
            .activate_composed("comments", "test comment routes")
            .unwrap();
        features
            .set_runtime_status(
                "code_runner",
                ModuleStatus::Active,
                true,
                "test runner is ready",
            )
            .unwrap();
        active.features = Arc::new(features);
        active.runner = Some(test_runner_client());
        let response = app(active)
            .oneshot(
                Request::get("/.well-known/open-soverign-blog.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            json(response).await["endpoints"]["runnerProfiles"]["available"],
            true
        );
    }

    #[tokio::test]
    async fn mutations_are_read_only_without_an_owner_credential() {
        let response = app(test_state(None))
            .oneshot(
                Request::post("/api/v1/posts")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r##"{"title":"T","slug":"t","sourceMarkdown":"# T"}"##,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json(response).await["error"], "read_only");
    }

    #[tokio::test]
    async fn wrong_owner_credential_is_rejected() {
        let response = app(test_state(Some("correct")))
            .oneshot(
                Request::post("/api/v1/posts")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer wrong")
                    .body(Body::from(
                        r##"{"title":"T","slug":"t","sourceMarkdown":"# T"}"##,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authentication_precedes_json_extraction() {
        let response = app(test_state(Some("correct")))
            .oneshot(
                Request::post("/api/v1/posts")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{ definitely not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn first_party_images_roundtrip_without_active_content_formats() {
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRtest-payload";
        let router = app(test_state(Some("secret")));
        let upload = router
            .clone()
            .oneshot(
                Request::post("/api/v1/assets")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_TYPE, "image/png")
                    .header("x-osb-filename", "cover.png")
                    .body(Body::from(PNG))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(upload.status(), StatusCode::CREATED);
        let uploaded = json(upload).await;
        let url = uploaded["url"].as_str().unwrap();

        let download = router
            .clone()
            .oneshot(Request::get(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(download.status(), StatusCode::OK);
        assert_eq!(download.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(
            to_bytes(download.into_body(), 1024).await.unwrap().as_ref(),
            PNG
        );

        let svg = router
            .oneshot(
                Request::post("/api/v1/assets")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_TYPE, "image/svg+xml")
                    .body(Body::from("<svg><script>alert(1)</script></svg>"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(svg.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn owner_can_resume_a_draft_and_review_immutable_history() {
        let router = app(test_state(Some("secret")));
        let create = router
            .clone()
            .oneshot(
                Request::post("/api/v1/posts")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r##"{"title":"Draft","slug":"draft","sourceMarkdown":"one"}"##,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::CREATED);
        let created = json(create).await;
        assert_eq!(created["revision"]["actor"]["kind"], "human");
        assert_eq!(created["revision"]["actor"]["id"], "owner");
        let document_id = created["id"].as_str().unwrap();
        let base_revision_id = created["currentRevisionId"].as_str().unwrap();

        let private_without_token = router
            .clone()
            .oneshot(
                Request::get("/api/v1/admin/documents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(private_without_token.status(), StatusCode::UNAUTHORIZED);

        let revise = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/documents/{document_id}/revisions"))
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r##"{{"baseRevisionId":"{base_revision_id}","title":"Draft two","slug":"draft","sourceMarkdown":"two","idempotencyKey":"studio-test"}}"##
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revise.status(), StatusCode::CREATED);
        let revised = json(revise).await;
        assert_eq!(revised["revisionNumber"], 2);
        assert_eq!(revised["actor"]["kind"], "human");
        assert_eq!(revised["actor"]["id"], "owner");

        let documents = router
            .clone()
            .oneshot(
                Request::get("/api/v1/admin/documents")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let documents = json(documents).await;
        assert_eq!(documents.as_array().unwrap().len(), 1);
        assert_eq!(documents[0]["revision"]["title"], "Draft two");

        let history = router
            .oneshot(
                Request::get(format!("/api/v1/admin/documents/{document_id}/revisions"))
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let history = json(history).await;
        assert_eq!(history.as_array().unwrap().len(), 2);
        assert_eq!(history[0]["revisionNumber"], 2);
        assert_eq!(history[1]["revisionNumber"], 1);
    }

    #[tokio::test]
    async fn end_to_end_views_preserve_markdown_and_sanitize_intent_html() {
        let router = app(test_state(Some("secret")));
        let create = router
            .clone()
            .oneshot(
                Request::post("/api/v1/posts")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::from(
                        r##"{
                          "title":"AI2AI",
                          "slug":"ai2ai",
                          "sourceMarkdown":"# AI2AI\n\n<script>not executable</script>",
                          "intent":{
                            "format":"enhanced-html-v1",
                            "sourceHtml":"<h1 onclick='x()'>Intent</h1><iframe src='https://evil.invalid'></iframe>"
                          }
                        }"##,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::CREATED);
        let created = json(create).await;
        let document_id = created["id"].as_str().unwrap();
        let revision_id = created["currentRevisionId"].as_str().unwrap();

        let publish = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/documents/{document_id}/publish"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::from(format!(r#"{{"revisionId":"{revision_id}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(publish.status(), StatusCode::OK);

        let community_feed = router
            .clone()
            .oneshot(Request::get("/api/v1/feed").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let community_feed = json(community_feed).await;
        assert_eq!(community_feed["items"][0]["title"], "AI2AI");
        assert!(
            community_feed["items"][0]["blog"]["handle"]
                .as_str()
                .unwrap()
                .starts_with("legacy-")
        );

        let intent = router
            .clone()
            .oneshot(
                Request::get("/api/v1/posts/ai2ai?view=intent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let intent = json(intent).await;
        let html = intent["artifact"]["html"].as_str().unwrap();
        assert!(html.contains("<h1>Intent</h1>"));
        assert!(!html.contains("onclick"));
        assert!(!html.contains("iframe"));

        let source = router
            .oneshot(
                Request::get("/api/v1/posts/ai2ai/source.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            to_bytes(source.into_body(), 1024 * 1024).await.unwrap(),
            "# AI2AI\n\n<script>not executable</script>"
        );
    }

    #[tokio::test]
    async fn community_flow_keeps_the_published_revision_visible_while_editing() {
        let router = app(test_state(None));
        let register = router
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/register")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"email":"alice@example.test","password":"correct horse battery staple","handle":"alice","displayName":"Alice"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(register.status(), StatusCode::CREATED);
        assert_eq!(
            register.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let set_cookie = register.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"));
        assert!(set_cookie.contains("Path=/"));
        assert!(set_cookie.contains("Secure"));
        let cookie = set_cookie.split(';').next().unwrap().to_owned();
        let registered = json(register).await;
        assert_eq!(registered["state"], "authenticated");
        assert_eq!(registered["instanceAdministrator"], false);
        assert_eq!(registered["user"]["handle"], "alice");
        assert!(registered.get("blog").is_none());

        let create_blog = router
            .clone()
            .oneshot(
                Request::post("/api/v1/blogs")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(
                        r#"{"handle":"alice-notes","title":"Alice Notes","description":"Independent notes","themePreset":"forest"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_blog.status(), StatusCode::CREATED);
        let blog = json(create_blog).await;
        assert_eq!(blog["theme"]["presetId"], "forest");
        assert_eq!(blog["isPrimary"], false);

        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRcommunity-cover";
        let upload = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/assets")
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "image/png")
                    .header("x-osb-filename", "cover.png")
                    .body(Body::from(PNG))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(upload.status(), StatusCode::CREATED);
        assert_eq!(upload.headers()[header::CACHE_CONTROL], "private, no-store");
        let asset_url = json(upload).await["url"].as_str().unwrap().to_owned();
        let asset = router
            .clone()
            .oneshot(Request::get(asset_url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(asset.into_body(), 1024).await.unwrap().as_ref(),
            PNG
        );

        let unsupported_asset = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/assets")
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "text/plain")
                    .header("x-osb-filename", "notes.txt")
                    .body(Body::from("not an image"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            unsupported_asset.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(json(unsupported_asset).await["error"], "unsupported_asset");

        let oversized_asset = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/assets")
                    .header(header::COOKIE, &cookie)
                    .header(header::CONTENT_TYPE, "image/png")
                    .header("x-osb-filename", "oversized.png")
                    .body(Body::from(vec![0_u8; osb_assets_fs::MAX_ASSET_BYTES + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(oversized_asset.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(json(oversized_asset).await["error"], "asset_too_large");

        let create = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/documents")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(
                        r##"{"title":"Published title","slug":"continuity","sourceMarkdown":"# Public body"}"##,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::CREATED);
        let created = json(create).await;
        let document_id = created["id"].as_str().unwrap().to_owned();
        let first_revision = created["currentRevisionId"].as_str().unwrap().to_owned();

        let publish = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/studio/documents/{document_id}/publish"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(format!(
                        r#"{{"revisionId":"{first_revision}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(publish.status(), StatusCode::OK);

        let feed = router
            .clone()
            .oneshot(Request::get("/api/v1/feed").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(feed.status(), StatusCode::OK);
        assert!(
            feed.headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .starts_with("public")
        );
        let etag = feed.headers()[header::ETAG].to_str().unwrap().to_owned();
        let feed_json = json(feed).await;
        assert_eq!(feed_json["items"][0]["title"], "Published title");
        assert_eq!(feed_json["items"][0]["blog"]["handle"], "alice-notes");
        assert_eq!(feed_json["items"][0]["blog"]["isPrimary"], false);

        let archive = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/alice-notes/posts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(archive.status(), StatusCode::OK);
        let archive_etag = archive.headers()[header::ETAG].to_str().unwrap().to_owned();
        assert_eq!(json(archive).await["items"][0]["slug"], "continuity");
        let archive_not_modified = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/alice-notes/posts")
                    .header(header::IF_NONE_MATCH, archive_etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(archive_not_modified.status(), StatusCode::NOT_MODIFIED);

        let not_modified = router
            .clone()
            .oneshot(
                Request::get("/api/v1/feed")
                    .header(header::IF_NONE_MATCH, &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(not_modified.headers()[header::ETAG], etag);

        let revise = router
            .clone()
            .oneshot(
                Request::post(format!(
                    "/api/v1/studio/documents/{document_id}/revisions"
                ))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .body(Body::from(format!(
                    r##"{{"baseRevisionId":"{first_revision}","title":"Private draft title","slug":"continuity-next","sourceMarkdown":"# Draft body"}}"##
                )))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revise.status(), StatusCode::CREATED);
        let revised = json(revise).await;
        assert_eq!(revised["revision"]["title"], "Private draft title");
        assert_eq!(revised["publishedRevisionId"], first_revision);

        let direct_document = router
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/studio/documents/{document_id}"))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(direct_document.status(), StatusCode::OK);
        assert_eq!(
            json(direct_document).await["revision"]["title"],
            "Private draft title"
        );

        let public = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/alice-notes/posts/continuity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(public.status(), StatusCode::OK);
        let public = json(public).await;
        assert_eq!(public["title"], "Published title");
        assert_eq!(public["markdown"], "# Public body");

        let private = router
            .clone()
            .oneshot(
                Request::get("/api/v1/studio/documents")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(private.status(), StatusCode::OK);
        assert_eq!(
            json(private).await[0]["revision"]["title"],
            "Private draft title"
        );

        let logout = router
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/logout")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::OK);
        assert_eq!(json(logout).await["state"], "anonymous");
        let expired_session = router
            .clone()
            .oneshot(
                Request::get("/api/v1/session")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json(expired_session).await["state"], "anonymous");

        let login = router
            .oneshot(
                Request::post("/api/v1/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"email":"alice@example.test","password":"correct horse battery staple"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        assert_eq!(json(login).await["blog"]["handle"], "alice-notes");
    }

    #[tokio::test]
    async fn studio_ownership_and_comment_authors_are_server_scoped() {
        let state = test_state(Some("legacy-secret"));
        let repository = Arc::clone(&state.repository);
        let alice = repository
            .create_user(
                "alice@example.test",
                "alice",
                "Alice",
                "$argon2id$test-only",
            )
            .unwrap();
        let bob = repository
            .create_user("bob@example.test", "bob", "Bob", "$argon2id$test-only")
            .unwrap();
        let alice_site = repository
            .create_site(
                alice.id,
                "alice-blog",
                "Alice Blog",
                None,
                osb_storage_sqlite::ThemeProfile::Paper,
            )
            .unwrap();
        repository
            .create_site(
                bob.id,
                "bob-blog",
                "Bob Blog",
                None,
                osb_storage_sqlite::ThemeProfile::Ink,
            )
            .unwrap();
        let document = repository
            .create_document_in_owned_site(
                alice.id,
                NewDocument {
                    site_id: alice_site.id,
                    title: "Alice post".into(),
                    slug: "alice-post".into(),
                    source_markdown: "hello".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: alice.id.to_string(),
                        display_name: Some("Alice".into()),
                    },
                },
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                alice.id,
                alice_site.id,
                document.id,
                document.current_revision_id,
            )
            .unwrap();
        let raw_token = [7_u8; 32];
        let token_hash: [u8; 32] = Sha256::digest(raw_token).into();
        repository
            .create_session(
                bob.id,
                &token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        let bob_cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(raw_token));
        let router = app(state);

        let article_before_comment = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/alice-blog/posts/alice-post")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let article_etag = article_before_comment.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        let comments_before = router
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/posts/{}/comments", document.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let comments_etag = comments_before.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();

        let forbidden_revision = router
            .clone()
            .oneshot(
                Request::post(format!(
                    "/api/v1/studio/documents/{}/revisions",
                    document.id
                ))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &bob_cookie)
                .body(Body::from(format!(
                    r#"{{"baseRevisionId":"{}","title":"stolen","slug":"stolen","sourceMarkdown":"stolen"}}"#,
                    document.current_revision_id
                )))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forbidden_revision.status(), StatusCode::NOT_FOUND);

        let forbidden_get = router
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/studio/documents/{}", document.id))
                    .header(header::COOKIE, &bob_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forbidden_get.status(), StatusCode::NOT_FOUND);

        let legacy_cross_tenant_publish = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/documents/{}/publish", document.id))
                    .header(header::AUTHORIZATION, "Bearer legacy-secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"revisionId":"{}"}}"#,
                        document.current_revision_id
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy_cross_tenant_publish.status(), StatusCode::NOT_FOUND);

        let spoof = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/posts/{}/comments", document.id))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &bob_cookie)
                    .body(Body::from(
                        r#"{"sourceMarkdown":"hello","authorUserId":"spoofed"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spoof.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let create_comment = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/posts/{}/comments", document.id))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &bob_cookie)
                    .body(Body::from(
                        r#"{"sourceMarkdown":"hello <img src=x onerror=alert(1)>"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_comment.status(), StatusCode::CREATED);
        let comment = json(create_comment).await;
        assert_eq!(comment["author"]["id"], bob.id.to_string());
        assert!(!comment["artifactHtml"].as_str().unwrap().contains("<img"));

        let article_after_comment = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/alice-blog/posts/alice-post")
                    .header(header::IF_NONE_MATCH, &article_etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(article_after_comment.status(), StatusCode::NOT_MODIFIED);

        let comments = router
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/posts/{}/comments", document.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(comments.status(), StatusCode::OK);
        assert_ne!(comments.headers()[header::ETAG], comments_etag);
        assert_eq!(json(comments).await["items"][0]["author"]["handle"], "bob");
    }

    #[tokio::test]
    async fn owner_controls_collaborators_and_site_appearance_while_writer_only_drafts() {
        let mut state = test_state(None);
        state.collaboration_enabled = true;
        state.custom_css_enabled = true;
        let repository = Arc::clone(&state.repository);
        let owner = repository
            .create_user(
                "owner@example.test",
                "owner-settings",
                "Owner",
                "$argon2id$test-only",
            )
            .unwrap();
        let writer = repository
            .create_user(
                "writer@example.test",
                "writer-settings",
                "Writer",
                "$argon2id$test-only",
            )
            .unwrap();
        let site = repository
            .create_site(
                owner.id,
                "settings-blog",
                "Settings Blog",
                None,
                osb_storage_sqlite::ThemeProfile::Paper,
            )
            .unwrap();
        let owner_session_token = [10_u8; 32];
        let writer_token = [11_u8; 32];
        let owner_session_token_hash: [u8; 32] = Sha256::digest(owner_session_token).into();
        let writer_token_hash: [u8; 32] = Sha256::digest(writer_token).into();
        repository
            .create_session(
                owner.id,
                &owner_session_token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        repository
            .create_session(
                writer.id,
                &writer_token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        let owner_cookie = format!(
            "osb_session={}",
            URL_SAFE_NO_PAD.encode(owner_session_token)
        );
        let writer_cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(writer_token));
        let router = app(state);

        let invited = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/collaborators")
                    .header(header::COOKIE, &owner_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"email":"writer@example.test","role":"writer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invited.status(), StatusCode::CREATED);
        assert_eq!(json(invited).await["role"], "writer");

        let created = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/documents")
                    .header(header::COOKIE, &writer_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"title":"Writer draft","slug":"writer-draft","sourceMarkdown":"draft"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = json(created).await;
        let document_id = created["id"].as_str().unwrap();
        let revision_id = created["currentRevisionId"].as_str().unwrap();

        let writer_publish = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/studio/documents/{document_id}/publish"))
                    .header(header::COOKIE, &writer_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"revisionId":"{revision_id}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(writer_publish.status(), StatusCode::FORBIDDEN);

        let writer_settings = router
            .clone()
            .oneshot(
                Request::get("/api/v1/studio/settings")
                    .header(header::COOKIE, &writer_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(writer_settings.status(), StatusCode::FORBIDDEN);

        let settings = router
            .clone()
            .oneshot(
                Request::put("/api/v1/studio/settings")
                    .header(header::COOKIE, &owner_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"themePreset":"forest","customCss":".blog-profile { border-radius: 2rem; }"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(settings.status(), StatusCode::OK);
        let settings = json(settings).await;
        assert_eq!(settings["themePreset"], "forest");
        assert_eq!(settings["themeRevision"], 2);

        let public_blog = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/settings-blog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let public_blog = json(public_blog).await;
        assert_eq!(public_blog["theme"]["presetId"], "forest");
        assert!(public_blog["theme"].get("customCss").is_none());
        assert_eq!(
            public_blog["theme"]["customCssUrl"],
            "https://blog.example/api/v1/blogs/settings-blog/custom.css"
        );
        let site_css = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/settings-blog/custom.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(site_css.status(), StatusCode::OK);
        assert_eq!(
            site_css.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        let site_css = text(site_css).await;
        assert!(site_css.contains(&format!(
            "@scope (.osb-site-theme[data-site-id=\"{}\"])",
            site.id
        )));
        assert!(site_css.contains("border-radius: 2rem"));

        let unsafe_css = router
            .clone()
            .oneshot(
                Request::put("/api/v1/studio/settings")
                    .header(header::COOKIE, &owner_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"themePreset":"forest","customCss":"@import url(https://evil.example/x.css);"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unsafe_css.status(), StatusCode::BAD_REQUEST);

        let remove_owner = router
            .clone()
            .oneshot(
                Request::delete(format!("/api/v1/studio/collaborators/{}", owner.id))
                    .header(header::COOKIE, &owner_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(remove_owner.status(), StatusCode::BAD_REQUEST);

        let removed = router
            .clone()
            .oneshot(
                Request::delete(format!("/api/v1/studio/collaborators/{}", writer.id))
                    .header(header::COOKIE, &owner_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(removed.status(), StatusCode::OK);
        assert_eq!(json(removed).await["userId"], writer.id.to_string());
        assert!(repository.get_site_membership(writer.id, site.id).is_err());
    }

    #[tokio::test]
    async fn site_custom_css_is_rejected_when_operator_disabled_the_feature() {
        let mut state = test_state(None);
        state.custom_css_enabled = false;
        let repository = Arc::clone(&state.repository);
        let owner = repository
            .create_user(
                "css-owner@example.test",
                "css-owner",
                "CSS Owner",
                "$argon2id$test-only",
            )
            .unwrap();
        repository
            .create_site(
                owner.id,
                "css-disabled",
                "CSS Disabled",
                None,
                osb_storage_sqlite::ThemeProfile::Paper,
            )
            .unwrap();
        let token = [12_u8; 32];
        let token_hash: [u8; 32] = Sha256::digest(token).into();
        repository
            .create_session(
                owner.id,
                &token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        let cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(token));
        let router = app(state);
        let response = router
            .clone()
            .oneshot(
                Request::put("/api/v1/studio/settings")
                    .header(header::COOKIE, cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"themePreset":"paper","customCss":null}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json(response).await["error"], "bad_request");
        let public_css = router
            .oneshot(
                Request::get("/api/v1/blogs/css-disabled/custom.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(public_css.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delivery_only_is_anonymous_and_fails_closed() {
        let mut state = test_state(Some("legacy-secret"));
        state.delivery_only = true;
        let router = app(state);
        let session = router
            .clone()
            .oneshot(
                Request::get("/api/v1/session")
                    .header(header::COOKIE, "osb_session=invalid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::OK);
        let session = json(session).await;
        assert_eq!(session["state"], "anonymous");
        assert_eq!(session["registrationOpen"], false);

        let register = router
            .clone()
            .oneshot(
                Request::post("/api/v1/auth/register")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{ definitely not valid JSON"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(register.status(), StatusCode::SERVICE_UNAVAILABLE);

        let legacy = router
            .oneshot(
                Request::post("/api/v1/posts")
                    .header(header::AUTHORIZATION, "Bearer legacy-secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"title":"T","slug":"t","sourceMarkdown":"T"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn registration_is_closed_unless_explicitly_enabled() {
        let mut state = test_state(None);
        state.registration_open = false;
        let router = app(state);
        let session = router
            .clone()
            .oneshot(Request::get("/api/v1/session").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(json(session).await["registrationOpen"], false);
        let register = router
            .oneshot(
                Request::post("/api/v1/auth/register")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"email":"closed@example.test","password":"long-enough","handle":"closed","displayName":"Closed"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(register.status(), StatusCode::FORBIDDEN);
        assert_eq!(json(register).await["error"], "registration_closed");
    }

    #[tokio::test]
    async fn community_html_is_crawlable_route_aware_cached_and_xss_safe() {
        let mut state = test_state(None);
        state.custom_css_enabled = true;
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "blog".into(),
            no_index: false,
        });
        let owner = state
            .repository
            .create_user(
                "alice@example.test",
                "alice",
                "Alice <img src=x onerror=alert(1)>",
                "$argon2id$test-only",
            )
            .unwrap();
        let site = state
            .repository
            .create_site(
                owner.id,
                "alice-notes",
                "Alice <Notes>",
                Some("Notes \" onmouseover=\"bad & <script>alert(1)</script>"),
                osb_storage_sqlite::ThemeProfile::Forest,
            )
            .unwrap();
        state
            .repository
            .change_site_appearance(
                owner.id,
                site.id,
                osb_storage_sqlite::ThemeProfile::Forest,
                Some(".article-content { line-height: 1.75; }"),
            )
            .unwrap();
        let first = state
            .repository
            .create_document_in_owned_site(
                owner.id,
                NewDocument {
                    site_id: site.id,
                    title: "Old title".into(),
                    slug: "old-slug".into(),
                    source_markdown: "Old body".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name.clone()),
                    },
                },
            )
            .unwrap();
        state
            .repository
            .publish_document_in_owned_site(owner.id, site.id, first.id, first.current_revision_id)
            .unwrap();
        let canonical_revision = state
            .repository
            .revise_document_in_owned_site(
                owner.id,
                site.id,
                ProposedRevision {
                    document_id: first.id,
                    base_revision_id: first.current_revision_id,
                    title: "A </title><script>alert(1)</script> story".into(),
                    slug: "canonical-slug".into(),
                    source_markdown:
                        "# Crawlable heading\n\nSafe body.\n\n<img src=x onerror=alert(1)>".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: PublicAuthorship {
                        kind: PublicAuthorshipKind::AiAssisted,
                        generator: Some("test-agent <unsafe>".into()),
                        human_reviewed: true,
                    },
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name.clone()),
                    },
                    idempotency_key: Some("community-ssr-canonical".into()),
                },
            )
            .unwrap();
        state
            .repository
            .publish_document_in_owned_site(owner.id, site.id, first.id, canonical_revision.id)
            .unwrap();
        let router = app(state);

        let blog = router
            .clone()
            .oneshot(
                Request::get("/@alice-notes")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blog.status(), StatusCode::OK);
        assert_eq!(
            blog.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert!(
            blog.headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .starts_with("public")
        );
        let blog_html = text(blog).await;
        assert!(blog_html.contains("<base href=\"/base/\" />"));
        assert!(blog_html.contains("<meta name=\"osb-base-path\" content=\"/base\" />"));
        assert!(
            blog_html
                .contains("<title>Alice &lt;Notes&gt; (@alice-notes) · OpenSoverignBlog</title>")
        );
        assert!(
            blog_html.contains(
                "<link rel=\"canonical\" href=\"https://blog.example/base/@alice-notes\">"
            )
        );
        assert!(blog_html.contains("<meta property=\"og:type\" content=\"website\">"));
        assert!(
            blog_html.contains("href=\"https://blog.example/base/@alice-notes/canonical-slug\"")
        );
        assert!(blog_html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(blog_html.contains("AI 보조 · test-agent &lt;unsafe&gt; · 사람 검토"));
        assert!(!blog_html.contains("<script>alert(1)</script>"));

        let article = router
            .clone()
            .oneshot(
                Request::get("/@alice-notes/canonical-slug")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(article.status(), StatusCode::OK);
        let etag = article.headers()[header::ETAG].to_str().unwrap().to_owned();
        let article_html = text(article).await;
        assert!(article_html.contains(
            "<title>A &lt;/title&gt;&lt;script&gt;alert(1)&lt;/script&gt; story · Alice &lt;Notes&gt;</title>"
        ));
        assert!(blog_html.contains("class=\"osb-site-frame\""));
        assert!(blog_html.contains("class=\"blog-monogram\" aria-hidden=\"true\">A&lt;</span>"));
        assert!(blog_html.contains(&format!(
            "class=\"blog-page osb-site-theme\" data-site-id=\"{}\"",
            site.id
        )));
        assert!(blog_html.contains(
            "data-osb-blog-custom-css href=\"https://blog.example/base/api/v1/blogs/alice-notes/custom.css\""
        ));
        assert!(article_html.contains("<meta property=\"og:type\" content=\"article\">"));
        assert!(article_html.contains("<meta name=\"twitter:title\""));
        assert!(article_html.contains("<meta name=\"twitter:description\""));
        assert!(article_html.contains("<meta property=\"article:published_time\""));
        assert!(article_html.contains(
            "<link rel=\"canonical\" href=\"https://blog.example/base/@alice-notes/canonical-slug\">"
        ));
        assert!(article_html.contains("<h1>Crawlable heading</h1>"));
        assert!(article_html.contains("Safe body."));
        assert!(article_html.contains("AI 보조 · test-agent &lt;unsafe&gt; · 사람 검토"));
        assert!(!article_html.contains("</title><script>alert(1)</script>"));
        assert!(!article_html.contains("<img src=x onerror=alert(1)>"));

        let not_modified = router
            .clone()
            .oneshot(
                Request::get("/@alice-notes/canonical-slug")
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);

        for alias in ["/@alice-notes/old-slug", "/@ALICE-NOTES/canonical-slug"] {
            let redirect = router
                .clone()
                .oneshot(Request::get(alias).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(redirect.status(), StatusCode::PERMANENT_REDIRECT);
            assert_eq!(
                redirect.headers()[header::LOCATION],
                "https://blog.example/base/@alice-notes/canonical-slug"
            );
        }
    }

    #[tokio::test]
    async fn deeply_nested_legacy_alias_redirects_to_absolute_primary_category_url() {
        let mut state =
            access_key_state("legacy-alias-test-administrator-access-key-with-enough-entropy");
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://eff0rtchung.kr/").unwrap(),
            article_base_path: "blog".into(),
            no_index: false,
        });
        let control = state.repository.get_admin_control_plane().unwrap();
        let created_at = chrono::DateTime::parse_from_rfc3339("2020-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        state
            .repository
            .import_offline_batch(
                control.owner_user_id,
                state.site_id,
                osb_storage_sqlite::OfflineImportBatch {
                    source: "legacy-static-site".into(),
                    owner_display_name: "me".into(),
                    categories: vec![osb_storage_sqlite::OfflineImportCategory {
                        slug: "yangja".into(),
                        title: "Yangja".into(),
                        description: None,
                    }],
                    posts: vec![osb_storage_sqlite::OfflineImportPost {
                        source_id: "yangja:grover".into(),
                        title: "Grover".into(),
                        slug: "grover".into(),
                        source_markdown: "# Grover".into(),
                        created_at,
                        author_id: "me".into(),
                        author_display_name: "me".into(),
                        primary_category: "yangja".into(),
                        human_reviewed: true,
                        aliases: vec![
                            osb_storage_sqlite::OfflineImportAlias {
                                path: "topics/algorithms/grover.html".into(),
                                created_at,
                            },
                            osb_storage_sqlite::OfflineImportAlias {
                                path: "legacy-grover".into(),
                                created_at,
                            },
                        ],
                    }],
                },
                false,
            )
            .unwrap();
        state.legacy_alias_slots = Arc::new(tokio::sync::Semaphore::new(1));
        let saturated = Arc::clone(&state.legacy_alias_slots)
            .try_acquire_owned()
            .unwrap();
        let router = app(state);

        let canonical = router
            .clone()
            .oneshot(
                Request::get("/yangja/grover")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(canonical.status(), StatusCode::OK);

        let saturated_miss = router
            .clone()
            .oneshot(
                Request::get("/topics/algorithms/grover.html")
                    .header(header::ACCEPT, "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(saturated_miss.status(), StatusCode::NOT_FOUND);
        drop(saturated);

        for alias in ["/topics/algorithms/grover.html", "/legacy-grover"] {
            for method in [Method::GET, Method::HEAD] {
                let redirect = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(alias)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(redirect.status(), StatusCode::MOVED_PERMANENTLY, "{alias}");
                assert_eq!(
                    redirect.headers()[header::LOCATION],
                    "https://eff0rtchung.kr/yangja/grover",
                    "{alias}"
                );
            }
        }
    }

    #[tokio::test]
    async fn primary_category_lookup_capacity_bounds_landing_and_post_queries() {
        let mut state =
            access_key_state("category-capacity-test-administrator-access-key-with-enough-entropy");
        let control = state.repository.get_admin_control_plane().unwrap();
        let owner = state
            .repository
            .get_user_by_id(control.owner_user_id)
            .unwrap();
        let category = state
            .repository
            .create_category(
                owner.id,
                state.site_id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "yangja".into(),
                    title: "Yangja".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let document = state
            .repository
            .create_document_in_writable_site_with_category(
                owner.id,
                NewDocument {
                    site_id: state.site_id,
                    title: "Grover".into(),
                    slug: "grover".into(),
                    source_markdown: "# Grover".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name),
                    },
                },
                Some(category.id),
            )
            .unwrap();
        state
            .repository
            .publish_document_in_owned_site(
                owner.id,
                state.site_id,
                document.id,
                document.current_revision_id,
            )
            .unwrap();

        state.primary_category_slots = Arc::new(tokio::sync::Semaphore::new(1));
        let saturated = Arc::clone(&state.primary_category_slots)
            .try_acquire_owned()
            .unwrap();
        let router = app(state);

        for path in ["/yangja", "/yangja/grover"] {
            let response = router
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::ACCEPT, "text/html")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(response.headers()[header::RETRY_AFTER], "1", "{path}");
        }

        drop(saturated);
        for path in ["/yangja", "/yangja/grover"] {
            let response = router
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::ACCEPT, "text/html")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
    }

    #[tokio::test]
    async fn community_blog_ssr_uses_natural_category_post_links() {
        let mut state = test_state(None);
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "blog".into(),
            no_index: false,
        });
        let owner = state
            .repository
            .create_user(
                "category-listing@example.test",
                "category-listing-owner",
                "Category listing owner",
                "$argon2id$test-only",
            )
            .unwrap();
        let site = state
            .repository
            .create_site(
                owner.id,
                "category-listing",
                "Category listing",
                None,
                ThemeProfile::Paper,
            )
            .unwrap();
        let category = state
            .repository
            .create_category(
                owner.id,
                site.id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "yangja".into(),
                    title: "Yangja".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let post = state
            .repository
            .create_document_in_writable_site_with_category(
                owner.id,
                NewDocument {
                    site_id: site.id,
                    title: "Natural route".into(),
                    slug: "measurement".into(),
                    source_markdown: "# Natural route".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name.clone()),
                    },
                },
                Some(category.id),
            )
            .unwrap();
        state
            .repository
            .publish_document_in_owned_site(owner.id, site.id, post.id, post.current_revision_id)
            .unwrap();

        let repository = Arc::clone(&state.repository);
        let router = app(state);
        let natural = router
            .clone()
            .oneshot(
                Request::get("/@category-listing/yangja/measurement")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(natural.status(), StatusCode::OK);
        let natural = text(natural).await;
        assert!(natural.contains("<h1>Natural route</h1>"));
        assert!(natural.contains("<link rel=\"canonical\" href=\"https://blog.example/base/@category-listing/yangja/measurement\">"));

        let leaf_fallback = router
            .clone()
            .oneshot(
                Request::get("/@category-listing/measurement")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(leaf_fallback.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            leaf_fallback.headers()[header::LOCATION],
            "https://blog.example/base/@category-listing/yangja/measurement"
        );

        let response = router
            .clone()
            .oneshot(
                Request::get("/@category-listing")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = text(response).await;
        assert!(
            html.contains(
                "href=\"https://blog.example/base/@category-listing/yangja/measurement\""
            )
        );
        assert!(!html.contains("href=\"https://blog.example/base/@category-listing/measurement\""));

        let second_category = repository
            .create_category(
                owner.id,
                site.id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "classical".into(),
                    title: "Classical".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let duplicate_leaf = repository
            .create_document_in_writable_site_with_category(
                owner.id,
                NewDocument {
                    site_id: site.id,
                    title: "A second measurement".into(),
                    slug: "measurement".into(),
                    source_markdown: "# A second measurement".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name.clone()),
                    },
                },
                Some(second_category.id),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                owner.id,
                site.id,
                duplicate_leaf.id,
                duplicate_leaf.current_revision_id,
            )
            .unwrap();
        let ambiguous_leaf = router
            .oneshot(
                Request::get("/@category-listing/measurement")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ambiguous_leaf.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn member_category_landing_is_a_safe_no_js_archive() {
        let mut state = test_state(None);
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "blog".into(),
            no_index: true,
        });
        state.custom_css_enabled = true;
        let owner = state
            .repository
            .create_user(
                "member-category@example.test",
                "member-category-owner",
                "Member <Owner>",
                "$argon2id$test-only",
            )
            .unwrap();
        let site = state
            .repository
            .create_site(
                owner.id,
                "member-notes",
                "Member <Notes>",
                None,
                ThemeProfile::Paper,
            )
            .unwrap();
        state
            .repository
            .change_site_appearance(
                owner.id,
                site.id,
                ThemeProfile::Terminal,
                Some(".blog-page { color: rebeccapurple; }"),
            )
            .unwrap();
        let category = state
            .repository
            .create_category(
                owner.id,
                site.id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "field-notes".into(),
                    title: "Field <Notes>".into(),
                    description: Some("Public & curated".into()),
                    theme_profile: None,
                },
            )
            .unwrap();
        let source_markdown = format!(
            "# Public heading\n\n{} PRIVATE_SOURCE_SENTINEL",
            "A public excerpt sentence. ".repeat(20)
        );
        let post = state
            .repository
            .create_document_in_writable_site_with_category(
                owner.id,
                NewDocument {
                    site_id: site.id,
                    title: "Public <Post>".into(),
                    slug: "first-observation".into(),
                    source_markdown,
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name.clone()),
                    },
                },
                Some(category.id),
            )
            .unwrap();
        state
            .repository
            .publish_document_in_owned_site(owner.id, site.id, post.id, post.current_revision_id)
            .unwrap();
        let repository = Arc::clone(&state.repository);
        let router = app(state);

        let response = router
            .clone()
            .oneshot(
                Request::get("/@member-notes/field-notes")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .extensions()
                .get::<PrimaryCategoryCacheableResponse>()
                .is_none()
        );
        let html = text(response).await;
        assert!(html.contains("<base href=\"/base/\" />"));
        assert!(html.contains("<h1>Field &lt;Notes&gt;</h1>"));
        assert!(html.contains("Public &amp; curated"));
        assert!(html.contains("data-theme=\"terminal\""));
        assert!(html.contains(
            "data-osb-blog-custom-css href=\"https://blog.example/base/api/v1/blogs/member-notes/custom.css\""
        ));
        assert!(html.contains(
            "<link rel=\"canonical\" href=\"https://blog.example/base/@member-notes/field-notes\">"
        ));
        assert!(html.contains(
            "<meta property=\"og:url\" content=\"https://blog.example/base/@member-notes/field-notes\">"
        ));
        assert!(html.contains("<meta property=\"og:type\" content=\"website\">"));
        assert!(html.contains("<meta name=\"robots\" content=\"noindex,nofollow\">"));
        assert!(html.contains(
            "href=\"https://blog.example/base/@member-notes/field-notes/first-observation\""
        ));
        assert!(html.contains("Public &lt;Post&gt;"));
        assert!(html.contains("A public excerpt sentence."));
        assert!(!html.contains("PRIVATE_SOURCE_SENTINEL"));
        assert!(!html.contains("<h1>Public heading</h1>"));

        let head = router
            .clone()
            .oneshot(
                Request::head("/@member-notes/field-notes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert!(head.headers().contains_key(header::ETAG));
        assert!(text(head).await.is_empty());

        repository
            .archive_category(owner.id, site.id, category.id)
            .unwrap();
        let archived = router
            .clone()
            .oneshot(
                Request::get("/@member-notes/field-notes")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(archived.status(), StatusCode::OK);
        assert!(text(archived).await.contains("Public &lt;Post&gt;"));

        let missing = router
            .oneshot(
                Request::get("/@member-notes/not-a-category")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn primary_category_posts_are_machine_followable_and_server_rendered() {
        let mut state = access_key_state(
            "machine-index-primary-category-administrator-key-with-enough-entropy",
        );
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "blog".into(),
            no_index: true,
        });
        let site = state.repository.get_site_by_id(state.site_id).unwrap();
        let owner = state.repository.get_user_by_id(site.owner_user_id).unwrap();
        state.custom_css_enabled = true;
        state
            .repository
            .change_site_appearance(
                owner.id,
                site.id,
                ThemeProfile::Paper,
                Some(".blog-page { letter-spacing: 0.01em; }"),
            )
            .unwrap();
        let category = state
            .repository
            .create_category(
                owner.id,
                site.id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "yangja".into(),
                    title: "Yangja".into(),
                    description: Some("Primary category archive".into()),
                    theme_profile: Some(ThemeProfile::Forest),
                },
            )
            .unwrap();
        let post = state
            .repository
            .create_document_in_writable_site_with_category(
                owner.id,
                NewDocument {
                    site_id: site.id,
                    title: "Primary natural route".into(),
                    slug: "measurement".into(),
                    source_markdown: "# Primary natural route\n\nPortable body.".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name.clone()),
                    },
                },
                Some(category.id),
            )
            .unwrap();
        state
            .repository
            .publish_document_in_owned_site(owner.id, site.id, post.id, post.current_revision_id)
            .unwrap();
        let repository = Arc::clone(&state.repository);
        let router = app(state);

        let index = router
            .clone()
            .oneshot(Request::get("/api/v1/posts").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(index.status(), StatusCode::OK);
        let index = json(index).await;
        assert_eq!(index[0]["slug"], "measurement");
        assert_eq!(index[0]["routePath"], "yangja/measurement");
        assert_eq!(
            index[0]["apiHref"],
            "https://blog.example/base/api/v1/posts/yangja/measurement"
        );
        assert_eq!(
            index[0]["sourceHref"],
            "https://blog.example/base/api/v1/posts/yangja/measurement/source.md"
        );

        let machine_view = router
            .clone()
            .oneshot(
                Request::get("/api/v1/posts/yangja/measurement")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(machine_view.status(), StatusCode::OK);
        assert_eq!(json(machine_view).await["canonicalSlug"], "measurement");
        let source = router
            .clone()
            .oneshot(
                Request::get("/api/v1/posts/yangja/measurement/source.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(source.status(), StatusCode::OK);
        assert_eq!(
            text(source).await,
            "# Primary natural route\n\nPortable body."
        );

        let natural = router
            .clone()
            .oneshot(
                Request::get("/yangja/measurement")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(natural.status(), StatusCode::OK);
        assert_eq!(natural.headers()[header::VARY], "Accept");
        assert!(
            natural
                .extensions()
                .get::<PrimaryCategoryCacheableResponse>()
                .is_some()
        );
        let natural = text(natural).await;
        assert!(natural.contains("<h1>Primary natural route</h1>"));
        assert!(natural.contains("Portable body."));
        assert!(natural.contains("data-theme=\"forest\""));
        assert!(natural.contains(
            "<link rel=\"canonical\" href=\"https://blog.example/base/yangja/measurement\">"
        ));

        let natural_head = router
            .clone()
            .oneshot(
                Request::head("/yangja/measurement")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(natural_head.status(), StatusCode::OK);
        assert_eq!(natural_head.headers()[header::VARY], "Accept");
        assert!(natural_head.headers().contains_key(header::ETAG));
        assert!(text(natural_head).await.is_empty());

        let member_alias = router
            .clone()
            .oneshot(
                Request::get("/@test-blog/yangja/measurement")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(member_alias.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            member_alias.headers()[header::LOCATION],
            "https://blog.example/base/yangja/measurement"
        );

        let category_landing = router
            .clone()
            .oneshot(Request::get("/yangja").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(category_landing.status(), StatusCode::OK);
        assert_eq!(category_landing.headers()[header::VARY], "Accept");
        assert!(
            category_landing
                .extensions()
                .get::<PrimaryCategoryCacheableResponse>()
                .is_some()
        );
        let category_landing = text(category_landing).await;
        assert!(category_landing.contains("<h1>Yangja</h1>"));
        assert!(category_landing.contains("Primary category archive"));
        assert!(category_landing.contains("data-theme=\"forest\""));
        assert!(category_landing.contains(
            "data-osb-blog-custom-css href=\"https://blog.example/base/api/v1/blogs/test-blog/custom.css\""
        ));
        assert!(
            category_landing
                .contains("<link rel=\"canonical\" href=\"https://blog.example/base/yangja\">")
        );
        assert!(
            category_landing.contains(
                "<meta property=\"og:url\" content=\"https://blog.example/base/yangja\">"
            )
        );
        assert!(category_landing.contains("<meta name=\"robots\" content=\"noindex,nofollow\">"));
        assert!(category_landing.contains("href=\"https://blog.example/base/yangja/measurement\""));
        assert!(
            !category_landing
                .contains("href=\"https://blog.example/base/@test-blog/yangja/measurement\"")
        );

        let category_head = router
            .clone()
            .oneshot(Request::head("/yangja").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(category_head.status(), StatusCode::OK);
        assert_eq!(category_head.headers()[header::VARY], "Accept");
        assert!(category_head.headers().contains_key(header::ETAG));
        assert!(text(category_head).await.is_empty());

        let curl_category_landing = router
            .clone()
            .oneshot(
                Request::get("/yangja")
                    .header(header::ACCEPT, "*/*")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(curl_category_landing.status(), StatusCode::OK);

        let category_member_alias = router
            .clone()
            .oneshot(
                Request::get("/@test-blog/yangja")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            category_member_alias.status(),
            StatusCode::PERMANENT_REDIRECT
        );
        assert_eq!(
            category_member_alias.headers()[header::LOCATION],
            "https://blog.example/base/yangja"
        );

        let primary_blog = router
            .clone()
            .oneshot(Request::get("/@test-blog").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(primary_blog.status(), StatusCode::OK);
        let primary_blog = text(primary_blog).await;
        assert!(primary_blog.contains("href=\"https://blog.example/base/yangja/measurement\""));
        assert!(
            !primary_blog
                .contains("href=\"https://blog.example/base/@test-blog/yangja/measurement\"")
        );

        let non_html_natural = router
            .clone()
            .oneshot(
                Request::get("/yangja/measurement")
                    .header(header::ACCEPT, "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(non_html_natural.status(), StatusCode::NOT_FOUND);
        assert_eq!(non_html_natural.headers()[header::VARY], "Accept");

        for legacy in ["/blog/measurement", "/@test-blog/measurement"] {
            let redirect = router
                .clone()
                .oneshot(Request::get(legacy).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                redirect.status(),
                StatusCode::PERMANENT_REDIRECT,
                "{legacy}"
            );
            assert_eq!(
                redirect.headers()[header::LOCATION],
                "https://blog.example/base/yangja/measurement",
                "{legacy}"
            );
        }

        let duplicate_category = repository
            .create_category(
                owner.id,
                site.id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "classical".into(),
                    title: "Classical".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();
        let duplicate = repository
            .create_document_in_writable_site_with_category(
                owner.id,
                NewDocument {
                    site_id: site.id,
                    title: "Duplicate primary leaf".into(),
                    slug: "measurement".into(),
                    source_markdown: "# Duplicate primary leaf".into(),
                    embeds: vec![],
                    intent: None,
                    ontology: None,
                    authorship: Default::default(),
                    ai_summary: None,
                    actor: RevisionActor {
                        kind: RevisionActorKind::Human,
                        id: owner.id.to_string(),
                        display_name: Some(owner.display_name.clone()),
                    },
                },
                Some(duplicate_category.id),
            )
            .unwrap();
        repository
            .publish_document_in_owned_site(
                owner.id,
                site.id,
                duplicate.id,
                duplicate.current_revision_id,
            )
            .unwrap();
        for ambiguous in ["/blog/measurement", "/@test-blog/measurement"] {
            let response = router
                .clone()
                .oneshot(Request::get(ambiguous).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{ambiguous}");
        }
    }

    #[tokio::test]
    async fn disabling_seo_removes_public_metadata_from_community_and_legacy_html() {
        let mut state = test_state(None);
        state.features = Arc::new(FeatureRegistry::from_requested("").unwrap());
        seed_community_post(&state, "no-seo", "no-seo-blog", "Plain post", "plain-post");
        let legacy_site = state.repository.ensure_legacy_site(state.site_id).unwrap();
        let legacy = state
            .repository
            .create_document(NewDocument {
                site_id: legacy_site.id,
                title: "Legacy without SEO".into(),
                slug: "legacy-without-seo".into(),
                source_markdown: "# Legacy body".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                authorship: Default::default(),
                ai_summary: None,
                actor: RevisionActor {
                    kind: RevisionActorKind::Human,
                    id: "legacy-owner".into(),
                    display_name: None,
                },
            })
            .unwrap();
        state
            .repository
            .publish(legacy.id, legacy.current_revision_id)
            .unwrap();
        let router = app(state);

        for path in ["/@no-seo-blog", "/@no-seo-blog/plain-post"] {
            let response = router
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::ACCEPT, "text/html")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let html = text(response).await;
            assert!(html.contains("<title>"), "{path}");
            assert!(!html.contains("rel=\"canonical\""), "{path}");
            assert!(!html.contains("property=\"og:"), "{path}");
            assert!(!html.contains("name=\"twitter:"), "{path}");
        }

        let legacy = router
            .oneshot(
                Request::get("/blog/legacy-without-seo")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy.status(), StatusCode::OK);
        let html = text(legacy).await;
        assert!(html.contains("<title>Legacy without SEO</title>"));
        assert!(!html.contains("rel=\"canonical\""));
        assert!(!html.contains("property=\"og:"));
        assert!(!html.contains("name=\"twitter:"));
    }

    #[tokio::test]
    async fn provisioned_primary_site_uses_community_canonical_route() {
        let mut state = access_key_state("correct horse battery staple");
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "blog".into(),
            no_index: false,
        });
        let site = state.repository.get_site_by_id(state.site_id).unwrap();
        let document = state
            .repository
            .create_document(NewDocument {
                site_id: state.site_id,
                title: "Owned post".into(),
                slug: "old-owned-post".into(),
                source_markdown: "# Owned post\n\nPublic body.".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                authorship: Default::default(),
                ai_summary: None,
                actor: RevisionActor {
                    kind: RevisionActorKind::Human,
                    id: site.owner_user_id.to_string(),
                    display_name: Some("Test owner".into()),
                },
            })
            .unwrap();
        state
            .repository
            .publish(document.id, document.current_revision_id)
            .unwrap();
        let revision = state
            .repository
            .append_revision(ProposedRevision {
                document_id: document.id,
                base_revision_id: document.current_revision_id,
                title: "Owned post".into(),
                slug: "owned-post".into(),
                source_markdown: "# Owned post\n\nPublic body.".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                authorship: Default::default(),
                ai_summary: None,
                actor: RevisionActor {
                    kind: RevisionActorKind::Human,
                    id: site.owner_user_id.to_string(),
                    display_name: Some("Test owner".into()),
                },
                idempotency_key: Some("provisioned-primary-canonical".into()),
            })
            .unwrap();
        state.repository.publish(document.id, revision.id).unwrap();
        let router = app(state);

        let community_article = router
            .clone()
            .oneshot(
                Request::get("/@test-blog/owned-post")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(community_article.status(), StatusCode::OK);
        assert!(text(community_article).await.contains(
            "<link rel=\"canonical\" href=\"https://blog.example/base/@test-blog/owned-post\">"
        ));

        let legacy_article = router
            .clone()
            .oneshot(
                Request::get("/blog/old-owned-post?view=markdown_source")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy_article.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            legacy_article.headers()[header::LOCATION],
            "https://blog.example/base/@test-blog/owned-post?view=markdown_source"
        );

        let sitemap = router
            .oneshot(Request::get("/sitemap.xml").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(sitemap.status(), StatusCode::OK);
        let sitemap = text(sitemap).await;
        let canonical = "<loc>https://blog.example/base/@test-blog/owned-post</loc>";
        assert_eq!(sitemap.matches(canonical).count(), 1);
        assert!(!sitemap.contains("<loc>https://blog.example/base/blog/owned-post</loc>"));
    }

    #[tokio::test]
    async fn sitemap_includes_published_posts_from_every_community_blog() {
        let state = test_state(None);
        seed_community_post(&state, "alice", "alice-notes", "Alice post", "first");
        seed_community_post(&state, "bob", "bob-notes", "Bob post", "second");
        let legacy_site = state.repository.ensure_legacy_site(state.site_id).unwrap();
        let legacy = state
            .repository
            .create_document(NewDocument {
                site_id: state.site_id,
                title: "Legacy post".into(),
                slug: "legacy".into(),
                source_markdown: "Legacy body".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                authorship: Default::default(),
                ai_summary: None,
                actor: RevisionActor {
                    kind: RevisionActorKind::Human,
                    id: "owner".into(),
                    display_name: None,
                },
            })
            .unwrap();
        state
            .repository
            .publish(legacy.id, legacy.current_revision_id)
            .unwrap();
        let router = app(state);

        let legacy_alias = router
            .clone()
            .oneshot(
                Request::get(format!("/@{}/legacy", legacy_site.handle))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy_alias.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            legacy_alias.headers()[header::LOCATION],
            "https://blog.example/blog/legacy"
        );

        let legacy_article = router
            .clone()
            .oneshot(Request::get("/blog/legacy").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(legacy_article.status(), StatusCode::OK);
        assert!(
            text(legacy_article)
                .await
                .contains("<link rel=\"canonical\" href=\"https://blog.example/blog/legacy\">")
        );

        let response = router
            .clone()
            .oneshot(Request::get("/sitemap.xml").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/xml; charset=utf-8"
        );
        let etag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        let sitemap = text(response).await;
        assert!(sitemap.contains("<loc>https://blog.example/@alice-notes/first</loc>"));
        assert!(sitemap.contains("<loc>https://blog.example/@bob-notes/second</loc>"));
        assert!(sitemap.contains("<loc>https://blog.example/blog/legacy</loc>"));
        assert!(!sitemap.contains(&format!("/@{}/legacy", legacy_site.handle)));
        assert!(sitemap.matches("<url>").count() <= SITEMAP_URL_LIMIT);
        assert!(sitemap.contains("<lastmod>"));

        let not_modified = router
            .oneshot(
                Request::get("/sitemap.xml")
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn spa_deep_links_serve_the_index() {
        let mut state = test_state(None);
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "blog".into(),
            no_index: false,
        });
        let router = app(state);
        for path in [
            "/",
            "/index.html",
            "/login",
            "/onboarding",
            "/studio",
            "/studio/write",
        ] {
            let response = router
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "deep link {path}");
        }

        for path in ["/@alice", "/@alice/post"] {
            let response = router
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "missing public route {path}"
            );
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/html; charset=utf-8"
            );
            let shell = text(response).await;
            assert!(shell.contains("<base href=\"/base/\" />"));
            assert!(shell.contains("<div id=\"root\"></div>"));
        }

        let home = router
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            home.headers()[header::CACHE_CONTROL],
            "public, max-age=0, must-revalidate"
        );
        let home = text(home).await;
        assert!(home.contains("<base href=\"/base/\" />"));
        assert!(home.contains("<meta name=\"osb-base-path\" content=\"/base\" />"));

        let unknown_navigation = router
            .clone()
            .oneshot(
                Request::get("/some-client-side-404")
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown_navigation.status(), StatusCode::OK);
        assert_eq!(unknown_navigation.headers()[header::VARY], "Accept");
        assert!(
            unknown_navigation
                .extensions()
                .get::<PrimaryCategoryCacheableResponse>()
                .is_none()
        );
        for path in ["/api/", "/api/v2/missing", "/assets/does-not-exist.js"] {
            let response = router
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "reserved {path}");
        }
    }

    #[tokio::test]
    async fn semantic_flags_remove_disabled_interaction_and_discovery_routes() {
        let mut state = test_state(None);
        state.comments_enabled = false;
        state.local_auth_enabled = false;
        state.agent_discovery_enabled = false;
        state.custom_css_enabled = false;
        let router = app(state);

        for request in [
            Request::get(format!("/api/v1/posts/{}/comments", Uuid::now_v7()))
                .body(Body::empty())
                .unwrap(),
            Request::post("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
            Request::get("/api/v1/studio/collaborators")
                .body(Body::empty())
                .unwrap(),
            Request::post("/api/v1/studio/collaborators")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
            Request::delete(format!("/api/v1/studio/collaborators/{}", Uuid::now_v7()))
                .body(Body::empty())
                .unwrap(),
            Request::get("/agents.txt").body(Body::empty()).unwrap(),
            Request::get("/llms.txt").body(Body::empty()).unwrap(),
        ] {
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let css = router
            .oneshot(Request::get("/custom.css").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(css.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            css.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn semantic_agent_indexes_and_owner_css_are_first_party_cacheable_resources() {
        let root = tempfile::tempdir().unwrap();
        let css_path = root.path().join("owner.css");
        std::fs::write(&css_path, ".article-page { --theme-accent: #c40; }").unwrap();
        let mut state = test_state(None);
        state.custom_css_enabled = true;
        state.custom_css_file = Arc::new(css_path);
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "blog".into(),
            no_index: false,
        });
        let router = app(state);

        let redirect = router
            .clone()
            .oneshot(Request::get("/agent.txt").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(redirect.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            redirect.headers()[header::LOCATION],
            "https://blog.example/base/agents.txt"
        );

        let agents = router
            .clone()
            .oneshot(Request::get("/agents.txt").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(agents.status(), StatusCode::OK);
        assert_eq!(
            agents.headers()[header::CONTENT_TYPE],
            "text/markdown; charset=utf-8"
        );
        assert!(agents.headers().contains_key(header::ETAG));
        assert_eq!(agents.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        let agents_text = text(agents).await;
        assert!(agents_text.contains("not a claim of protocol conformance"));
        assert!(
            agents_text.contains("https://blog.example/base/.well-known/open-soverign-blog.json")
        );

        let llms = router
            .clone()
            .oneshot(Request::get("/llms.txt").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            text(llms)
                .await
                .contains("https://blog.example/base/api/v1/feed")
        );

        let discovery = router
            .clone()
            .oneshot(
                Request::get("/.well-known/open-soverign-blog.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let discovery = json(discovery).await;
        assert_eq!(
            discovery["openapi"],
            "https://blog.example/base/openapi/openapi.yaml"
        );
        let contract = router
            .clone()
            .oneshot(
                Request::get("/openapi/openapi.yaml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let contract = text(contract).await;
        assert!(contract.contains("url: \"https://blog.example/base\""));
        assert!(!contract.contains("url: \"https://blog.example/base/\""));
        assert_eq!(
            discovery["endpoints"]["comments"]["href"],
            "https://blog.example/base/api/v1/posts/{postId}/comments"
        );
        assert_eq!(
            discovery["endpoints"]["commentSubmission"]["methods"],
            serde_json::json!(["POST"])
        );
        assert_eq!(discovery["endpoints"]["runnerProfiles"]["available"], false);
        assert_eq!(
            discovery["endpoints"]["proposeRevision"]["available"],
            false
        );

        let css = router
            .oneshot(Request::get("/custom.css").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(css.status(), StatusCode::OK);
        assert_eq!(
            css.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        assert!(text(css).await.contains("--theme-accent"));
    }

    #[test]
    fn semantic_cache_variant_changes_with_operator_intent() {
        let baseline = test_state(None);
        let baseline_variant = semantic_cache_variant(&baseline);
        let mut noindex = baseline.clone();
        noindex.seo_policy = Arc::new(SeoPolicy {
            public_url: baseline.seo_policy.public_url.clone(),
            article_base_path: baseline.seo_policy.article_base_path.clone(),
            no_index: true,
        });
        assert_ne!(baseline_variant, semantic_cache_variant(&noindex));

        let mut discovery_disabled = baseline.clone();
        discovery_disabled.agent_discovery_enabled = false;
        assert_ne!(
            baseline_variant,
            semantic_cache_variant(&discovery_disabled)
        );
        assert!(!public_cache_path(&baseline, "/custom.css"));
        assert!(public_cache_path(&baseline, "/api/v1/primary/categories"));
        assert!(public_cache_path(
            &baseline,
            "/api/v1/primary/categories/yangja/posts/measurement"
        ));
        assert!(public_cache_path(&baseline, "/yangja"));
        assert!(public_cache_path(&baseline, "/yangja/measurement"));
        assert!(public_cache_path(&baseline, "/yangja/measurement/"));
        assert!(!public_cache_path(&baseline, "/studio/system"));
        assert!(!public_cache_path(&baseline, "/login"));
        assert!(!public_cache_path(&baseline, "/healthz"));
        assert!(!public_cache_path(
            &baseline,
            &format!("/yangja/{}", "x".repeat(721))
        ));
        let mut custom_article_root = baseline.clone();
        custom_article_root.seo_policy = Arc::new(SeoPolicy {
            public_url: baseline.seo_policy.public_url.clone(),
            article_base_path: "writing/articles".into(),
            no_index: false,
        });
        assert!(!primary_category_public_path(
            &custom_article_root,
            "/writing/measurement"
        ));
        assert!(!public_cache_path(
            &custom_article_root,
            "/writing/measurement"
        ));
        assert!(public_cache_path(
            &custom_article_root,
            "/writing/articles/measurement"
        ));
    }

    #[test]
    fn startup_rejects_an_article_root_that_collides_with_a_persisted_category() {
        let state = test_state(None);
        let site = state.repository.ensure_legacy_site(state.site_id).unwrap();
        state
            .repository
            .create_category(
                site.owner_user_id,
                site.id,
                osb_storage_sqlite::CreateCategoryInput {
                    slug: "writing".into(),
                    title: "Writing".into(),
                    description: None,
                    theme_profile: None,
                },
            )
            .unwrap();

        assert!(ensure_article_category_namespace(&state.repository, site.id, "blog").is_ok());
        assert!(ensure_article_category_namespace(&state.repository, site.id, "notes").is_ok());
        let error = ensure_article_category_namespace(&state.repository, site.id, "writing")
            .unwrap_err()
            .to_string();
        let nested_error =
            ensure_article_category_namespace(&state.repository, site.id, "writing/articles")
                .unwrap_err()
                .to_string();
        assert!(error.contains("starts with persisted category 'writing'"));
        assert!(nested_error.contains("starts with persisted category 'writing'"));
    }

    #[tokio::test]
    async fn category_management_is_owner_only_while_collaborators_can_list_and_select() {
        let mut state = test_state(None);
        state.collaboration_enabled = true;
        let repository = Arc::clone(&state.repository);
        let owner = repository
            .create_user(
                "category-owner@example.test",
                "category-owner",
                "Category owner",
                "$argon2id$test-only",
            )
            .unwrap();
        let writer = repository
            .create_user(
                "category-writer@example.test",
                "category-writer",
                "Category writer",
                "$argon2id$test-only",
            )
            .unwrap();
        let editor = repository
            .create_user(
                "category-editor@example.test",
                "category-editor",
                "Category editor",
                "$argon2id$test-only",
            )
            .unwrap();
        let site = repository
            .create_site(
                owner.id,
                "category-security-blog",
                "Category security blog",
                None,
                ThemeProfile::Paper,
            )
            .unwrap();
        state.site_id = site.id;
        repository
            .add_site_collaborator(
                owner.id,
                site.id,
                &writer.email,
                osb_storage_sqlite::SiteMembershipRole::Writer,
            )
            .unwrap();
        repository
            .add_site_collaborator(
                owner.id,
                site.id,
                &editor.email,
                osb_storage_sqlite::SiteMembershipRole::Editor,
            )
            .unwrap();

        let owner_token = [0xc1_u8; 32];
        let writer_token = [0xc2_u8; 32];
        let editor_token = [0xc3_u8; 32];
        for (user_id, token) in [
            (owner.id, owner_token),
            (writer.id, writer_token),
            (editor.id, editor_token),
        ] {
            let token_hash: [u8; 32] = Sha256::digest(token).into();
            repository
                .create_session(
                    user_id,
                    &token_hash,
                    chrono::Utc::now() + chrono::Duration::hours(1),
                )
                .unwrap();
        }
        let owner_cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(owner_token));
        let writer_cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(writer_token));
        let editor_cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(editor_token));
        let router = app(state);

        let created = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/categories")
                    .header(header::COOKIE, &owner_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"slug":"yangja","title":"Yangja","description":"Quantum notes","themePreset":"forest"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let category = json(created).await;
        assert_eq!(category["slug"], "yangja");
        assert_eq!(category["status"], "active");
        let category_id = category["id"].as_str().unwrap().to_owned();

        for (role, cookie) in [("writer", &writer_cookie), ("editor", &editor_cookie)] {
            let list = router
                .clone()
                .oneshot(
                    Request::get("/api/v1/studio/categories")
                        .header(header::COOKIE, cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(list.status(), StatusCode::OK, "{role} can list categories");
            let categories = json(list).await;
            assert_eq!(categories["items"][0]["id"], category_id);

            let forbidden_create = router
                .clone()
                .oneshot(
                    Request::post("/api/v1/studio/categories")
                        .header(header::COOKIE, cookie)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(format!(
                            r#"{{"slug":"{role}-managed","title":"Forbidden"}}"#
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(forbidden_create.status(), StatusCode::FORBIDDEN);
            assert_eq!(json(forbidden_create).await["error"], "forbidden");

            let forbidden_update = router
                .clone()
                .oneshot(
                    Request::put(format!("/api/v1/studio/categories/{category_id}"))
                        .header(header::COOKIE, cookie)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"title":"Forbidden rename"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(forbidden_update.status(), StatusCode::FORBIDDEN);

            let forbidden_archive = router
                .clone()
                .oneshot(
                    Request::post(format!("/api/v1/studio/categories/{category_id}/archive"))
                        .header(header::COOKIE, cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(forbidden_archive.status(), StatusCode::FORBIDDEN);

            let selected = router
                .clone()
                .oneshot(
                    Request::post("/api/v1/studio/documents")
                        .header(header::COOKIE, cookie)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(format!(
                            r#"{{"title":"{role} category draft","slug":"{role}-category-draft","sourceMarkdown":"draft","categoryId":"{category_id}"}}"#
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                selected.status(),
                StatusCode::CREATED,
                "{role} can select an active category"
            );
            assert_eq!(json(selected).await["categoryId"], category_id);
        }

        let archived = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/studio/categories/{category_id}/archive"))
                    .header(header::COOKIE, &owner_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(archived.status(), StatusCode::OK);
        assert_eq!(json(archived).await["status"], "archived");

        let archived_assignment = router
            .oneshot(
                Request::post("/api/v1/studio/documents")
                    .header(header::COOKIE, &writer_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"title":"Archived category draft","slug":"archived-category-draft","sourceMarkdown":"draft","categoryId":"{category_id}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(archived_assignment.status(), StatusCode::BAD_REQUEST);
        let error = json(archived_assignment).await;
        assert_eq!(error["error"], "bad_request");
        assert!(error["message"].as_str().unwrap().contains("archived"));
    }

    #[tokio::test]
    async fn published_category_post_is_resolved_only_through_its_natural_category_path() {
        let mut state = test_state(None);
        let repository = Arc::clone(&state.repository);
        let owner = repository
            .create_user(
                "category-route-owner@example.test",
                "category-route-owner",
                "Category route owner",
                "$argon2id$test-only",
            )
            .unwrap();
        let site = repository
            .create_site(
                owner.id,
                "category-route-blog",
                "Category route blog",
                None,
                ThemeProfile::Paper,
            )
            .unwrap();
        state.site_id = site.id;
        state.custom_css_enabled = true;
        repository
            .change_site_appearance(
                owner.id,
                site.id,
                ThemeProfile::Paper,
                Some(".article-content { color: rebeccapurple; }"),
            )
            .unwrap();
        let owner_token = [0xc4_u8; 32];
        let owner_token_hash: [u8; 32] = Sha256::digest(owner_token).into();
        repository
            .create_session(
                owner.id,
                &owner_token_hash,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap();
        let owner_cookie = format!("osb_session={}", URL_SAFE_NO_PAD.encode(owner_token));
        let router = app(state);

        let category = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/categories")
                    .header(header::COOKIE, &owner_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"slug":"yangja","title":"Yangja","description":"Natural category route"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(category.status(), StatusCode::CREATED);
        let category_id = json(category).await["id"].as_str().unwrap().to_owned();

        let created = router
            .clone()
            .oneshot(
                Request::post("/api/v1/studio/documents")
                    .header(header::COOKIE, &owner_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r##"{{"title":"Quantum computing","slug":"quantum-computing","sourceMarkdown":"# Quantum computing","categoryId":"{category_id}"}}"##
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let document = json(created).await;
        let document_id = document["id"].as_str().unwrap();
        let revision_id = document["currentRevisionId"].as_str().unwrap();

        let published = router
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/studio/documents/{document_id}/publish"))
                    .header(header::COOKIE, &owner_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"revisionId":"{revision_id}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(published.status(), StatusCode::OK);

        let landing = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/category-route-blog/categories/yangja")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(landing.status(), StatusCode::OK);
        let landing = json(landing).await;
        assert_eq!(landing["postCount"], 1);
        assert_eq!(landing["blog"]["isPrimary"], true);
        assert_eq!(
            landing["blog"]["theme"]["customCssUrl"],
            "https://blog.example/api/v1/blogs/category-route-blog/custom.css"
        );

        let category_posts = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/category-route-blog/categories/yangja/posts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(category_posts.status(), StatusCode::OK);
        let category_posts = json(category_posts).await;
        assert_eq!(category_posts["items"][0]["slug"], "quantum-computing");
        assert_eq!(category_posts["items"][0]["category"]["slug"], "yangja");
        assert_eq!(category_posts["items"][0]["blog"]["isPrimary"], true);

        let natural = router
            .clone()
            .oneshot(
                Request::get(
                    "/api/v1/blogs/category-route-blog/categories/yangja/posts/quantum-computing",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(natural.status(), StatusCode::OK);
        let natural_etag = natural.headers()[header::ETAG].clone();
        let natural = json(natural).await;
        assert_eq!(natural["canonicalSlug"], "quantum-computing");
        assert_eq!(natural["requestedSlug"], "quantum-computing");
        assert_eq!(natural["category"]["slug"], "yangja");
        assert_eq!(natural["blog"]["isPrimary"], true);

        let primary_natural = router
            .clone()
            .oneshot(
                Request::get("/api/v1/primary/categories/yangja/posts/quantum-computing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(primary_natural.status(), StatusCode::OK);
        let primary_natural = json(primary_natural).await;
        assert_eq!(primary_natural["category"]["slug"], "yangja");
        assert_eq!(
            primary_natural["blog"]["theme"]["customCssUrl"],
            "https://blog.example/api/v1/blogs/category-route-blog/custom.css"
        );

        let updated_category = router
            .clone()
            .oneshot(
                Request::put(format!("/api/v1/studio/categories/{category_id}"))
                    .header(header::COOKIE, &owner_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"title":"Renamed Yangja","themePreset":"ink"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(updated_category.status(), StatusCode::OK);

        let conditionally_refetched = router
            .clone()
            .oneshot(
                Request::get(
                    "/api/v1/blogs/category-route-blog/categories/yangja/posts/quantum-computing",
                )
                .header(header::IF_NONE_MATCH, natural_etag)
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(conditionally_refetched.status(), StatusCode::OK);
        let conditionally_refetched = json(conditionally_refetched).await;
        assert_eq!(
            conditionally_refetched["category"]["title"],
            "Renamed Yangja"
        );
        assert_eq!(conditionally_refetched["category"]["themePreset"], "ink");

        let flat = router
            .clone()
            .oneshot(
                Request::get("/api/v1/blogs/category-route-blog/posts/quantum-computing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(flat.status(), StatusCode::NOT_FOUND);

        for path in ["/yangja", "/yangja/quantum-computing"] {
            let response = router
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header::ACCEPT, "text/html")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
        for (alias, canonical) in [
            (
                "/@category-route-blog/yangja",
                "https://blog.example/yangja",
            ),
            (
                "/@category-route-blog/yangja/quantum-computing",
                "https://blog.example/yangja/quantum-computing",
            ),
        ] {
            let response = router
                .clone()
                .oneshot(Request::get(alias).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT, "{alias}");
            assert_eq!(response.headers()[header::LOCATION], canonical, "{alias}");
        }

        let sitemap = router
            .oneshot(Request::get("/sitemap.xml").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(sitemap.status(), StatusCode::OK);
        let sitemap = text(sitemap).await;
        assert!(sitemap.contains("<loc>https://blog.example/yangja/quantum-computing</loc>"));
        assert!(
            !sitemap
                .contains("<loc>https://blog.example/@category-route-blog/quantum-computing</loc>")
        );
    }

    #[test]
    fn corrupted_cache_cannot_inject_status_or_security_headers() {
        let mut headers = BTreeMap::new();
        headers.insert(header::CONTENT_TYPE.as_str().into(), "text/plain".into());
        headers.insert(
            header::CONTENT_SECURITY_POLICY.as_str().into(),
            "default-src *".into(),
        );
        headers.insert(header::SET_COOKIE.as_str().into(), "stolen=true".into());
        let signing_key = [0x5a; 32];
        let mut cached = CachedPublicResponse {
            schema_version: "open-soverign-blog-http-cache/4".into(),
            headers,
            body_base64: BASE64_STANDARD.encode("safe"),
            signature: String::new(),
        };
        cached.signature = sign_cached_response(&cached, &signing_key, "route-a", "generation-a");
        let response = cached_response(
            cached.clone(),
            None,
            &signing_key,
            "route-a",
            "generation-a",
        )
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
        assert!(
            !response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        assert!(!response.headers().contains_key(header::SET_COOKIE));

        let mut tampered = cached.clone();
        tampered.body_base64 = BASE64_STANDARD.encode("attacker-controlled");
        assert!(cached_response(tampered, None, &signing_key, "route-a", "generation-a").is_none());
        assert!(
            cached_response(
                cached.clone(),
                None,
                &signing_key,
                "route-b",
                "generation-a"
            )
            .is_none()
        );
        assert!(cached_response(cached, None, &signing_key, "route-a", "generation-b").is_none());
    }

    #[test]
    fn cache_hmac_matches_the_sha256_standard_vector() {
        assert_eq!(
            hmac_sha256(&[0_u8; 32], b"test"),
            [
                0x43, 0xb0, 0xce, 0xf9, 0x92, 0x65, 0xf9, 0xe3, 0x4c, 0x10, 0xea, 0x9d, 0x35, 0x01,
                0x92, 0x6d, 0x27, 0xb3, 0x9f, 0x57, 0xc6, 0xd6, 0x74, 0x56, 0x1d, 0x8b, 0xa2, 0x36,
                0xe7, 0xa8, 0x19, 0xfb,
            ]
        );
    }

    #[tokio::test]
    async fn noindex_remains_crawlable_so_robots_meta_can_be_observed() {
        let mut state = test_state(None);
        state.seo_policy = Arc::new(SeoPolicy {
            public_url: Url::parse("https://blog.example/").unwrap(),
            article_base_path: "blog".into(),
            no_index: true,
        });
        let router = app(state);
        let robots = router
            .clone()
            .oneshot(Request::get("/robots.txt").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = text(robots).await;
        assert!(body.contains("Allow: /"));
        assert!(!body.contains("Disallow: /"));
        assert!(!body.contains("Sitemap:"));
        for path in ["/", "/studio", "/login"] {
            let shell = router
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(
                text(shell)
                    .await
                    .contains("<meta name=\"robots\" content=\"noindex,nofollow\">")
            );
        }
        let sitemap = router
            .oneshot(Request::get("/sitemap.xml").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(sitemap.status(), StatusCode::NOT_FOUND);
    }
}
