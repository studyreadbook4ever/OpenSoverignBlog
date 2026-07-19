use std::{env, fs, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
};
use osb_feature_code_runner_client::{
    BearerToken, OutputMode, ProfileRegistry, RemoteRunnerConfig, RunLimits, RunnerProfile,
};
use serde::Deserialize;
use url::Url;
use uuid::Uuid;

const DEFAULT_SITE_ID: &str = "00000000-0000-7000-8000-000000000001";
pub(crate) const CONFIG_SCHEMA_VERSION: &str = "open-soverign-blog/2";
const LEGACY_CONFIG_SCHEMA_VERSION: &str = "open-soverign-blog/1";

pub struct RuntimeConfig {
    pub bind: SocketAddr,
    pub public_url: Url,
    pub article_base_path: String,
    pub no_index: bool,
    pub site_id: Uuid,
    pub database: PathBuf,
    pub blob_directory: PathBuf,
    /// Dedicated, static MCP content credential loaded only from the environment.
    pub mcp_token: Option<McpToken>,
    pub admin_auth: AdminAuthSettings,
    pub admin_auth_rotate: bool,
    pub cache_signing_key: Option<[u8; 32]>,
    pub requested_features: String,
    pub registration_open: bool,
    pub deployment_intent: DeploymentIntent,
    pub auth_mode: AuthMode,
    pub comments_enabled: bool,
    pub collaboration_enabled: bool,
    pub custom_css_enabled: bool,
    pub custom_css_file: PathBuf,
    pub agent_discovery_enabled: bool,
    pub delivery_only: bool,
    pub secure_session_cookie: bool,
    /// Optional derivative cache. SQLite and blobs remain the source of truth
    /// when Redis is deliberately disabled by the installation profile.
    pub redis: Option<RedisSettings>,
    pub operations: OperationsSettings,
    pub runner: Option<RunnerSettings>,
}

#[derive(Clone)]
pub struct McpToken(String);

impl McpToken {
    fn parse(value: String) -> Result<Self> {
        if !(32..=128).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || URL_SAFE_NO_PAD.decode(value.as_bytes()).is_err()
        {
            anyhow::bail!(
                "OSB_MCP_TOKEN must contain 32..=128 unpadded Base64url ASCII characters"
            );
        }
        Ok(Self(value))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for McpToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted MCP content token]")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentIntent {
    Personal,
    Community,
    Delivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Local,
    Oauth,
    LocalAndOauth,
    Disabled,
}

/// Authentication for the instance control plane. This is deliberately
/// separate from `AuthMode`, which governs reader/member accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
}

#[derive(Clone)]
pub struct AdminAuthSettings {
    pub mode: AdminAuthMode,
    /// Argon2id PHC decoded from the environment-only Base64 credential.
    pub access_key_phc: Option<String>,
    pub external: Option<ExternalAdminSettings>,
    pub session_days: i64,
}

impl std::fmt::Debug for AdminAuthSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdminAuthSettings")
            .field("mode", &self.mode)
            .field(
                "access_key_phc",
                &self.access_key_phc.as_ref().map(|_| "[redacted]"),
            )
            .field("external", &self.external)
            .field("session_days", &self.session_days)
            .finish()
    }
}

#[derive(Clone)]
pub struct ExternalAdminSettings {
    pub adapter: String,
    pub issuer_url: Url,
    pub client_id: String,
    pub client_secret: Option<String>,
    /// Exact stable OIDC `sub` allowed to become the primary instance owner.
    pub owner_subject: String,
    pub label: String,
}

impl std::fmt::Debug for ExternalAdminSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExternalAdminSettings")
            .field("adapter", &self.adapter)
            .field("issuer_url", &self.issuer_url)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[redacted]"),
            )
            .field("owner_subject", &"[redacted stable subject]")
            .field("label", &self.label)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedisTopology {
    Standalone,
    Sentinel,
}

#[derive(Clone)]
pub struct RedisSettings {
    pub topology: RedisTopology,
    pub url: Url,
    pub sentinel_urls: Vec<Url>,
    pub sentinel_master: String,
    pub namespace: String,
    pub content_release: String,
    pub required: bool,
    pub response_ttl_seconds: u64,
    pub connect_timeout_ms: u64,
}

impl std::fmt::Debug for RedisSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedisSettings")
            .field("topology", &self.topology)
            .field("url", &"[redacted Redis endpoint]")
            .field("sentinel_count", &self.sentinel_urls.len())
            .field("sentinel_master", &self.sentinel_master)
            .field("namespace", &self.namespace)
            .field("content_release", &self.content_release)
            .field("required", &self.required)
            .field("response_ttl_seconds", &self.response_ttl_seconds)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseProfile {
    Durable,
    Balanced,
    Fast,
}

#[derive(Debug, Clone)]
pub struct OperationsSettings {
    pub database_profile: DatabaseProfile,
    pub managed_backups: bool,
    pub backup_directory: PathBuf,
    pub backup_interval_minutes: u64,
    pub backup_retention: usize,
}

pub struct RunnerSettings {
    pub transport: RemoteRunnerConfig,
    pub profiles: ProfileRegistry,
}

impl RuntimeConfig {
    pub fn load() -> Result<Self> {
        let (file, source) = load_file()?;
        let config_schema_version = file.schema_version.clone();
        let bind: SocketAddr = env_value("OSB_BIND")
            .or(file.server.bind)
            .unwrap_or_else(|| "127.0.0.1:8787".into())
            .parse()
            .context("server.bind/OSB_BIND must be a socket address")?;
        let public_url = Url::parse(
            &env_value("OSB_PUBLIC_URL")
                .or(file.server.public_url)
                .unwrap_or_else(|| format!("http://{bind}")),
        )
        .context("server.public_url/OSB_PUBLIC_URL must be an absolute URL")?;
        let article_base_path = env_value("OSB_ARTICLE_BASE_PATH")
            .or(file.server.article_base_path)
            .unwrap_or_else(|| "blog".into());
        validate_article_base_path(&article_base_path)?;
        let no_index = env_value("OSB_NO_INDEX")
            .map(|value| parse_bool("OSB_NO_INDEX", &value))
            .transpose()?
            .or(file.server.no_index)
            .unwrap_or(false);
        let site_id = Uuid::parse_str(
            &env_value("OSB_SITE_ID")
                .or(file.server.site_id)
                .unwrap_or_else(|| DEFAULT_SITE_ID.into()),
        )
        .context("server.site_id/OSB_SITE_ID must be a UUID")?;
        let database = PathBuf::from(
            env_value("OSB_DATABASE")
                .or(file.storage.database)
                .unwrap_or_else(|| ".data/open-soverign-blog.db".into()),
        );
        let blob_directory = PathBuf::from(
            env_value("OSB_BLOB_DIRECTORY")
                .or(file.storage.blob_directory)
                .unwrap_or_else(|| ".data/blobs".into()),
        );
        let legacy_admin_token_present = env_value("OSB_ADMIN_TOKEN")
            .or(file.security.admin_token)
            .is_some();
        validate_legacy_admin_token_policy(
            config_schema_version.as_deref(),
            legacy_admin_token_present,
        )?;
        let mcp_token = env_value("OSB_MCP_TOKEN")
            .map(McpToken::parse)
            .transpose()?;
        let access_key_phc = env_value("OSB_ADMIN_ACCESS_KEY_PHC_B64")
            .map(|encoded| decode_access_key_phc(&encoded))
            .transpose()?;
        let cache_signing_key = env_value("OSB_CACHE_SIGNING_KEY")
            .map(|value| parse_cache_signing_key(&value))
            .transpose()?;
        let mut requested_features = env_value("OSB_FEATURES")
            .map(|value| {
                if value.eq_ignore_ascii_case("none") {
                    String::new()
                } else {
                    value
                }
            })
            .unwrap_or_else(|| {
                file.features
                    .map(|features| features.enabled_csv())
                    .unwrap_or_else(|| "seo".into())
            });
        let registration_open = env_value("OSB_REGISTRATION_OPEN")
            .map(|value| parse_bool("OSB_REGISTRATION_OPEN", &value))
            .transpose()?
            .or(file.community.registration_open)
            .unwrap_or(false);
        let deployment_intent = env_value("OSB_INTENT")
            .map(|value| parse_deployment_intent("OSB_INTENT", &value))
            .transpose()?
            .or(file.semantic.intent)
            .unwrap_or(DeploymentIntent::Personal);
        let auth_mode = env_value("OSB_AUTH_MODE")
            .map(|value| parse_auth_mode("OSB_AUTH_MODE", &value))
            .transpose()?
            .or(file.community.auth)
            .unwrap_or(match deployment_intent {
                DeploymentIntent::Delivery => AuthMode::Disabled,
                DeploymentIntent::Personal => AuthMode::Disabled,
                DeploymentIntent::Community => AuthMode::Local,
            });
        let admin_auth_mode = env_value("OSB_ADMIN_AUTH")
            .map(|value| parse_admin_auth_mode("OSB_ADMIN_AUTH", &value))
            .transpose()?
            .or(file.admin.auth)
            .unwrap_or(if matches!(deployment_intent, DeploymentIntent::Delivery) {
                AdminAuthMode::Disabled
            } else if access_key_phc.is_some() {
                AdminAuthMode::AccessKey
            } else {
                // Source checkouts stay safely public-read-only until bootstrap
                // explicitly materializes an administrator module and secret.
                AdminAuthMode::Disabled
            });
        let admin_auth_rotate = env_bool("OSB_ADMIN_AUTH_ROTATE")?.unwrap_or(false);
        let admin_session_days = env_value("OSB_ADMIN_SESSION_DAYS")
            .map(|value| {
                value
                    .parse::<i64>()
                    .context("OSB_ADMIN_SESSION_DAYS must be an integer")
            })
            .transpose()?
            .or(file.admin.session_days)
            .unwrap_or(30);
        if !(1..=365).contains(&admin_session_days) {
            anyhow::bail!("admin.session_days must be between 1 and 365");
        }
        let external_admin = resolve_external_admin(file.admin.external)?;
        match admin_auth_mode {
            AdminAuthMode::AccessKey => {
                if access_key_phc.is_none() {
                    anyhow::bail!(
                        "admin.auth=\"access_key\" requires OSB_ADMIN_ACCESS_KEY_PHC_B64"
                    );
                }
                if external_admin.is_some() {
                    anyhow::bail!(
                        "admin.external cannot be configured when admin.auth=\"access_key\""
                    );
                }
            }
            AdminAuthMode::External => {
                if access_key_phc.is_some() {
                    anyhow::bail!(
                        "OSB_ADMIN_ACCESS_KEY_PHC_B64 cannot be set when admin.auth=\"external\""
                    );
                }
                if external_admin.is_none() {
                    anyhow::bail!("admin.auth=\"external\" requires [admin.external]");
                }
            }
            AdminAuthMode::Disabled => {
                if access_key_phc.is_some() || external_admin.is_some() {
                    anyhow::bail!(
                        "admin credentials/providers are configured while admin.auth=\"disabled\""
                    );
                }
            }
        }
        if !matches!(admin_auth_mode, AdminAuthMode::Disabled)
            && public_url.scheme() != "https"
            && !is_loopback_url(&public_url)
        {
            anyhow::bail!(
                "remote administrator authentication requires an https public URL; plain HTTP is allowed only on localhost/loopback"
            );
        }
        let admin_auth = AdminAuthSettings {
            mode: admin_auth_mode,
            access_key_phc,
            external: external_admin,
            session_days: admin_session_days,
        };
        let comments_enabled = env_bool("OSB_COMMENTS")?
            .or(file.community.comments)
            .unwrap_or(matches!(deployment_intent, DeploymentIntent::Community));
        let collaboration_enabled = env_bool("OSB_COLLABORATION")?
            .or(file.community.collaboration)
            .unwrap_or(false);
        for (name, enabled) in [
            ("comments", comments_enabled),
            ("rbac", collaboration_enabled),
            (
                "external_auth",
                matches!(auth_mode, AuthMode::Oauth | AuthMode::LocalAndOauth)
                    || matches!(admin_auth.mode, AdminAuthMode::External),
            ),
        ] {
            if enabled {
                requested_features = append_feature(&requested_features, name);
            }
        }
        let custom_css_enabled = env_bool("OSB_CUSTOM_CSS")?
            .or(file.appearance.custom_css)
            .unwrap_or(false);
        let custom_css_file = PathBuf::from(
            env_value("OSB_CUSTOM_CSS_FILE")
                .or(file.appearance.custom_css_file)
                .unwrap_or_else(|| ".data/custom.css".into()),
        );
        let agent_discovery_enabled = env_bool("OSB_AGENT_DISCOVERY")?
            .or(file.discovery.agent_txt)
            .unwrap_or(true);
        let delivery_only = env_value("OSB_DELIVERY_ONLY")
            .map(|value| parse_bool("OSB_DELIVERY_ONLY", &value))
            .transpose()?
            .or(file.deployment.delivery_only)
            .unwrap_or(matches!(deployment_intent, DeploymentIntent::Delivery));
        if delivery_only && deployment_intent != DeploymentIntent::Delivery {
            anyhow::bail!(
                "deployment.delivery_only=true requires semantic.intent=\"delivery\" so operators and agents see one unambiguous mode"
            );
        }
        if matches!(deployment_intent, DeploymentIntent::Delivery) && !delivery_only {
            anyhow::bail!("semantic.intent=\"delivery\" requires deployment.delivery_only=true");
        }
        if delivery_only && auth_mode != AuthMode::Disabled {
            anyhow::bail!("delivery intent requires community.auth=\"disabled\"");
        }
        if delivery_only && admin_auth.mode != AdminAuthMode::Disabled {
            anyhow::bail!("delivery intent requires admin.auth=\"disabled\"");
        }
        if delivery_only && admin_auth_rotate {
            anyhow::bail!(
                "OSB_ADMIN_AUTH_ROTATE cannot run on a delivery-only read-only deployment"
            );
        }
        validate_mcp_token_policy(mcp_token.as_ref(), delivery_only, admin_auth.mode)?;
        if !delivery_only && matches!(auth_mode, AuthMode::Oauth) {
            anyhow::bail!(
                "oauth-only control planes are unavailable until a verified adapter is composed; use local or local_and_oauth"
            );
        }
        // A writable origin may intentionally expose no remote control plane.
        // Public reads continue while writes remain available only to local
        // maintenance jobs and future separately-scoped automation modules.
        if registration_open && !matches!(auth_mode, AuthMode::Local | AuthMode::LocalAndOauth) {
            anyhow::bail!("open registration requires local authentication");
        }
        if !delivery_only
            && (comments_enabled || collaboration_enabled)
            && !matches!(auth_mode, AuthMode::Local | AuthMode::LocalAndOauth)
        {
            anyhow::bail!(
                "comments and collaboration require operational local authentication in this release"
            );
        }
        let redis = RedisSettings::resolve(file.redis)?;
        let operations = OperationsSettings::resolve(file.storage.profile, file.operations)?;
        let secure_session_cookie = public_url.scheme() == "https";
        let runner = file
            .runner
            .map(|runner| runner.into_runtime())
            .transpose()?;

        if let Some(path) = source {
            tracing::info!(path = %path.display(), "loaded configuration file");
        }
        Ok(Self {
            bind,
            public_url,
            article_base_path,
            no_index,
            site_id,
            database,
            blob_directory,
            mcp_token,
            admin_auth,
            admin_auth_rotate,
            cache_signing_key,
            requested_features,
            registration_open,
            deployment_intent,
            auth_mode,
            comments_enabled,
            collaboration_enabled,
            custom_css_enabled,
            custom_css_file,
            agent_discovery_enabled,
            delivery_only,
            secure_session_cookie,
            redis,
            operations,
            runner,
        })
    }
}

fn validate_mcp_token_policy(
    mcp_token: Option<&McpToken>,
    delivery_only: bool,
    admin_auth_mode: AdminAuthMode,
) -> Result<()> {
    if mcp_token.is_none() {
        return Ok(());
    }
    if delivery_only {
        anyhow::bail!("OSB_MCP_TOKEN cannot be enabled on a delivery-only deployment");
    }
    if admin_auth_mode == AdminAuthMode::Disabled {
        anyhow::bail!("OSB_MCP_TOKEN requires an active administrator authentication module");
    }
    Ok(())
}

fn append_feature(current: &str, feature: &str) -> String {
    let mut features = current
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    features.insert(feature);
    features.into_iter().collect::<Vec<_>>().join(",")
}

fn env_bool(name: &str) -> Result<Option<bool>> {
    env_value(name)
        .map(|value| parse_bool(name, &value))
        .transpose()
}

fn parse_deployment_intent(name: &str, value: &str) -> Result<DeploymentIntent> {
    match value.to_ascii_lowercase().as_str() {
        "personal" => Ok(DeploymentIntent::Personal),
        "community" => Ok(DeploymentIntent::Community),
        "delivery" => Ok(DeploymentIntent::Delivery),
        _ => anyhow::bail!("{name} must be personal, community, or delivery"),
    }
}

fn parse_auth_mode(name: &str, value: &str) -> Result<AuthMode> {
    match value.to_ascii_lowercase().as_str() {
        "local" => Ok(AuthMode::Local),
        "oauth" => Ok(AuthMode::Oauth),
        "local_and_oauth" | "hybrid" => Ok(AuthMode::LocalAndOauth),
        "disabled" | "off" => Ok(AuthMode::Disabled),
        _ => anyhow::bail!("{name} must be local, oauth, local_and_oauth, or disabled"),
    }
}

fn parse_admin_auth_mode(name: &str, value: &str) -> Result<AdminAuthMode> {
    match value.to_ascii_lowercase().replace('-', "_").as_str() {
        "access_key" | "key" => Ok(AdminAuthMode::AccessKey),
        "external" | "oauth" | "oidc" => Ok(AdminAuthMode::External),
        "disabled" | "off" | "none" => Ok(AdminAuthMode::Disabled),
        _ => anyhow::bail!("{name} must be access_key, external, or disabled"),
    }
}

fn validate_legacy_admin_token_policy(
    _schema_version: Option<&str>,
    token_present: bool,
) -> Result<()> {
    if !token_present {
        return Ok(());
    }
    anyhow::bail!(
        "security.admin_token/OSB_ADMIN_TOKEN is no longer accepted because it bypasses the selected access_key, external, or disabled administrator authentication module"
    )
}

fn decode_access_key_phc(encoded: &str) -> Result<String> {
    if encoded.len() > 8_192 {
        anyhow::bail!("OSB_ADMIN_ACCESS_KEY_PHC_B64 is too large");
    }
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .context("OSB_ADMIN_ACCESS_KEY_PHC_B64 must be standard Base64")?;
    let phc = String::from_utf8(decoded)
        .context("OSB_ADMIN_ACCESS_KEY_PHC_B64 must decode to UTF-8 PHC text")?;
    if !(32..=4_096).contains(&phc.len())
        || !phc.starts_with("$argon2id$")
        || phc.chars().any(char::is_control)
    {
        anyhow::bail!(
            "OSB_ADMIN_ACCESS_KEY_PHC_B64 must decode to a bounded Argon2id PHC credential"
        );
    }
    Ok(phc)
}

fn is_loopback_url(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("[::1]") | Some("::1")
    )
}

fn env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn load_file() -> Result<(FileConfig, Option<PathBuf>)> {
    let explicit = env_value("OSB_CONFIG").map(PathBuf::from);
    let path = explicit
        .clone()
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    if !path.exists() {
        if explicit.is_some() {
            anyhow::bail!("OSB_CONFIG does not exist: {}", path.display());
        }
        return Ok((FileConfig::default(), None));
    }
    let source = fs::read_to_string(&path)
        .with_context(|| format!("failed to read configuration file {}", path.display()))?;
    let parsed: FileConfig = toml::from_str(&source)
        .with_context(|| format!("invalid configuration file {}", path.display()))?;
    match parsed.schema_version.as_deref() {
        Some(CONFIG_SCHEMA_VERSION) => {}
        Some(LEGACY_CONFIG_SCHEMA_VERSION) => tracing::warn!(
            path = %path.display(),
            "configuration schema v1 is supported for migration; bootstrap a v2 deployment to select modular administrator authentication"
        ),
        Some(other) => {
            anyhow::bail!("unsupported config schema {other:?}; expected {CONFIG_SCHEMA_VERSION:?}")
        }
        None => tracing::warn!(
            path = %path.display(),
            "legacy configuration has no schema_version; run `osb bootstrap` before production deployment"
        ),
    }
    Ok((parsed, Some(path)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    schema_version: Option<String>,
    semantic: SemanticConfig,
    server: ServerConfig,
    storage: StorageConfig,
    security: SecurityConfig,
    admin: AdminConfig,
    community: CommunityConfig,
    deployment: DeploymentConfig,
    redis: RedisFileConfig,
    appearance: AppearanceConfig,
    discovery: DiscoveryConfig,
    operations: OperationsFileConfig,
    features: Option<FeaturesConfig>,
    runner: Option<RunnerFileConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AdminConfig {
    auth: Option<AdminAuthMode>,
    session_days: Option<i64>,
    external: Option<ExternalAdminFileConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExternalAdminFileConfig {
    adapter: Option<String>,
    issuer_url: Option<String>,
    client_id: Option<String>,
    owner_subject: Option<String>,
    label: Option<String>,
}

fn resolve_external_admin(
    file: Option<ExternalAdminFileConfig>,
) -> Result<Option<ExternalAdminSettings>> {
    let requested = file.is_some()
        || [
            "OSB_EXTERNAL_ADAPTER",
            "OSB_EXTERNAL_ISSUER_URL",
            "OSB_EXTERNAL_CLIENT_ID",
            "OSB_EXTERNAL_OWNER_SUBJECT",
            "OSB_EXTERNAL_CLIENT_SECRET",
        ]
        .iter()
        .any(|name| env_value(name).is_some());
    if !requested {
        return Ok(None);
    }
    let file = file.unwrap_or_default();
    let adapter = env_value("OSB_EXTERNAL_ADAPTER")
        .or(file.adapter)
        .unwrap_or_else(|| "oidc".into())
        .to_ascii_lowercase();
    if adapter != "oidc" {
        anyhow::bail!(
            "admin.external.adapter currently supports oidc; Firebase/email adapters plug into the same external identity boundary"
        );
    }
    let issuer_url = Url::parse(
        &env_value("OSB_EXTERNAL_ISSUER_URL")
            .or(file.issuer_url)
            .context("admin.external.issuer_url/OSB_EXTERNAL_ISSUER_URL is required")?,
    )
    .context("admin.external.issuer_url must be an absolute URL")?;
    validate_external_issuer_url(&issuer_url)?;
    let client_id = env_value("OSB_EXTERNAL_CLIENT_ID")
        .or(file.client_id)
        .context("admin.external.client_id/OSB_EXTERNAL_CLIENT_ID is required")?;
    validate_bounded_external_text("admin.external.client_id", &client_id, 512)?;
    let owner_subject = env_value("OSB_EXTERNAL_OWNER_SUBJECT")
        .or(file.owner_subject)
        .context("admin.external.owner_subject/OSB_EXTERNAL_OWNER_SUBJECT is required")?;
    validate_bounded_external_text("admin.external.owner_subject", &owner_subject, 512)?;
    let label = env_value("OSB_EXTERNAL_LABEL")
        .or(file.label)
        .unwrap_or_else(|| "외부 계정으로 계속하기".into());
    validate_bounded_external_text("admin.external.label", &label, 80)?;
    let client_secret = env_value("OSB_EXTERNAL_CLIENT_SECRET");
    if let Some(secret) = &client_secret {
        validate_secret("OSB_EXTERNAL_CLIENT_SECRET", secret)?;
    }
    Ok(Some(ExternalAdminSettings {
        adapter,
        issuer_url,
        client_id,
        client_secret,
        owner_subject,
        label,
    }))
}

fn validate_external_issuer_url(issuer_url: &Url) -> Result<()> {
    let transport_allowed = issuer_url.scheme() == "https"
        || (issuer_url.scheme() == "http" && is_loopback_url(issuer_url));
    if issuer_url.host_str().is_none()
        || !issuer_url.username().is_empty()
        || issuer_url.password().is_some()
        || issuer_url.query().is_some()
        || issuer_url.fragment().is_some()
        || !transport_allowed
    {
        anyhow::bail!(
            "admin.external.issuer_url must be HTTPS (plain HTTP is allowed only for localhost) and have no credentials, query, or fragment"
        );
    }
    Ok(())
}

fn validate_bounded_external_text(name: &str, value: &str, maximum: usize) -> Result<()> {
    if value.trim() != value
        || value.is_empty()
        || value.len() > maximum
        || value.chars().any(char::is_control)
    {
        anyhow::bail!("{name} must be 1-{maximum} trimmed non-control bytes");
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SemanticConfig {
    intent: Option<DeploymentIntent>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ServerConfig {
    bind: Option<String>,
    public_url: Option<String>,
    article_base_path: Option<String>,
    no_index: Option<bool>,
    site_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CommunityConfig {
    registration_open: Option<bool>,
    auth: Option<AuthMode>,
    comments: Option<bool>,
    collaboration: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DeploymentConfig {
    delivery_only: Option<bool>,
}

fn parse_bool(name: &str, value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => anyhow::bail!("{name} must be true/false, yes/no, on/off, or 1/0"),
    }
}

fn validate_secret(name: &str, value: &str) -> Result<()> {
    if !(32..=4096).contains(&value.len()) || value.chars().any(char::is_control) {
        anyhow::bail!("{name} must be 32-4096 non-control bytes");
    }
    Ok(())
}

fn validate_article_base_path(value: &str) -> Result<()> {
    let first_segment = value
        .trim_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    const RESERVED: [&str; 18] = [
        ".well-known",
        "agent.txt",
        "agents.txt",
        "api",
        "assets",
        "custom.css",
        "docs",
        "login",
        "llms.txt",
        "media",
        "onboarding",
        "openapi",
        "providers",
        "robots.txt",
        "schemas",
        "sitemap.xml",
        "studio",
        "vendor",
    ];
    if RESERVED.contains(&first_segment) {
        anyhow::bail!(
            "server.article_base_path/OSB_ARTICLE_BASE_PATH starts with reserved route segment {first_segment}"
        );
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StorageConfig {
    database: Option<String>,
    blob_directory: Option<String>,
    profile: Option<DatabaseProfile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RedisFileConfig {
    enabled: Option<bool>,
    topology: Option<RedisTopology>,
    url: Option<String>,
    password: Option<String>,
    sentinel_urls: Vec<String>,
    sentinel_master: Option<String>,
    namespace: Option<String>,
    content_release: Option<String>,
    required: Option<bool>,
    response_ttl_seconds: Option<u64>,
    connect_timeout_ms: Option<u64>,
}

impl RedisSettings {
    fn resolve(file: RedisFileConfig) -> Result<Option<Self>> {
        let enabled = env_bool("OSB_REDIS_ENABLED")?
            .or(file.enabled)
            .unwrap_or(true);
        if !enabled {
            if env_bool("OSB_REDIS_REQUIRED")?.unwrap_or(false) {
                anyhow::bail!("OSB_REDIS_REQUIRED=true contradicts OSB_REDIS_ENABLED=false");
            }
            return Ok(None);
        }
        let topology = env_value("OSB_REDIS_TOPOLOGY")
            .map(|value| match value.to_ascii_lowercase().as_str() {
                "standalone" => Ok(RedisTopology::Standalone),
                "sentinel" | "managed" => Ok(RedisTopology::Sentinel),
                _ => anyhow::bail!("OSB_REDIS_TOPOLOGY must be standalone or sentinel"),
            })
            .transpose()?
            .or(file.topology)
            .unwrap_or(RedisTopology::Standalone);
        let raw_url = env_value("OSB_REDIS_URL")
            .or(file.url)
            .unwrap_or_else(|| "redis://127.0.0.1:6379/".into());
        let mut url = parse_redis_url("redis.url/OSB_REDIS_URL", &raw_url)?;
        let raw_sentinels = env_value("OSB_REDIS_SENTINELS")
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or(file.sentinel_urls);
        let mut sentinel_urls = raw_sentinels
            .iter()
            .map(|value| parse_redis_url("redis.sentinel_urls/OSB_REDIS_SENTINELS", value))
            .collect::<Result<Vec<_>>>()?;
        if let Some(password) = env_value("OSB_REDIS_PASSWORD").or(file.password) {
            validate_redis_password(&password)?;
            url.set_password(Some(&password))
                .map_err(|_| anyhow::anyhow!("Redis URL cannot accept a password"))?;
            for sentinel_url in &mut sentinel_urls {
                sentinel_url
                    .set_password(Some(&password))
                    .map_err(|_| anyhow::anyhow!("Redis Sentinel URL cannot accept a password"))?;
            }
        }
        let sentinel_master = env_value("OSB_REDIS_SENTINEL_MASTER")
            .or(file.sentinel_master)
            .unwrap_or_else(|| "osb-primary".into());
        let namespace = env_value("OSB_REDIS_NAMESPACE")
            .or(file.namespace)
            .unwrap_or_else(|| "osb".into());
        if namespace.is_empty()
            || namespace.len() > 64
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-:".contains(&byte))
        {
            anyhow::bail!(
                "redis.namespace/OSB_REDIS_NAMESPACE must contain 1-64 ASCII letters, digits, _, -, or :"
            );
        }
        let content_release = env_value("OSB_CONTENT_RELEASE")
            .or(file.content_release)
            .unwrap_or_else(|| "live".into());
        if content_release.is_empty()
            || content_release.len() > 128
            || !content_release
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        {
            anyhow::bail!(
                "redis.content_release/OSB_CONTENT_RELEASE must contain 1-128 ASCII letters, digits, _, ., or -"
            );
        }
        if topology == RedisTopology::Sentinel && sentinel_urls.is_empty() {
            anyhow::bail!("sentinel Redis topology requires at least one sentinel URL");
        }
        if topology == RedisTopology::Sentinel && sentinel_master.trim().is_empty() {
            anyhow::bail!("sentinel Redis topology requires redis.sentinel_master");
        }
        let required = env_bool("OSB_REDIS_REQUIRED")?
            .or(file.required)
            .unwrap_or(true);
        let response_ttl_seconds = env_u64("OSB_REDIS_TTL_SECONDS")?
            .or(file.response_ttl_seconds)
            .unwrap_or(60);
        if !(1..=86_400).contains(&response_ttl_seconds) {
            anyhow::bail!("Redis response TTL must be between 1 and 86400 seconds");
        }
        let connect_timeout_ms = env_u64("OSB_REDIS_CONNECT_TIMEOUT_MS")?
            .or(file.connect_timeout_ms)
            .unwrap_or(2_000);
        if !(100..=60_000).contains(&connect_timeout_ms) {
            anyhow::bail!("Redis connect timeout must be between 100 and 60000 milliseconds");
        }
        Ok(Some(Self {
            topology,
            url,
            sentinel_urls,
            sentinel_master,
            namespace,
            content_release,
            required,
            response_ttl_seconds,
            connect_timeout_ms,
        }))
    }
}

fn validate_redis_password(value: &str) -> Result<()> {
    if !(32..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        anyhow::bail!(
            "redis.password/OSB_REDIS_PASSWORD must be 32-128 URL-safe ASCII letters, digits, _ or -"
        );
    }
    Ok(())
}

fn parse_cache_signing_key(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("OSB_CACHE_SIGNING_KEY must be exactly 64 hexadecimal characters");
    }
    let mut key = [0_u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .context("OSB_CACHE_SIGNING_KEY contains invalid hexadecimal data")?;
    }
    Ok(key)
}

fn parse_redis_url(name: &str, value: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("{name} must be an absolute URL"))?;
    if !matches!(url.scheme(), "redis" | "rediss") || url.host_str().is_none() {
        anyhow::bail!("{name} must use redis:// or rediss:// and include a host");
    }
    Ok(url)
}

fn env_u64(name: &str) -> Result<Option<u64>> {
    env_value(name)
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("{name} must be an unsigned integer"))
        })
        .transpose()
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AppearanceConfig {
    custom_css: Option<bool>,
    custom_css_file: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DiscoveryConfig {
    agent_txt: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OperationsFileConfig {
    managed_backups: Option<bool>,
    backup_directory: Option<String>,
    backup_interval_minutes: Option<u64>,
    backup_retention: Option<usize>,
}

impl OperationsSettings {
    fn resolve(
        database_profile: Option<DatabaseProfile>,
        file: OperationsFileConfig,
    ) -> Result<Self> {
        let database_profile = env_value("OSB_DATABASE_PROFILE")
            .map(|value| match value.to_ascii_lowercase().as_str() {
                "durable" => Ok(DatabaseProfile::Durable),
                "balanced" => Ok(DatabaseProfile::Balanced),
                "fast" => Ok(DatabaseProfile::Fast),
                _ => anyhow::bail!("OSB_DATABASE_PROFILE must be durable, balanced, or fast"),
            })
            .transpose()?
            .or(database_profile)
            .unwrap_or(DatabaseProfile::Durable);
        let managed_backups = env_bool("OSB_MANAGED_BACKUPS")?
            .or(file.managed_backups)
            .unwrap_or(true);
        let backup_directory = PathBuf::from(
            env_value("OSB_BACKUP_DIRECTORY")
                .or(file.backup_directory)
                .unwrap_or_else(|| ".data/backups".into()),
        );
        if backup_directory.file_name().is_none() {
            anyhow::bail!(
                "backup directory cannot be a filesystem root or current-directory alias"
            );
        }
        let backup_interval_minutes = env_u64("OSB_BACKUP_INTERVAL_MINUTES")?
            .or(file.backup_interval_minutes)
            .unwrap_or(15);
        if !(1..=10_080).contains(&backup_interval_minutes) {
            anyhow::bail!("backup interval must be between 1 minute and 7 days");
        }
        let backup_retention = env_value("OSB_BACKUP_RETENTION")
            .map(|value| {
                value
                    .parse::<usize>()
                    .context("OSB_BACKUP_RETENTION must be an unsigned integer")
            })
            .transpose()?
            .or(file.backup_retention)
            .unwrap_or(96);
        if !(2..=10_000).contains(&backup_retention) {
            anyhow::bail!("backup retention must be between 2 and 10000 generations");
        }
        Ok(Self {
            database_profile,
            managed_backups,
            backup_directory,
            backup_interval_minutes,
            backup_retention,
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SecurityConfig {
    admin_token: Option<String>,
}

impl std::fmt::Debug for SecurityConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecurityConfig")
            .field(
                "admin_token",
                &self.admin_token.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FeaturesConfig {
    external_auth: bool,
    rbac: bool,
    comments: bool,
    seo: bool,
    code_runner: bool,
    ads: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerFileConfig {
    endpoint: String,
    #[serde(default = "default_runner_timeout")]
    request_timeout_ms: u64,
    #[serde(default = "default_runner_response_bytes")]
    maximum_response_bytes: usize,
    #[serde(default = "default_runner_ttl")]
    job_ttl_seconds: u64,
    profiles: Vec<RunnerProfileFileConfig>,
}

impl RunnerFileConfig {
    fn into_runtime(self) -> Result<RunnerSettings> {
        let endpoint = env_value("OSB_RUNNER_ENDPOINT").unwrap_or(self.endpoint);
        let mut transport = RemoteRunnerConfig::new(
            Url::parse(&endpoint).context("runner.endpoint must be an absolute URL")?,
        )?
        .with_request_timeout(std::time::Duration::from_millis(self.request_timeout_ms))?
        .with_maximum_response_bytes(self.maximum_response_bytes)?
        .with_job_ttl(std::time::Duration::from_secs(self.job_ttl_seconds))?;
        if let Some(token) = env_value("OSB_RUNNER_TOKEN") {
            transport = transport.with_bearer_token(BearerToken::new(token)?);
        }
        let profiles = self
            .profiles
            .into_iter()
            .map(RunnerProfileFileConfig::into_runtime)
            .collect::<Result<Vec<_>>>()?;
        Ok(RunnerSettings {
            transport,
            profiles: ProfileRegistry::new(profiles)?,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerProfileFileConfig {
    id: String,
    digest: String,
    aliases: Vec<String>,
    output_mode: OutputMode,
    #[serde(default = "default_runner_source_bytes")]
    maximum_source_bytes: usize,
    limits: RunnerLimitsFileConfig,
}

impl RunnerProfileFileConfig {
    fn into_runtime(self) -> Result<RunnerProfile> {
        Ok(RunnerProfile::new(
            self.id,
            self.digest,
            self.aliases,
            self.output_mode,
            self.limits.into_runtime()?,
            self.maximum_source_bytes,
        )?)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerLimitsFileConfig {
    wall_time_ms: u64,
    cpu_time_ms: u64,
    memory_bytes: u64,
    output_bytes: u64,
    process_limit: u32,
}

impl RunnerLimitsFileConfig {
    fn into_runtime(self) -> Result<RunLimits> {
        Ok(RunLimits::new(
            self.wall_time_ms,
            self.cpu_time_ms,
            self.memory_bytes,
            self.output_bytes,
            self.process_limit,
        )?)
    }
}

const fn default_runner_timeout() -> u64 {
    10_000
}

const fn default_runner_response_bytes() -> usize {
    1024 * 1024
}

const fn default_runner_ttl() -> u64 {
    60
}

const fn default_runner_source_bytes() -> usize {
    256 * 1024
}

impl FeaturesConfig {
    fn enabled_csv(&self) -> String {
        let enabled: Vec<_> = [
            ("external_auth", self.external_auth),
            ("rbac", self.rbac),
            ("comments", self.comments),
            ("seo", self.seo),
            ("code_runner", self.code_runner),
            ("ads", self.ads),
        ]
        .into_iter()
        .filter_map(|(name, enabled)| enabled.then_some(name))
        .collect();
        enabled.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_configuration_keys_are_rejected() {
        let error = toml::from_str::<FileConfig>("[security]\nraw_html = true")
            .expect_err("unknown security options must not be silently ignored");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn feature_flags_become_a_deterministic_registry_request() {
        let config: FileConfig = toml::from_str("[features]\nseo = true\ncomments = true").unwrap();
        assert_eq!(config.features.unwrap().enabled_csv(), "comments,seo");
    }

    #[test]
    fn redis_can_be_structurally_disabled() {
        let config: FileConfig = toml::from_str("[redis]\nenabled = false").unwrap();
        assert_eq!(config.redis.enabled, Some(false));
    }

    #[test]
    fn checked_in_example_is_accepted_by_the_runtime_parser() {
        toml::from_str::<FileConfig>(include_str!("../../../config.example.toml"))
            .expect("config.example.toml must not drift from RuntimeConfig");
    }

    #[test]
    fn bootstrap_secret_rejects_short_or_control_values() {
        assert!(validate_secret("token", "too-short").is_err());
        assert!(validate_secret("token", &"x".repeat(32)).is_ok());
        assert!(validate_secret("token", &format!("{}\n", "x".repeat(32))).is_err());
    }

    #[test]
    fn cache_signing_key_is_fixed_width_hex() {
        assert_eq!(
            parse_cache_signing_key(&"ab".repeat(32)).unwrap(),
            [0xab; 32]
        );
        assert!(parse_cache_signing_key(&"ab".repeat(31)).is_err());
        assert!(parse_cache_signing_key(&format!("{}zz", "ab".repeat(31))).is_err());
    }

    #[test]
    fn community_and_delivery_profiles_parse_from_config() {
        let config: FileConfig = toml::from_str(
            "[community]\nregistration_open = true\n[deployment]\ndelivery_only = true",
        )
        .unwrap();
        assert_eq!(config.community.registration_open, Some(true));
        assert_eq!(config.deployment.delivery_only, Some(true));
    }

    #[test]
    fn administrator_auth_is_an_independent_configuration_axis() {
        let config: FileConfig = toml::from_str(
            r#"
                schema_version = "open-soverign-blog/2"
                [admin]
                auth = "external"
                session_days = 45
                [admin.external]
                adapter = "oidc"
                issuer_url = "https://identity.example/realm/blog"
                client_id = "blog"
                owner_subject = "stable-owner-subject"
            "#,
        )
        .unwrap();
        assert_eq!(config.admin.auth, Some(AdminAuthMode::External));
        assert_eq!(config.admin.session_days, Some(45));
        assert!(config.admin.external.is_some());
        assert_eq!(config.community.auth, None);
    }

    #[test]
    fn administrator_access_key_environment_contains_only_a_phc() {
        let phc = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA";
        let encoded = BASE64_STANDARD.encode(phc);
        assert_eq!(decode_access_key_phc(&encoded).unwrap(), phc);
        assert!(decode_access_key_phc("plain-text-key").is_err());
        assert!(decode_access_key_phc(&BASE64_STANDARD.encode("not-a-phc")).is_err());
    }

    #[test]
    fn mcp_token_is_bounded_unpadded_base64url_and_redacted() {
        let minimum = URL_SAFE_NO_PAD.encode([0x5a; 24]);
        let maximum = URL_SAFE_NO_PAD.encode([0x5a; 96]);
        assert_eq!(minimum.len(), 32);
        assert_eq!(maximum.len(), 128);
        let token = McpToken::parse(minimum.clone()).unwrap();
        assert_eq!(token.as_bytes(), minimum.as_bytes());
        assert!(!format!("{token:?}").contains(&minimum));
        assert!(McpToken::parse(maximum).is_ok());
        assert!(McpToken::parse(URL_SAFE_NO_PAD.encode([0x5a; 23])).is_err());
        assert!(McpToken::parse(format!("{minimum}=")).is_err());
        assert!(McpToken::parse("@".repeat(32)).is_err());
        assert!(McpToken::parse("A".repeat(129)).is_err());
    }

    #[test]
    fn mcp_token_requires_a_writable_active_admin_module() {
        let token = McpToken::parse(URL_SAFE_NO_PAD.encode([0x6b; 32])).unwrap();
        assert!(validate_mcp_token_policy(Some(&token), false, AdminAuthMode::AccessKey).is_ok());
        assert!(validate_mcp_token_policy(Some(&token), false, AdminAuthMode::External).is_ok());
        assert!(validate_mcp_token_policy(Some(&token), false, AdminAuthMode::Disabled).is_err());
        assert!(validate_mcp_token_policy(Some(&token), true, AdminAuthMode::AccessKey).is_err());
    }

    #[test]
    fn every_schema_rejects_the_legacy_owner_token_bypass() {
        assert!(validate_legacy_admin_token_policy(Some(CONFIG_SCHEMA_VERSION), true).is_err());
        assert!(
            validate_legacy_admin_token_policy(Some(LEGACY_CONFIG_SCHEMA_VERSION), true).is_err()
        );
        assert!(validate_legacy_admin_token_policy(None, true).is_err());
        assert!(validate_legacy_admin_token_policy(Some(CONFIG_SCHEMA_VERSION), false).is_ok());
    }

    #[test]
    fn external_issuer_rejects_embedded_credentials() {
        let credentialed = Url::parse("https://user:secret@identity.example/realm/blog").unwrap();
        assert!(validate_external_issuer_url(&credentialed).is_err());
        let non_http_loopback = Url::parse("ftp://localhost/realm/blog").unwrap();
        assert!(validate_external_issuer_url(&non_http_loopback).is_err());
        let local_development = Url::parse("http://127.0.0.1:9000/realm/blog").unwrap();
        assert!(validate_external_issuer_url(&local_development).is_ok());
        let ordinary = Url::parse("https://identity.example/realm/blog").unwrap();
        assert!(validate_external_issuer_url(&ordinary).is_ok());
    }

    #[test]
    fn article_routes_cannot_overlap_reserved_server_routes() {
        assert!(validate_article_base_path("blog").is_ok());
        assert!(validate_article_base_path("writing/articles").is_ok());
        assert!(validate_article_base_path("api/v1/posts").is_err());
        assert!(validate_article_base_path("studio/posts").is_err());
    }
}
