use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const CONFIG_SCHEMA: &str = "open-soverign-blog/1";
const INTENT_SCHEMA: &str = "open-soverign-blog-intent/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Personal,
    Community,
    Delivery,
}

impl Intent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Community => "community",
            Self::Delivery => "delivery",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthChoice {
    Local,
    Oauth,
    LocalAndOauth,
    Disabled,
}

impl AuthChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Oauth => "oauth",
            Self::LocalAndOauth => "local_and_oauth",
            Self::Disabled => "disabled",
        }
    }

    fn local_enabled(self) -> bool {
        matches!(self, Self::Local | Self::LocalAndOauth)
    }

    fn oauth_enabled(self) -> bool {
        matches!(self, Self::Oauth | Self::LocalAndOauth)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Toggle {
    Enabled,
    Disabled,
}

impl Toggle {
    fn enabled(self) -> bool {
        self == Self::Enabled
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedisTopologyChoice {
    Standalone,
    Managed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseProfileChoice {
    Durable,
    Balanced,
    Fast,
}

impl DatabaseProfileChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::Durable => "durable",
            Self::Balanced => "balanced",
            Self::Fast => "fast",
        }
    }
}

#[derive(Debug, Args)]
pub struct BootstrapArgs {
    /// New deployment directory. Existing files are never overwritten.
    #[arg(long, default_value = ".")]
    pub directory: PathBuf,
    /// Compose bundle from this source checkout. Defaults to ./compose.yaml.
    #[arg(long)]
    pub compose_file: Option<PathBuf>,
    /// Stable site UUID. Delivery restores must reuse the writable node's value.
    #[arg(long)]
    pub site_id: Option<Uuid>,
    /// Snapshot/cache generation. Delivery restores must use a new value per generation.
    #[arg(long)]
    pub content_release: Option<String>,
    /// Human intent from which safe feature defaults are derived.
    #[arg(long, value_enum, default_value_t = Intent::Personal)]
    pub intent: Intent,
    /// Canonical URL visible to readers and search/AI agents.
    #[arg(long, default_value = "http://localhost:8787")]
    pub public_url: String,
    /// Authentication experience. Defaults to local, or disabled for delivery nodes.
    #[arg(long, value_enum)]
    pub auth: Option<AuthChoice>,
    /// Allow new local accounts. Closed is the safe default.
    #[arg(long, value_enum, default_value_t = Toggle::Disabled)]
    pub registration: Toggle,
    /// Authenticated reader comments. Community intent enables this by default.
    #[arg(long, value_enum)]
    pub comments: Option<Toggle>,
    /// Invited co-authors. The initial owner-only profile remains simpler.
    #[arg(long, value_enum, default_value_t = Toggle::Disabled)]
    pub collaboration: Toggle,
    /// Owner-managed CSS served from this on-premise instance.
    #[arg(long, value_enum, default_value_t = Toggle::Enabled)]
    pub custom_css: Toggle,
    /// Canonical metadata, robots policy, and sitemap.
    #[arg(long, value_enum, default_value_t = Toggle::Enabled)]
    pub seo: Toggle,
    /// Publish semantic agent.txt/agents.txt/llms.txt discovery aliases.
    #[arg(long, value_enum, default_value_t = Toggle::Enabled)]
    pub agent_discovery: Toggle,
    /// Managed uses a Redis primary, replica, and Sentinel discovery.
    #[arg(long, value_enum, default_value_t = RedisTopologyChoice::Managed)]
    pub redis_topology: RedisTopologyChoice,
    /// SQLite durability/latency policy. Durable is recommended on-premise.
    #[arg(long, value_enum, default_value_t = DatabaseProfileChoice::Durable)]
    pub database_profile: DatabaseProfileChoice,
    /// Create verified SQLite/blob backup generations automatically.
    #[arg(long, value_enum, default_value_t = Toggle::Enabled)]
    pub managed_backups: Toggle,
    /// Minutes between managed backup generations.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=10_080))]
    pub backup_interval_minutes: u64,
    /// Number of local generations retained.
    #[arg(long, default_value_t = 96, value_parser = clap::value_parser!(u64).range(2..=10_000))]
    pub backup_retention: u64,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Semantic TOML generated by `osb bootstrap`.
    #[arg(long, default_value = "config.toml")]
    pub config: PathBuf,
    /// Validate files and semantics without contacting Redis.
    #[arg(long)]
    pub offline: bool,
    /// Emit a stable machine-readable report for another agent.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IntentManifest {
    schema_version: &'static str,
    config_schema_version: &'static str,
    intent: Intent,
    public_url: String,
    site_id: String,
    deployment_id: String,
    compose_project: String,
    guarantees: Vec<&'static str>,
    features: ManifestFeatures,
    data: ManifestData,
    operations: ManifestOperations,
    next_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestFeatures {
    auth: AuthChoice,
    registration_open: bool,
    comments: bool,
    collaboration: bool,
    custom_css: bool,
    seo: bool,
    agent_discovery: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestData {
    source_of_truth: &'static str,
    cache: &'static str,
    redis_required: bool,
    redis_topology: RedisTopologyChoice,
    database_profile: DatabaseProfileChoice,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestOperations {
    managed_backups: bool,
    backup_interval_minutes: u64,
    backup_retention: u64,
    delivery_is_read_only: bool,
}

pub fn bootstrap(args: BootstrapArgs) -> Result<()> {
    let public_url = Url::parse(&args.public_url)
        .context("--public-url must be an absolute http:// or https:// URL")?;
    ensure!(
        matches!(public_url.scheme(), "http" | "https")
            && public_url.host_str().is_some()
            && public_url.username().is_empty()
            && public_url.password().is_none()
            && public_url.query().is_none()
            && public_url.fragment().is_none()
            && safe_public_path(public_url.path()),
        "--public-url must be an http(s) origin with a simple URL-safe base path and no credentials, query, or fragment"
    );
    let auth = args.auth.unwrap_or_else(|| {
        if args.intent == Intent::Delivery {
            AuthChoice::Disabled
        } else {
            AuthChoice::Local
        }
    });
    let comments = args
        .comments
        .unwrap_or(if args.intent == Intent::Community {
            Toggle::Enabled
        } else {
            Toggle::Disabled
        })
        .enabled();
    let registration_open = args.registration.enabled();
    let collaboration = args.collaboration.enabled();
    let delivery = args.intent == Intent::Delivery;
    ensure!(
        !delivery || args.site_id.is_some(),
        "delivery intent requires --site-id copied from the writable node's config or handoff"
    );
    ensure!(
        !delivery || args.content_release.is_some(),
        "delivery intent requires --content-release identifying the restored snapshot generation"
    );
    let content_release = args.content_release.as_deref().unwrap_or("live");
    ensure!(
        !content_release.is_empty()
            && content_release.len() <= 128
            && content_release
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte)),
        "--content-release must contain 1-128 ASCII letters, digits, _, ., or -"
    );
    ensure!(
        !delivery || auth == AuthChoice::Disabled,
        "delivery intent requires --auth disabled"
    );
    ensure!(
        !registration_open || auth.local_enabled(),
        "open registration requires local or local-and-oauth authentication"
    );
    ensure!(
        !collaboration || auth != AuthChoice::Disabled,
        "collaboration requires authentication"
    );
    ensure!(
        !delivery || (!comments && !collaboration && !registration_open),
        "a delivery node cannot accept registration, comments, or collaboration writes"
    );

    let compose_file = resolve_compose_file(args.compose_file.as_deref())?;

    fs::create_dir_all(&args.directory).with_context(|| {
        format!(
            "failed to create deployment directory {}",
            args.directory.display()
        )
    })?;
    let deployment_root = args.directory.canonicalize().with_context(|| {
        format!(
            "failed to resolve deployment directory {}",
            args.directory.display()
        )
    })?;
    validate_deployment_path(&deployment_root)?;
    let backup_root = deployment_root.join(".osb-backups");
    let backup_generations = backup_root.join("generations");
    fs::create_dir_all(&backup_generations).with_context(|| {
        format!(
            "failed to create the deployment backup directory {}",
            backup_generations.display()
        )
    })?;
    #[cfg(unix)]
    for path in [&backup_root, &backup_generations] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to protect backup directory {}", path.display()))?;
    }
    let normalized_public_url = public_url.as_str();
    let site_id = args.site_id.unwrap_or_else(Uuid::now_v7);
    // Deployment identity is deliberately independent from content identity.
    // Writable and delivery copies of one site may coexist on the same host.
    let deployment_id = Uuid::now_v7();
    let compose_project = format!("osb-{}", deployment_id.simple());
    let config = render_config(
        &args,
        normalized_public_url,
        site_id,
        content_release,
        auth,
        comments,
        delivery,
    );
    write_new(&args.directory.join("config.toml"), config.as_bytes())?;
    let redis_password = random_hex_secret();
    let cache_signing_key = random_hex_secret();
    let environment = EnvironmentRender {
        auth,
        comments,
        collaboration,
        delivery,
        redis_password: &redis_password,
        cache_signing_key: &cache_signing_key,
        deployment_root: &deployment_root,
        public_url: normalized_public_url,
        compose_project: &compose_project,
    };
    write_new_secret(
        &args.directory.join(".env"),
        render_env(&args, &environment).as_bytes(),
    )?;
    // Compose bind-mounts this path even when the feature is disabled. Keeping a
    // harmless first-party template avoids Docker creating a directory at the
    // file mount while the semantic flag still controls whether it is served.
    write_new(
        &args.directory.join("custom.css"),
        include_bytes!("../../../deploy/custom.css"),
    )?;
    let start_command = compose_command(
        &compose_file,
        &deployment_root,
        &compose_project,
        "up -d --build --wait",
    );
    let doctor_command = compose_command(
        &compose_file,
        &deployment_root,
        &compose_project,
        "exec -T blog osb doctor --config /config/config.toml",
    );
    let next_commands = if delivery {
        vec![
            format!(
                "copy a verified writable-node generation into {}/generations/",
                deployment_root.join(".osb-backups").display()
            ),
            compose_command(
                &compose_file,
                &deployment_root,
                &compose_project,
                "--profile maintenance run --build --rm osb-restore --database /data/open-soverign-blog.db --blob-directory /data/blobs restore /backups/generations/<generation>",
            ),
            start_command.clone(),
            doctor_command,
        ]
    } else {
        vec![
            start_command.clone(),
            doctor_command,
            format!("open {normalized_public_url} and follow the owner onboarding"),
        ]
    };
    let manifest = IntentManifest {
        schema_version: INTENT_SCHEMA,
        config_schema_version: CONFIG_SCHEMA,
        intent: args.intent,
        public_url: normalized_public_url.to_owned(),
        site_id: site_id.to_string(),
        deployment_id: deployment_id.to_string(),
        compose_project: compose_project.clone(),
        guarantees: vec![
            "Markdown remains exportable",
            "SQLite and first-party blobs remain authoritative",
            "Redis is required for the hot path but is never the only copy",
            "Redis cache bodies require an application-only integrity signature",
            "unknown configuration keys fail closed",
            "delivery intent rejects mutations",
        ],
        features: ManifestFeatures {
            auth,
            registration_open,
            comments,
            collaboration,
            custom_css: args.custom_css.enabled(),
            seo: args.seo.enabled(),
            agent_discovery: args.agent_discovery.enabled(),
        },
        data: ManifestData {
            source_of_truth: "sqlite_and_content_addressed_blobs",
            cache: "redis",
            redis_required: true,
            redis_topology: args.redis_topology,
            database_profile: args.database_profile,
        },
        operations: ManifestOperations {
            managed_backups: args.managed_backups.enabled() && !delivery,
            backup_interval_minutes: args.backup_interval_minutes,
            backup_retention: args.backup_retention,
            delivery_is_read_only: delivery,
        },
        next_commands,
    };
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    manifest_bytes.push(b'\n');
    write_new(&args.directory.join("osb.intent.json"), &manifest_bytes)?;

    println!(
        "OpenSoverignBlog bootstrap is ready: {}",
        args.directory.display()
    );
    println!("  intent: {}", args.intent.as_str());
    println!("  Redis: required / {:?}", args.redis_topology);
    println!("  config: {}", args.directory.join("config.toml").display());
    println!(
        "  AI handoff: {}",
        args.directory.join("osb.intent.json").display()
    );
    println!("  Compose project: {compose_project}");
    if delivery {
        println!(
            "Next: restore a verified generation first; exact commands are in {}",
            args.directory.join("osb.intent.json").display()
        );
    } else {
        println!("Next: {start_command}");
    }
    Ok(())
}

fn safe_public_path(path: &str) -> bool {
    let path = path.trim_matches('/');
    path.is_empty()
        || path.split('/').all(|segment| {
            !segment.is_empty()
                && !matches!(segment, "." | "..")
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.~%".contains(&byte))
        })
}

fn validate_deployment_path(path: &Path) -> Result<()> {
    let value = path.to_string_lossy();
    ensure!(
        !value.chars().any(char::is_control) && !value.contains('\''),
        "deployment directory path cannot contain control characters or apostrophes"
    );
    Ok(())
}

fn resolve_compose_file(configured: Option<&Path>) -> Result<PathBuf> {
    let candidate = match configured {
        Some(path) => path.to_owned(),
        None => std::env::current_dir()
            .context("failed to resolve the current directory")?
            .join("compose.yaml"),
    };
    ensure!(
        candidate.is_file(),
        "Compose bundle not found at {}; run bootstrap from the source checkout or pass --compose-file",
        candidate.display()
    );
    candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve Compose bundle {}", candidate.display()))
}

fn compose_command(
    compose_file: &Path,
    deployment_root: &Path,
    project: &str,
    action: &str,
) -> String {
    format!(
        "docker compose -p {project} --env-file {} -f {} {action}",
        shell_quote(&deployment_root.join(".env")),
        shell_quote(compose_file),
    )
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn random_hex_secret() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn render_config(
    args: &BootstrapArgs,
    public_url: &str,
    site_id: Uuid,
    content_release: &str,
    auth: AuthChoice,
    comments: bool,
    delivery: bool,
) -> String {
    let (redis_topology, redis_url, sentinel_urls) = match args.redis_topology {
        RedisTopologyChoice::Standalone => (
            "standalone",
            "redis://redis-primary:6379/",
            "sentinel_urls = []",
        ),
        RedisTopologyChoice::Managed => (
            "sentinel",
            "redis://redis-primary:6379/",
            "sentinel_urls = [\"redis://redis-sentinel-1:26379/\", \"redis://redis-sentinel-2:26379/\", \"redis://redis-sentinel-3:26379/\"]",
        ),
    };
    format!(
        r#"schema_version = "{CONFIG_SCHEMA}"

[semantic]
intent = "{intent}"

[server]
bind = "0.0.0.0:8787"
public_url = "{public_url}"
article_base_path = "blog"
site_id = "{site_id}"
no_index = {no_index}

[storage]
database = "/data/open-soverign-blog.db"
blob_directory = "/data/blobs"
profile = "{database_profile}"

[security]
# Secrets are environment-only. Never put OAuth or owner credentials here.

[community]
auth = "{auth}"
registration_open = {registration_open}
comments = {comments}
collaboration = {collaboration}

[deployment]
delivery_only = {delivery}

[redis]
topology = "{redis_topology}"
url = "{redis_url}"
{sentinel_urls}
sentinel_master = "osb-primary"
namespace = "osb"
content_release = "{content_release}"
required = true
response_ttl_seconds = 60
connect_timeout_ms = 2000

[appearance]
custom_css = {custom_css}
custom_css_file = "/config/custom.css"

[discovery]
agent_txt = {agent_discovery}

[operations]
managed_backups = {managed_backups}
backup_directory = "/backups"
backup_interval_minutes = {backup_interval}
backup_retention = {backup_retention}

[features]
external_auth = {external_auth}
rbac = {collaboration}
comments = {comments}
seo = {seo}
code_runner = false
ads = false
"#,
        intent = args.intent.as_str(),
        public_url = public_url,
        site_id = site_id,
        no_index = !args.seo.enabled(),
        database_profile = args.database_profile.as_str(),
        auth = auth.as_str(),
        registration_open = args.registration.enabled(),
        collaboration = args.collaboration.enabled(),
        custom_css = args.custom_css.enabled(),
        agent_discovery = args.agent_discovery.enabled(),
        managed_backups = args.managed_backups.enabled() && !delivery,
        backup_interval = args.backup_interval_minutes,
        backup_retention = args.backup_retention,
        external_auth = auth.oauth_enabled(),
        seo = args.seo.enabled(),
        content_release = content_release,
    )
}

struct EnvironmentRender<'a> {
    auth: AuthChoice,
    comments: bool,
    collaboration: bool,
    delivery: bool,
    redis_password: &'a str,
    cache_signing_key: &'a str,
    deployment_root: &'a Path,
    public_url: &'a str,
    compose_project: &'a str,
}

fn render_env(args: &BootstrapArgs, environment: &EnvironmentRender<'_>) -> String {
    let features = [
        ("seo", args.seo.enabled()),
        ("comments", environment.comments),
        ("rbac", environment.collaboration),
        ("external_auth", environment.auth.oauth_enabled()),
    ]
    .into_iter()
    .filter_map(|(name, enabled)| enabled.then_some(name))
    .collect::<Vec<_>>()
    .join(",");
    let features = if features.is_empty() {
        "none".to_owned()
    } else {
        features
    };
    format!(
        "COMPOSE_PROJECT_NAME={}\nOSB_CONFIG=/config/config.toml\nOSB_CONFIG_SOURCE='{}'\nOSB_CUSTOM_CSS_SOURCE='{}'\nOSB_PUBLIC_URL='{}'\nOSB_INTENT={}\nOSB_AUTH_MODE={}\nOSB_REGISTRATION_OPEN={}\nOSB_COMMENTS={}\nOSB_COLLABORATION={}\nOSB_CUSTOM_CSS={}\nOSB_AGENT_DISCOVERY={}\nOSB_DELIVERY_ONLY={}\nOSB_FEATURES={}\nOSB_REDIS_REQUIRED=true\nOSB_REDIS_PASSWORD={}\nOSB_CACHE_SIGNING_KEY={}\nOSB_MANAGED_BACKUPS={}\nOSB_BACKUP_VOLUME='{}'\nRUST_LOG=info\n",
        environment.compose_project,
        environment.deployment_root.join("config.toml").display(),
        environment.deployment_root.join("custom.css").display(),
        environment.public_url,
        args.intent.as_str(),
        environment.auth.as_str(),
        args.registration.enabled(),
        environment.comments,
        environment.collaboration,
        args.custom_css.enabled(),
        args.agent_discovery.enabled(),
        environment.delivery,
        features,
        environment.redis_password,
        environment.cache_signing_key,
        args.managed_backups.enabled() && !environment.delivery,
        environment.deployment_root.join(".osb-backups").display(),
    )
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    write_new_with_permissions(path, bytes, false)
}

fn write_new_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    write_new_with_permissions(path, bytes, true)
}

fn write_new_with_permissions(path: &Path, bytes: &[u8], secret: bool) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    if secret {
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = secret;
    let mut file = options
        .open(path)
        .with_context(|| format!("refusing to overwrite existing file {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorConfig {
    schema_version: Option<String>,
    semantic: DoctorSemantic,
    server: DoctorServer,
    storage: DoctorStorage,
    community: DoctorCommunity,
    deployment: DoctorDeployment,
    redis: DoctorRedis,
    appearance: DoctorAppearance,
    discovery: DoctorDiscovery,
    operations: DoctorOperations,
    #[serde(skip)]
    cache_signing_key_present: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorSemantic {
    intent: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorServer {
    public_url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorStorage {
    database: String,
    blob_directory: String,
    profile: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorCommunity {
    auth: String,
    comments: bool,
    collaboration: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorDeployment {
    delivery_only: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorRedis {
    topology: String,
    url: String,
    sentinel_urls: Vec<String>,
    required: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorAppearance {
    custom_css: bool,
    custom_css_file: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorDiscovery {
    agent_txt: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorOperations {
    managed_backups: bool,
    backup_directory: String,
    backup_interval_minutes: u64,
    backup_retention: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorCheck {
    id: &'static str,
    status: CheckStatus,
    summary: String,
    remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

pub fn doctor(args: DoctorArgs) -> Result<()> {
    let source = fs::read_to_string(&args.config)
        .with_context(|| format!("failed to read {}", args.config.display()))?;
    let mut parsed: DoctorConfig = toml::from_str(&source)
        .with_context(|| format!("failed to parse {}", args.config.display()))?;
    apply_environment_overrides(&mut parsed)?;
    let mut checks = Vec::new();
    check_semantics(&parsed, &mut checks);
    check_paths(&args.config, &parsed, &mut checks);
    if args.offline {
        checks.push(DoctorCheck {
            id: "redis.connectivity",
            status: CheckStatus::Warn,
            summary: "offline mode skipped Redis connectivity".into(),
            remediation: Some("rerun without --offline after the stack starts".into()),
        });
    } else {
        check_redis(&parsed.redis, &mut checks);
    }
    let failed = checks.iter().any(|check| check.status == CheckStatus::Fail);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schemaVersion": "open-soverign-blog-doctor/1",
                "ok": !failed,
                "intent": parsed.semantic.intent,
                "checks": checks,
            }))?
        );
    } else {
        println!(
            "OpenSoverignBlog doctor · intent={}",
            parsed.semantic.intent
        );
        for check in &checks {
            let label = match check.status {
                CheckStatus::Pass => "PASS",
                CheckStatus::Warn => "WARN",
                CheckStatus::Fail => "FAIL",
            };
            println!("[{label}] {} · {}", check.id, check.summary);
            if let Some(remediation) = &check.remediation {
                println!("       → {remediation}");
            }
        }
    }
    if failed {
        bail!("doctor found a blocking configuration or runtime problem");
    }
    Ok(())
}

/// Keep `doctor` aligned with the runtime's "non-empty environment wins over
/// TOML" contract. This is especially important inside the Compose container,
/// where credentials and operator-specific mount paths intentionally never
/// live in `config.toml`.
fn apply_environment_overrides(config: &mut DoctorConfig) -> Result<()> {
    apply_environment_overrides_with(config, |name| std::env::var(name).ok())
}

fn apply_environment_overrides_with(
    config: &mut DoctorConfig,
    mut read: impl FnMut(&str) -> Option<String>,
) -> Result<()> {
    let mut value = |name: &str| read(name).filter(|item| !item.trim().is_empty());

    if let Some(item) = value("OSB_INTENT") {
        config.semantic.intent = item.to_ascii_lowercase();
    }
    if let Some(item) = value("OSB_PUBLIC_URL") {
        config.server.public_url = item;
    }
    if let Some(item) = value("OSB_DATABASE") {
        config.storage.database = item;
    }
    if let Some(item) = value("OSB_BLOB_DIRECTORY") {
        config.storage.blob_directory = item;
    }
    if let Some(item) = value("OSB_DATABASE_PROFILE") {
        config.storage.profile = item.to_ascii_lowercase();
    }
    if let Some(item) = value("OSB_AUTH_MODE") {
        config.community.auth = match item.to_ascii_lowercase().as_str() {
            "hybrid" => "local_and_oauth".into(),
            "off" => "disabled".into(),
            _ => item.to_ascii_lowercase(),
        };
    }
    if let Some(item) = value("OSB_COMMENTS") {
        config.community.comments = doctor_bool("OSB_COMMENTS", &item)?;
    }
    if let Some(item) = value("OSB_COLLABORATION") {
        config.community.collaboration = doctor_bool("OSB_COLLABORATION", &item)?;
    }
    if let Some(item) = value("OSB_DELIVERY_ONLY") {
        config.deployment.delivery_only = doctor_bool("OSB_DELIVERY_ONLY", &item)?;
    }
    if let Some(item) = value("OSB_REDIS_TOPOLOGY") {
        config.redis.topology = match item.to_ascii_lowercase().as_str() {
            "managed" => "sentinel".into(),
            _ => item.to_ascii_lowercase(),
        };
    }
    if let Some(item) = value("OSB_REDIS_URL") {
        config.redis.url = item;
    }
    if let Some(item) = value("OSB_REDIS_SENTINELS") {
        config.redis.sentinel_urls = item
            .split(',')
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .map(str::to_owned)
            .collect();
    }
    if let Some(item) = value("OSB_REDIS_REQUIRED") {
        config.redis.required = doctor_bool("OSB_REDIS_REQUIRED", &item)?;
    }
    if let Some(item) = value("OSB_CACHE_SIGNING_KEY") {
        ensure!(
            item.len() == 64 && item.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "OSB_CACHE_SIGNING_KEY must be exactly 64 hexadecimal characters"
        );
        config.cache_signing_key_present = true;
    }
    if let Some(item) = value("OSB_CUSTOM_CSS") {
        config.appearance.custom_css = doctor_bool("OSB_CUSTOM_CSS", &item)?;
    }
    if let Some(item) = value("OSB_CUSTOM_CSS_FILE") {
        config.appearance.custom_css_file = item;
    }
    if let Some(item) = value("OSB_AGENT_DISCOVERY") {
        config.discovery.agent_txt = doctor_bool("OSB_AGENT_DISCOVERY", &item)?;
    }
    if let Some(item) = value("OSB_MANAGED_BACKUPS") {
        config.operations.managed_backups = doctor_bool("OSB_MANAGED_BACKUPS", &item)?;
    }
    if let Some(item) = value("OSB_BACKUP_DIRECTORY") {
        config.operations.backup_directory = item;
    }
    if let Some(item) = value("OSB_BACKUP_INTERVAL_MINUTES") {
        config.operations.backup_interval_minutes = item
            .parse()
            .context("OSB_BACKUP_INTERVAL_MINUTES must be an unsigned integer")?;
    }
    if let Some(item) = value("OSB_BACKUP_RETENTION") {
        config.operations.backup_retention = item
            .parse()
            .context("OSB_BACKUP_RETENTION must be an unsigned integer")?;
    }
    Ok(())
}

fn doctor_bool(name: &str, value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("{name} must be true/false, yes/no, on/off, or 1/0"),
    }
}

fn check_semantics(config: &DoctorConfig, checks: &mut Vec<DoctorCheck>) {
    let schema_ok = config.schema_version.as_deref() == Some(CONFIG_SCHEMA);
    checks.push(DoctorCheck {
        id: "config.schema",
        status: if schema_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: config
            .schema_version
            .as_deref()
            .unwrap_or("missing schema_version")
            .to_owned(),
        remediation: (!schema_ok).then(|| format!("set schema_version = \"{CONFIG_SCHEMA}\"")),
    });
    let intent_ok = matches!(
        config.semantic.intent.as_str(),
        "personal" | "community" | "delivery"
    );
    checks.push(DoctorCheck {
        id: "semantic.intent",
        status: if intent_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: if intent_ok {
            format!("{} intent is explicit", config.semantic.intent)
        } else {
            "intent is missing or unknown".into()
        },
        remediation: (!intent_ok).then(|| "choose personal, community, or delivery".into()),
    });
    let delivery_consistent =
        (config.semantic.intent == "delivery") == config.deployment.delivery_only;
    checks.push(DoctorCheck {
        id: "deployment.mutation_boundary",
        status: if delivery_consistent {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: if delivery_consistent {
            "intent and delivery_only agree".into()
        } else {
            "delivery intent and delivery_only contradict each other".into()
        },
        remediation: (!delivery_consistent).then(|| {
            "set both semantic.intent=delivery and deployment.delivery_only=true, or neither".into()
        }),
    });
    let redis_semantic = config.redis.required
        && matches!(config.redis.topology.as_str(), "standalone" | "sentinel")
        && !config.redis.url.is_empty();
    checks.push(DoctorCheck {
        id: "redis.required_hot_path",
        status: if redis_semantic {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: if redis_semantic {
            format!("Redis is required with {} topology", config.redis.topology)
        } else {
            "Redis must be explicit and required".into()
        },
        remediation: (!redis_semantic)
            .then(|| "set redis.required=true and configure its topology/URL".into()),
    });
    checks.push(DoctorCheck {
        id: "redis.cache_integrity",
        status: if config.cache_signing_key_present {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        summary: if config.cache_signing_key_present {
            "application-only cache response signing is deployment-stable".into()
        } else {
            "cache signing will use a process-local key".into()
        },
        remediation: (!config.cache_signing_key_present).then(|| {
            "set a 64-hex OSB_CACHE_SIGNING_KEY; osb bootstrap generates it automatically".into()
        }),
    });
    let sentinel_ok = config.redis.topology != "sentinel" || config.redis.sentinel_urls.len() >= 3;
    checks.push(DoctorCheck {
        id: "redis.failure_domains",
        status: if sentinel_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        summary: if config.redis.topology == "sentinel" {
            format!(
                "{} Sentinel endpoints declared",
                config.redis.sentinel_urls.len()
            )
        } else {
            "standalone profile has no automatic Redis failover".into()
        },
        remediation: (!sentinel_ok).then(|| "declare three Sentinel endpoints for quorum".into()),
    });
    let auth_ok = matches!(
        config.community.auth.as_str(),
        "local" | "oauth" | "local_and_oauth" | "disabled"
    );
    checks.push(DoctorCheck {
        id: "community.identity",
        status: if auth_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: if auth_ok {
            format!(
                "auth={}, comments={}, collaboration={}",
                config.community.auth, config.community.comments, config.community.collaboration
            )
        } else {
            "authentication intent is unknown".into()
        },
        remediation: (!auth_ok).then(|| "choose local, oauth, local_and_oauth, or disabled".into()),
    });
    checks.push(DoctorCheck {
        id: "discovery.agent_text",
        status: if config.discovery.agent_txt {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        summary: if config.discovery.agent_txt {
            "semantic agent discovery aliases are enabled".into()
        } else {
            "agent discovery is deliberately disabled".into()
        },
        remediation: None,
    });
    let public_url_ok = Url::parse(&config.server.public_url).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && safe_public_path(url.path())
    });
    if !public_url_ok {
        checks.push(DoctorCheck {
            id: "server.public_url",
            status: CheckStatus::Fail,
            summary: "public_url is not a safe canonical http(s) origin/path".into(),
            remediation: Some(
                "remove credentials/query/fragment and use simple URL-safe base segments".into(),
            ),
        });
    } else {
        checks.push(DoctorCheck {
            id: "server.public_url",
            status: CheckStatus::Pass,
            summary: config.server.public_url.clone(),
            remediation: None,
        });
    }
}

fn check_paths(config_path: &Path, config: &DoctorConfig, checks: &mut Vec<DoctorCheck>) {
    let root = config_path.parent().unwrap_or_else(|| Path::new("."));
    let custom_css = deployment_path(root, &config.appearance.custom_css_file);
    if config.appearance.custom_css {
        checks.push(path_check(
            "appearance.custom_css",
            &custom_css,
            custom_css.is_file(),
            "create the configured CSS file or disable owner CSS",
        ));
    }
    let database_parent = deployment_path(root, &config.storage.database)
        .parent()
        .map(Path::to_owned)
        .unwrap_or_else(|| root.to_owned());
    checks.push(DoctorCheck {
        id: "storage.database",
        status: if database_parent.exists() {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        summary: format!(
            "profile={} path={}",
            config.storage.profile, config.storage.database
        ),
        remediation: (!database_parent.exists())
            .then(|| "the container/bootstrap process must create the database parent".into()),
    });
    checks.push(DoctorCheck {
        id: "storage.blobs",
        status: if config.storage.blob_directory.is_empty() {
            CheckStatus::Fail
        } else {
            CheckStatus::Pass
        },
        summary: config.storage.blob_directory.clone(),
        remediation: config
            .storage
            .blob_directory
            .is_empty()
            .then(|| "configure the content-addressed blob directory".into()),
    });
    checks.push(DoctorCheck {
        id: "operations.backups",
        status: if config.operations.managed_backups {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        summary: if config.operations.managed_backups {
            format!(
                "every {} minute(s), retain {} generation(s) at {}",
                config.operations.backup_interval_minutes,
                config.operations.backup_retention,
                config.operations.backup_directory
            )
        } else {
            "managed backups are disabled".into()
        },
        remediation: (!config.operations.managed_backups).then(|| {
            "enable managed backups or document an external verified backup process".into()
        }),
    });
}

fn deployment_path(root: &Path, configured: &str) -> PathBuf {
    let path = Path::new(configured);
    if path.is_absolute() {
        if let Ok(stripped) = path.strip_prefix("/config") {
            return root.join(stripped);
        }
        path.to_owned()
    } else {
        root.join(path)
    }
}

fn path_check(id: &'static str, path: &Path, ok: bool, remediation: &'static str) -> DoctorCheck {
    DoctorCheck {
        id,
        status: if ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: path.display().to_string(),
        remediation: (!ok).then(|| remediation.into()),
    }
}

fn check_redis(config: &DoctorRedis, checks: &mut Vec<DoctorCheck>) {
    let endpoints: Vec<&str> = if config.topology == "sentinel" {
        config.sentinel_urls.iter().map(String::as_str).collect()
    } else {
        vec![config.url.as_str()]
    };
    let mut successes = 0;
    let mut failures = Vec::new();
    for endpoint in &endpoints {
        match redis_ping(endpoint) {
            Ok(()) => successes += 1,
            Err(error) => failures.push(error.to_string()),
        }
    }
    let minimum = if config.topology == "sentinel" { 2 } else { 1 };
    checks.push(DoctorCheck {
        id: "redis.connectivity",
        status: if successes >= minimum {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: format!(
            "{successes}/{} Redis control endpoint(s) answered PING",
            endpoints.len()
        ),
        remediation: (successes < minimum).then(|| {
            format!(
                "start the Redis profile and check DNS/network; {}",
                failures.join("; ")
            )
        }),
    });
}

fn redis_ping(raw_url: &str) -> Result<()> {
    let mut url = Url::parse(raw_url).context("invalid Redis endpoint")?;
    if url.password().is_none()
        && let Ok(password) = std::env::var("OSB_REDIS_PASSWORD")
        && !password.trim().is_empty()
    {
        url.set_password(Some(&password))
            .map_err(|_| anyhow::anyhow!("Redis endpoint cannot accept authentication"))?;
    }
    ensure!(
        url.scheme() == "redis",
        "doctor TCP probe does not terminate rediss:// TLS"
    );
    let host = url.host_str().context("Redis endpoint has no host")?;
    let port = url.port().unwrap_or(6379);
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .context("failed to resolve Redis host")?
        .collect();
    let mut stream = addresses
        .iter()
        .find_map(|address| TcpStream::connect_timeout(address, Duration::from_secs(2)).ok())
        .context("failed to connect to Redis")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    if let Some(password) = url.password() {
        let username = url.username();
        let auth = if username.is_empty() {
            resp(&["AUTH", password])
        } else {
            resp(&["AUTH", username, password])
        };
        stream.write_all(auth.as_bytes())?;
        let response = read_resp_line(&mut stream)?;
        ensure!(response.starts_with("+OK"), "Redis AUTH was rejected");
    }
    stream.write_all(resp(&["PING"]).as_bytes())?;
    let response = read_resp_line(&mut stream)?;
    ensure!(
        response.starts_with("+PONG"),
        "Redis PING returned an unexpected response"
    );
    Ok(())
}

fn resp(parts: &[&str]) -> String {
    let mut output = format!("*{}\r\n", parts.len());
    for part in parts {
        output.push_str(&format!("${}\r\n{}\r\n", part.len(), part));
    }
    output
}

fn read_resp_line(stream: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::with_capacity(64);
    let mut byte = [0_u8; 1];
    while bytes.len() < 4096 {
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n") {
            return String::from_utf8(bytes).context("Redis returned non-UTF-8 control response");
        }
    }
    bail!("Redis response exceeded the doctor probe limit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn personal(directory: PathBuf) -> BootstrapArgs {
        BootstrapArgs {
            directory,
            compose_file: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compose.yaml"),
            ),
            site_id: None,
            content_release: None,
            intent: Intent::Personal,
            public_url: "http://localhost:8787".into(),
            auth: None,
            registration: Toggle::Disabled,
            comments: None,
            collaboration: Toggle::Disabled,
            custom_css: Toggle::Enabled,
            seo: Toggle::Enabled,
            agent_discovery: Toggle::Enabled,
            redis_topology: RedisTopologyChoice::Managed,
            database_profile: DatabaseProfileChoice::Durable,
            managed_backups: Toggle::Enabled,
            backup_interval_minutes: 15,
            backup_retention: 96,
        }
    }

    #[test]
    fn bootstrap_writes_a_semantic_handoff_without_secrets() {
        let root = tempdir().unwrap();
        bootstrap(personal(root.path().to_owned())).unwrap();
        let config = fs::read_to_string(root.path().join("config.toml")).unwrap();
        assert!(config.contains("schema_version = \"open-soverign-blog/1\""));
        assert!(config.contains("required = true"));
        assert!(config.contains("managed_backups = true"));
        assert!(!config.to_ascii_lowercase().contains("password"));
        let environment = fs::read_to_string(root.path().join(".env")).unwrap();
        assert!(environment.contains(&format!(
            "OSB_CONFIG_SOURCE='{}'",
            root.path().join("config.toml").display()
        )));
        assert!(environment.contains(&format!(
            "OSB_BACKUP_VOLUME='{}'",
            root.path().join(".osb-backups").display()
        )));
        let password = environment
            .lines()
            .find_map(|line| line.strip_prefix("OSB_REDIS_PASSWORD="))
            .unwrap();
        assert_eq!(password.len(), 64);
        assert!(password.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let signing_key = environment
            .lines()
            .find_map(|line| line.strip_prefix("OSB_CACHE_SIGNING_KEY="))
            .unwrap();
        assert_eq!(signing_key.len(), 64);
        assert!(signing_key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(signing_key, password);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.path().join(".env"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("osb.intent.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["intent"], "personal");
        assert_eq!(manifest["data"]["redisRequired"], true);
        assert!(
            manifest["nextCommands"][0]
                .as_str()
                .unwrap()
                .contains("--env-file")
        );
    }

    #[test]
    fn bootstrap_never_overwrites_operator_files() {
        let root = tempdir().unwrap();
        bootstrap(personal(root.path().to_owned())).unwrap();
        let error = bootstrap(personal(root.path().to_owned())).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
    }

    #[test]
    fn delivery_intent_rejects_write_features() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.intent = Intent::Delivery;
        args.auth = Some(AuthChoice::Disabled);
        args.comments = Some(Toggle::Enabled);
        assert!(bootstrap(args).is_err());
    }

    #[test]
    fn delivery_handoff_requires_and_preserves_source_identity() {
        let root = tempdir().unwrap();
        let source_site = Uuid::parse_str("018f0000-0000-7000-8000-000000000123").unwrap();
        let mut args = personal(root.path().to_owned());
        args.intent = Intent::Delivery;
        args.auth = Some(AuthChoice::Disabled);
        args.comments = Some(Toggle::Disabled);
        args.custom_css = Toggle::Disabled;
        args.site_id = Some(source_site);
        args.content_release = Some("generation-20260719T120000Z".into());
        bootstrap(args).unwrap();
        let config = fs::read_to_string(root.path().join("config.toml")).unwrap();
        assert!(config.contains(&format!("site_id = \"{source_site}\"")));
        assert!(config.contains("content_release = \"generation-20260719T120000Z\""));
        let handoff: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("osb.intent.json")).unwrap())
                .unwrap();
        assert_eq!(handoff["siteId"], source_site.to_string());
        assert_ne!(handoff["deploymentId"], source_site.to_string());
        assert_ne!(
            handoff["composeProject"],
            format!("osb-{}", source_site.simple())
        );
        assert!(
            handoff["nextCommands"][0]
                .as_str()
                .unwrap()
                .contains("verified writable-node generation")
        );
    }

    #[test]
    fn bootstrap_rejects_ambiguous_or_secret_bearing_public_urls() {
        for public_url in [
            "file:///tmp/blog",
            "https://user:secret@example.test/",
            "https://example.test/?preview=true",
            "https://example.test/#fragment",
            "https://example.test/o'hare",
        ] {
            let root = tempdir().unwrap();
            let mut args = personal(root.path().to_owned());
            args.public_url = public_url.into();
            assert!(bootstrap(args).is_err(), "accepted {public_url}");
            assert!(!root.path().join("config.toml").exists());
        }
    }

    #[test]
    fn doctor_uses_the_same_non_empty_environment_precedence_as_runtime() {
        let mut config: DoctorConfig = toml::from_str(
            r#"
                schema_version = "open-soverign-blog/1"
                [semantic]
                intent = "personal"
                [server]
                public_url = "http://compose.example"
                [storage]
                database = "/data/blog.sqlite3"
                blob_directory = "/data/blobs"
                profile = "durable"
                [community]
                auth = "local"
                [deployment]
                delivery_only = false
                [redis]
                topology = "sentinel"
                url = "redis://redis-primary:6379/"
                sentinel_urls = ["redis://sentinel:26379/"]
                required = true
            "#,
        )
        .unwrap();
        let overrides = std::collections::BTreeMap::from([
            ("OSB_PUBLIC_URL", "http://127.0.0.1:18787/base"),
            ("OSB_DATABASE", "/tmp/osb/blog.sqlite3"),
            ("OSB_BLOB_DIRECTORY", "/tmp/osb/blobs"),
            ("OSB_REDIS_TOPOLOGY", "standalone"),
            ("OSB_REDIS_URL", "redis://127.0.0.1:6389/"),
            ("OSB_REDIS_SENTINELS", ""),
            ("OSB_COMMENTS", "yes"),
            ("OSB_COLLABORATION", "on"),
        ]);
        apply_environment_overrides_with(&mut config, |name| {
            overrides.get(name).map(|value| (*value).to_owned())
        })
        .unwrap();
        assert_eq!(config.server.public_url, "http://127.0.0.1:18787/base");
        assert_eq!(config.storage.database, "/tmp/osb/blog.sqlite3");
        assert_eq!(config.storage.blob_directory, "/tmp/osb/blobs");
        assert_eq!(config.redis.topology, "standalone");
        assert_eq!(config.redis.url, "redis://127.0.0.1:6389/");
        // Empty environment values are ignored, matching RuntimeConfig.
        assert_eq!(config.redis.sentinel_urls, ["redis://sentinel:26379/"]);
        assert!(config.community.comments);
        assert!(config.community.collaboration);
    }
}
