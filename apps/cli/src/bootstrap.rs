use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, BufRead, IsTerminal, Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use argon2::{
    Argon2,
    password_hash::{PasswordHasher, SaltString},
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
};
use clap::{Args, Subcommand, ValueEnum};
use osb_plugin_api::{
    DlcHistoryAction, DlcHistoryRecord, INSTALL_INTENT_SCHEMA_VERSION, INSTALL_LOCK_SCHEMA_VERSION,
    InstallationAdminAuth, InstallationCache, InstallationIntent, InstallationLock,
    InstallationSelection, InstallationStyle, InstallationStyleKind, InstalledDlc,
    InstalledDlcSourceKind, LockedEngine, PLUGIN_API_VERSION, PluginManifest, RequestedDlc,
};
use osb_storage_sqlite::DATABASE_SCHEMA_VERSION;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

mod dlc_lifecycle;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const CONFIG_SCHEMA: &str = "open-soverign-blog/2";
const INTENT_SCHEMA: &str = "open-soverign-blog-intent/1";
const INSTALL_MANIFEST: &str = "osb.install.toml";
const INSTALL_LOCK: &str = "osb.lock.json";
const REFERENCES_FILE: &str = "references.md";
const REFERENCES_MAX_BYTES: u64 = 1024 * 1024;
const GITIGNORE_LIMIT: u64 = 256 * 1024;
const GENERATED_GITIGNORE: &str = ".env\nadmin-access-key.txt\n.osb-backups/\n.osb-update/\n";
const REQUIRED_SECRET_IGNORES: [&str; 4] = [
    ".env",
    "admin-access-key.txt",
    ".osb-backups/",
    ".osb-update/",
];
const RECOMMENDED_PERSONAL_DLCS: [&str; 5] = [
    "seo",
    "home-curation",
    "ai-authorship",
    "social-embeds",
    "release-check",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LanguageChoice {
    #[default]
    Ko,
    En,
}

impl LanguageChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ko => "ko",
            Self::En => "en",
        }
    }

    fn external_label(self) -> &'static str {
        match self {
            Self::Ko => "외부 계정으로 계속하기",
            Self::En => "Continue with external account",
        }
    }

    fn references_label(self) -> &'static str {
        match self {
            Self::Ko => "레퍼런스",
            Self::En => "References",
        }
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminAuthChoice {
    AccessKey,
    External,
    Disabled,
}

impl AdminAuthChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::AccessKey => "access_key",
            Self::External => "external",
            Self::Disabled => "disabled",
        }
    }
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
pub enum CacheChoice {
    None,
    RedisStandalone,
    RedisManaged,
}

impl CacheChoice {
    fn installation(self) -> InstallationCache {
        match self {
            Self::None => InstallationCache::None,
            Self::RedisStandalone => InstallationCache::RedisStandalone,
            Self::RedisManaged => InstallationCache::RedisManaged,
        }
    }

    fn redis_topology(self) -> Option<RedisTopologyChoice> {
        match self {
            Self::None => None,
            Self::RedisStandalone => Some(RedisTopologyChoice::Standalone),
            Self::RedisManaged => Some(RedisTopologyChoice::Managed),
        }
    }

    fn compose_profile(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::RedisStandalone => Some("redis-standalone"),
            Self::RedisManaged => Some("redis-managed"),
        }
    }
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

#[derive(Debug, Clone, Args)]
pub struct BootstrapArgs {
    /// New deployment directory. Existing files are never overwritten.
    #[arg(long, default_value = ".")]
    pub directory: PathBuf,
    /// Never prompt; unspecified structural choices retain compatibility defaults.
    #[arg(long)]
    pub non_interactive: bool,
    /// Human-facing language for prompts, bundled UI, and generated starter content.
    #[arg(long, value_enum, value_name = "ko|en")]
    pub language: Option<LanguageChoice>,
    /// Compose bundle from this source checkout. Defaults to ./compose.yaml.
    #[arg(long)]
    pub compose_file: Option<PathBuf>,
    /// Stable Compose project name. Omit to generate an isolated osb-UUID name.
    #[arg(long, value_name = "NAME")]
    pub compose_project: Option<String>,
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
    /// Reader/member authentication. OAuth-only is reserved and rejected until an adapter ships.
    #[arg(long, value_enum)]
    pub auth: Option<AuthChoice>,
    /// Administrator control plane: one-time access-key exchange, external OIDC, or no remote admin.
    #[arg(long, value_enum)]
    pub admin_auth: Option<AdminAuthChoice>,
    /// OIDC issuer URL. Required with --admin-auth external.
    #[arg(long)]
    pub external_issuer_url: Option<String>,
    /// OIDC client identifier. Required with --admin-auth external.
    #[arg(long)]
    pub external_client_id: Option<String>,
    /// Exact stable OIDC subject (`sub`) allowed to administer this instance.
    #[arg(long)]
    pub external_owner_subject: Option<String>,
    /// Human-facing label for the external login button.
    #[arg(long)]
    pub external_label: Option<String>,
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
    #[arg(long, value_enum)]
    pub custom_css: Option<Toggle>,
    /// Stable built-in style id (`builtin:name`) or `none`.
    #[arg(long, value_name = "STYLE")]
    pub style: Option<String>,
    /// Install this regular CSS file as the selected custom style.
    #[arg(long, value_name = "FILE", conflicts_with = "style")]
    pub css_file: Option<PathBuf>,
    /// Install this UTF-8 Markdown file as the instance-wide references policy.
    #[arg(long, value_name = "FILE")]
    pub references_file: Option<PathBuf>,
    /// Human-facing navigation label for the instance-wide references policy.
    #[arg(long)]
    pub references_label: Option<String>,
    /// Canonical metadata, robots policy, and sitemap.
    #[arg(long, value_enum, default_value_t = Toggle::Enabled)]
    pub seo: Toggle,
    /// Publish semantic agent.txt/agents.txt/llms.txt discovery aliases.
    #[arg(long, value_enum, default_value_t = Toggle::Enabled)]
    pub agent_discovery: Toggle,
    /// Managed uses a Redis primary, replica, and Sentinel discovery.
    #[arg(long, value_enum)]
    pub redis_topology: Option<RedisTopologyChoice>,
    /// Cache module: disabled, direct Redis, or managed Redis/Sentinel.
    #[arg(long, value_enum)]
    pub cache: Option<CacheChoice>,
    /// Add an official DLC by alias or reverse-domain id; optionally append @SEMVER_REQ.
    #[arg(long = "dlc", value_name = "ID[@VERSION]")]
    pub dlcs: Vec<String>,
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
    /// Long-lived installation intent; defaults beside config.toml.
    #[arg(long, env = "OSB_INSTALL_MANIFEST")]
    pub install_manifest: Option<PathBuf>,
    /// Exact installation lock; defaults beside config.toml.
    #[arg(long, env = "OSB_INSTALL_LOCK")]
    pub install_lock: Option<PathBuf>,
    /// Generated deployment environment; defaults to a sibling .env when present.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
    /// Validate files and semantics without contacting Redis.
    #[arg(long)]
    pub offline: bool,
    /// Emit a stable machine-readable report for another agent.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct InstallationArgs {
    #[command(subcommand)]
    action: InstallationAction,
}

#[derive(Debug, Subcommand)]
enum InstallationAction {
    /// Verify intent, exact lock, canonical digest, and their shared selection.
    Verify {
        #[arg(long, default_value = INSTALL_MANIFEST)]
        intent: PathBuf,
        #[arg(long, default_value = INSTALL_LOCK)]
        lock: PathBuf,
    },
    /// Atomically record an updater-approved engine transition in the lock.
    RecordEngineUpgrade {
        #[arg(long, default_value = INSTALL_MANIFEST)]
        intent: PathBuf,
        #[arg(long, default_value = INSTALL_LOCK)]
        lock: PathBuf,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        artifact_sha256: Option<String>,
    },
    /// Adopt a pre-lock deployment without changing config, secrets, or CSS.
    Adopt {
        #[arg(long, default_value = ".")]
        directory: PathBuf,
    },
    /// Maintain bundled official DLC intent, exact locks, and runtime composition.
    Dlc(dlc_lifecycle::DlcArgs),
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
    installation_manifest: &'static str,
    installation_lock: &'static str,
    references_source: ReferencesSourceContract,
    guarantees: Vec<&'static str>,
    features: ManifestFeatures,
    data: ManifestData,
    operations: ManifestOperations,
    next_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestFeatures {
    member_auth: AuthChoice,
    admin_auth: AdminAuthChoice,
    registration_open: bool,
    comments: bool,
    collaboration: bool,
    custom_css: bool,
    agent_discovery: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestData {
    source_of_truth: &'static str,
    cache: &'static str,
    redis_required: bool,
    redis_topology: Option<RedisTopologyChoice>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReferencesSourceContract {
    path: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct ResolvedStyle {
    installation: InstallationStyle,
    css_bytes: Option<Vec<u8>>,
    environment_value: String,
}

#[derive(Debug, Clone)]
struct ResolvedReferences {
    bytes: Vec<u8>,
    contract: ReferencesSourceContract,
}

#[derive(Debug, Clone)]
struct ResolvedDlc {
    requested: RequestedDlc,
    installed: InstalledDlc,
    runtime_feature: &'static str,
}

#[derive(Clone, Copy)]
struct OfficialDlc {
    alias: &'static str,
    id: &'static str,
    runtime_feature: &'static str,
    source: &'static str,
    manifest: &'static str,
}

const OFFICIAL_DLCS: [OfficialDlc; 11] = [
    OfficialDlc {
        alias: "ads",
        id: "org.open-soverign-blog.monetization-policy",
        runtime_feature: "ads",
        source: "plugins/official/ads/plugin.toml",
        manifest: include_str!("../../../plugins/official/ads/plugin.toml"),
    },
    OfficialDlc {
        alias: "ai-authorship",
        id: "org.open-soverign-blog.ai-authorship",
        runtime_feature: "ai_authorship",
        source: "plugins/official/ai-authorship/plugin.toml",
        manifest: include_str!("../../../plugins/official/ai-authorship/plugin.toml"),
    },
    OfficialDlc {
        alias: "ai-summary",
        id: "org.open-soverign-blog.ai-summary",
        runtime_feature: "ai_summary",
        source: "plugins/official/ai-summary/plugin.toml",
        manifest: include_str!("../../../plugins/official/ai-summary/plugin.toml"),
    },
    OfficialDlc {
        alias: "code-runner",
        id: "org.open-soverign-blog.code-runner-client",
        runtime_feature: "code_runner",
        source: "plugins/official/code-runner/plugin.toml",
        manifest: include_str!("../../../plugins/official/code-runner/plugin.toml"),
    },
    OfficialDlc {
        alias: "comments",
        id: "org.open-soverign-blog.comments",
        runtime_feature: "comments",
        source: "plugins/official/comments/plugin.toml",
        manifest: include_str!("../../../plugins/official/comments/plugin.toml"),
    },
    OfficialDlc {
        alias: "external-auth",
        id: "org.open-soverign-blog.external-auth",
        runtime_feature: "external_auth",
        source: "plugins/official/external-auth/plugin.toml",
        manifest: include_str!("../../../plugins/official/external-auth/plugin.toml"),
    },
    OfficialDlc {
        alias: "home-curation",
        id: "org.open-soverign-blog.home-curation",
        runtime_feature: "home_curation",
        source: "plugins/official/home-curation/plugin.toml",
        manifest: include_str!("../../../plugins/official/home-curation/plugin.toml"),
    },
    OfficialDlc {
        alias: "rbac",
        id: "org.open-soverign-blog.rbac",
        runtime_feature: "rbac",
        source: "plugins/official/rbac/plugin.toml",
        manifest: include_str!("../../../plugins/official/rbac/plugin.toml"),
    },
    OfficialDlc {
        alias: "release-check",
        id: "org.open-soverign-blog.release-check",
        runtime_feature: "release_check",
        source: "plugins/official/release-check/plugin.toml",
        manifest: include_str!("../../../plugins/official/release-check/plugin.toml"),
    },
    OfficialDlc {
        alias: "seo",
        id: "org.open-soverign-blog.seo",
        runtime_feature: "seo",
        source: "plugins/official/seo/plugin.toml",
        manifest: include_str!("../../../plugins/official/seo/plugin.toml"),
    },
    OfficialDlc {
        alias: "social-embeds",
        id: "org.open-soverign-blog.social-embeds",
        runtime_feature: "social_embeds",
        source: "plugins/official/social-embeds/plugin.toml",
        manifest: include_str!("../../../plugins/official/social-embeds/plugin.toml"),
    },
];

fn resolve_prompted_args(mut args: BootstrapArgs) -> Result<BootstrapArgs> {
    let interactive =
        !args.non_interactive && io::stdin().is_terminal() && io::stdout().is_terminal();
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    resolve_prompted_args_with(&mut args, interactive, &mut reader, &mut writer)?;
    Ok(args)
}

fn resolve_prompted_args_with(
    args: &mut BootstrapArgs,
    interactive: bool,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<()> {
    if args.language.is_none() {
        args.language = Some(if interactive {
            let answer = prompt(
                reader,
                writer,
                "Language / 언어 (ko=한국어, en=English)",
                "ko",
            )?;
            match answer.trim().to_ascii_lowercase().as_str() {
                "ko" => LanguageChoice::Ko,
                "en" => LanguageChoice::En,
                _ => bail!("language must be ko or en / 언어는 ko 또는 en이어야 합니다"),
            }
        } else {
            LanguageChoice::Ko
        });
    }
    let language = args.language.unwrap_or_default();
    args.external_label
        .get_or_insert_with(|| language.external_label().to_owned());
    args.references_label
        .get_or_insert_with(|| language.references_label().to_owned());

    if !interactive {
        return Ok(());
    }

    if args.admin_auth.is_none() {
        let default = if args.intent == Intent::Delivery {
            "disabled"
        } else {
            "access-key"
        };
        let answer = prompt(
            reader,
            writer,
            match language {
                LanguageChoice::Ko => "관리자 인증 (access-key, external, disabled)",
                LanguageChoice::En => "Administrator auth (access-key, external, disabled)",
            },
            default,
        )?;
        args.admin_auth = Some(
            match answer.to_ascii_lowercase().replace('_', "-").as_str() {
                "access-key" | "key" => AdminAuthChoice::AccessKey,
                "external" | "oauth" | "oidc" => AdminAuthChoice::External,
                "disabled" | "none" | "off" => AdminAuthChoice::Disabled,
                _ => bail!("administrator auth must be access-key, external, or disabled"),
            },
        );
    }
    if args.admin_auth == Some(AdminAuthChoice::External) {
        if args.external_issuer_url.is_none() {
            args.external_issuer_url = Some(prompt(
                reader,
                writer,
                match language {
                    LanguageChoice::Ko => "OIDC 발급자 URL",
                    LanguageChoice::En => "OIDC issuer URL",
                },
                "https://identity.example",
            )?);
        }
        if args.external_client_id.is_none() {
            args.external_client_id = Some(prompt(
                reader,
                writer,
                match language {
                    LanguageChoice::Ko => "OIDC 클라이언트 ID",
                    LanguageChoice::En => "OIDC client id",
                },
                "open-soverign-blog",
            )?);
        }
        if args.external_owner_subject.is_none() {
            args.external_owner_subject = Some(prompt(
                reader,
                writer,
                match language {
                    LanguageChoice::Ko => "관리자 OIDC 주체(sub)의 정확한 값",
                    LanguageChoice::En => "Exact administrator OIDC subject (sub)",
                },
                "owner-subject",
            )?);
        }
    }

    if args.style.is_none() && args.css_file.is_none() && args.custom_css.is_none() {
        let answer = prompt(
            reader,
            writer,
            match language {
                LanguageChoice::Ko => "스타일 (none, builtin:STYLE, file:/path/to/style.css)",
                LanguageChoice::En => "Style (none, builtin:STYLE, file:/path/to/style.css)",
            },
            "builtin:paper",
        )?;
        if let Some(path) = answer.strip_prefix("file:") {
            ensure!(!path.is_empty(), "file: style requires a path");
            args.css_file = Some(PathBuf::from(path));
        } else {
            args.style = Some(answer);
        }
    }

    if args.cache.is_none() && args.redis_topology.is_none() {
        let answer = prompt(
            reader,
            writer,
            match language {
                LanguageChoice::Ko => "캐시 (none, redis-standalone, redis-managed)",
                LanguageChoice::En => "Cache (none, redis-standalone, redis-managed)",
            },
            "redis-managed",
        )?;
        args.cache = Some(
            match answer.to_ascii_lowercase().replace('_', "-").as_str() {
                "none" | "off" | "disabled" => CacheChoice::None,
                "redis-standalone" | "standalone" => CacheChoice::RedisStandalone,
                "redis-managed" | "managed" | "sentinel" => CacheChoice::RedisManaged,
                _ => bail!("cache must be none, redis-standalone, or redis-managed"),
            },
        );
    }

    if args.dlcs.is_empty() {
        let default_dlcs = if args.seo.enabled() {
            "seo,home-curation,ai-authorship,social-embeds,release-check"
        } else {
            "home-curation,ai-authorship,social-embeds,release-check"
        };
        let answer = prompt(
            reader,
            writer,
            match language {
                LanguageChoice::Ko => {
                    "선택 DLC 별칭, 쉼표로 구분 (seo, home-curation, ai-authorship, ai-summary, social-embeds, release-check, comments, rbac, external-auth, code-runner, ads; 또는 none)"
                }
                LanguageChoice::En => {
                    "Optional DLC aliases, comma-separated (seo, home-curation, ai-authorship, ai-summary, social-embeds, release-check, comments, rbac, external-auth, code-runner, ads; or none)"
                }
            },
            default_dlcs,
        )?;
        if answer.eq_ignore_ascii_case("none") {
            args.dlcs.push("none".into());
        } else {
            args.dlcs.extend(
                answer
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    Ok(())
}

fn prompt(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String> {
    write!(writer, "{label} [{default}]: ")?;
    writer.flush()?;
    let mut answer = String::new();
    let read = reader.read_line(&mut answer)?;
    ensure!(read != 0, "interactive bootstrap input ended unexpectedly");
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer.to_owned()
    })
}

fn resolve_cache(args: &BootstrapArgs) -> Result<CacheChoice> {
    let legacy = args.redis_topology.map(|topology| match topology {
        RedisTopologyChoice::Standalone => CacheChoice::RedisStandalone,
        RedisTopologyChoice::Managed => CacheChoice::RedisManaged,
    });
    match (args.cache, legacy) {
        (Some(cache), Some(legacy)) if cache != legacy => bail!(
            "--cache and --redis-topology disagree; use one cache choice or make them equivalent"
        ),
        (Some(cache), _) => Ok(cache),
        (None, Some(legacy)) => Ok(legacy),
        (None, None) => Ok(CacheChoice::RedisManaged),
    }
}

fn resolve_style(args: &BootstrapArgs) -> Result<ResolvedStyle> {
    if let Some(path) = &args.css_file {
        ensure!(
            args.custom_css != Some(Toggle::Disabled),
            "--css-file contradicts --custom-css disabled"
        );
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect CSS source {}", path.display()))?;
        ensure!(
            metadata.file_type().is_file(),
            "CSS source must be a regular file and cannot be a symlink"
        );
        ensure!(
            metadata.len() <= 256 * 1024,
            "CSS source exceeds the 256 KiB installation limit"
        );
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read CSS source {}", path.display()))?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        return Ok(ResolvedStyle {
            installation: InstallationStyle {
                kind: InstallationStyleKind::Custom,
                id: None,
                file: Some("custom.css".into()),
                sha256: Some(digest.clone()),
            },
            css_bytes: Some(bytes),
            environment_value: format!("custom:{digest}"),
        });
    }

    if let Some(raw) = args.style.as_deref() {
        ensure!(
            args.custom_css != Some(Toggle::Enabled),
            "--style contradicts --custom-css enabled; use --css-file for custom CSS"
        );
        if matches!(
            raw.to_ascii_lowercase().as_str(),
            "none" | "off" | "disabled"
        ) {
            return Ok(ResolvedStyle {
                installation: InstallationStyle {
                    kind: InstallationStyleKind::None,
                    id: None,
                    file: None,
                    sha256: None,
                },
                css_bytes: None,
                environment_value: "none".into(),
            });
        }
        let id = raw
            .strip_prefix("builtin:")
            .unwrap_or(raw)
            .to_ascii_lowercase();
        ensure!(
            matches!(id.as_str(), "paper" | "ink" | "forest" | "terminal"),
            "unknown built-in style {id:?}; choose paper, ink, forest, or terminal"
        );
        let installation = InstallationStyle {
            kind: InstallationStyleKind::Builtin,
            id: Some(id.clone()),
            file: None,
            sha256: None,
        };
        installation
            .validate()
            .map_err(anyhow::Error::msg)
            .context("invalid --style")?;
        return Ok(ResolvedStyle {
            installation,
            css_bytes: None,
            environment_value: format!("builtin:{id}"),
        });
    }

    if args.custom_css == Some(Toggle::Disabled) {
        return Ok(ResolvedStyle {
            installation: InstallationStyle {
                kind: InstallationStyleKind::None,
                id: None,
                file: None,
                sha256: None,
            },
            css_bytes: None,
            environment_value: "none".into(),
        });
    }

    let bytes = include_bytes!("../../../deploy/custom.css").to_vec();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok(ResolvedStyle {
        installation: InstallationStyle {
            kind: InstallationStyleKind::Custom,
            id: None,
            file: Some("custom.css".into()),
            sha256: Some(digest.clone()),
        },
        css_bytes: Some(bytes),
        environment_value: format!("custom:{digest}"),
    })
}

fn resolve_references_source(args: &BootstrapArgs) -> Result<ResolvedReferences> {
    let bytes = if let Some(path) = args.references_file.as_deref() {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect references source {}", path.display()))?;
        ensure!(
            metadata.file_type().is_file(),
            "references source must be a regular file and cannot be a symlink"
        );
        ensure!(
            metadata.len() <= REFERENCES_MAX_BYTES,
            "references source cannot exceed 1 MiB"
        );
        fs::read(path)
            .with_context(|| format!("failed to read references source {}", path.display()))?
    } else {
        match args.language.unwrap_or_default() {
            LanguageChoice::Ko => include_bytes!("../../../deploy/references.md").to_vec(),
            LanguageChoice::En => include_bytes!("../../../deploy/references.en.md").to_vec(),
        }
    };
    ensure!(
        !bytes.is_empty() && u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= REFERENCES_MAX_BYTES,
        "references source must contain 1 byte to 1 MiB"
    );
    let source = std::str::from_utf8(&bytes).context("references source must be UTF-8")?;
    ensure!(
        !source.trim().is_empty() && !source.contains('\0'),
        "references source must contain non-empty UTF-8 text without NUL characters"
    );
    Ok(ResolvedReferences {
        contract: ReferencesSourceContract {
            path: REFERENCES_FILE.into(),
            sha256: format!("sha256:{:x}", Sha256::digest(&bytes)),
        },
        bytes,
    })
}

fn find_official_dlc(name: &str) -> Option<OfficialDlc> {
    let normalized = name.to_ascii_lowercase().replace('_', "-");
    OFFICIAL_DLCS.iter().copied().find(|dlc| {
        dlc.id == normalized
            || dlc.alias == normalized
            || (dlc.alias == "code-runner" && normalized == "code-runner-client")
            || (dlc.alias == "external-auth" && normalized == "external")
    })
}

fn resolve_dlcs(
    args: &BootstrapArgs,
    auth: AuthChoice,
    admin_auth: AdminAuthChoice,
    comments: bool,
) -> Result<Vec<ResolvedDlc>> {
    let none_requested = args
        .dlcs
        .iter()
        .any(|value| value.eq_ignore_ascii_case("none"));
    ensure!(
        !none_requested || args.dlcs.len() == 1,
        "DLC value none cannot be combined with another --dlc"
    );
    if none_requested {
        ensure!(
            !comments,
            "--dlc none conflicts with enabled comments; disable comments or install the comments DLC"
        );
        ensure!(
            !args.collaboration.enabled(),
            "--dlc none conflicts with collaboration; disable collaboration or install the rbac DLC"
        );
        ensure!(
            !auth.oauth_enabled() && admin_auth != AdminAuthChoice::External,
            "--dlc none conflicts with OAuth/external administrator auth; choose local/access-key/disabled auth or install external-auth"
        );
        return Ok(Vec::new());
    }

    let mut selected = BTreeMap::<String, String>::new();
    if args.dlcs.is_empty() && args.intent == Intent::Personal {
        for alias in RECOMMENDED_PERSONAL_DLCS {
            if alias == "seo" && !args.seo.enabled() {
                continue;
            }
            let official = find_official_dlc(alias).expect("known recommended official DLC");
            let manifest =
                PluginManifest::from_toml(official.manifest).map_err(anyhow::Error::msg)?;
            selected.insert(official.id.into(), format!("^{}", manifest.version));
        }
    }
    let mut imply = |alias: &str| -> Result<()> {
        let official = find_official_dlc(alias).expect("known official DLC alias");
        let manifest = PluginManifest::from_toml(official.manifest).map_err(anyhow::Error::msg)?;
        selected
            .entry(official.id.into())
            .or_insert_with(|| format!("={}", manifest.version));
        Ok(())
    };
    if args.seo.enabled() {
        imply("seo")?;
    }
    if comments {
        imply("comments")?;
    }
    if args.collaboration.enabled() {
        imply("rbac")?;
    }
    if auth.oauth_enabled() || admin_auth == AdminAuthChoice::External {
        imply("external-auth")?;
    }

    for raw in &args.dlcs {
        let (name, requirement) = raw
            .rsplit_once('@')
            .map_or((raw.as_str(), None), |(name, requirement)| {
                (name, Some(requirement))
            });
        ensure!(!name.is_empty(), "--dlc requires an id before @");
        let official =
            find_official_dlc(name).with_context(|| format!("unknown official DLC {name:?}"))?;
        ensure!(
            official.alias != "seo" || args.seo.enabled(),
            "--dlc seo conflicts with --seo disabled"
        );
        let manifest = PluginManifest::from_toml(official.manifest).map_err(anyhow::Error::msg)?;
        let requirement = requirement
            .map(str::to_owned)
            .unwrap_or_else(|| format!("^{}", manifest.version));
        let request = RequestedDlc {
            id: official.id.into(),
            version: requirement.clone(),
            enabled: true,
        };
        request.validate().map_err(anyhow::Error::msg)?;
        selected.insert(official.id.into(), requirement);
    }

    resolve_selected_dlcs(selected)
}

fn resolve_selected_dlcs(selected: BTreeMap<String, String>) -> Result<Vec<ResolvedDlc>> {
    selected
        .into_iter()
        .map(|(id, requirement)| {
            let official = find_official_dlc(&id).expect("selected DLC is official");
            let manifest =
                PluginManifest::from_toml(official.manifest).map_err(anyhow::Error::msg)?;
            ensure!(
                manifest
                    .supports_core(env!("CARGO_PKG_VERSION"))
                    .map_err(anyhow::Error::msg)?,
                "DLC {} does not support engine {}",
                manifest.id,
                env!("CARGO_PKG_VERSION")
            );
            let mut capabilities = manifest
                .permissions
                .iter()
                .map(|permission| {
                    serde_json::to_value(permission.capability)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .context("failed to serialize DLC capability")
                })
                .collect::<Result<Vec<_>>>()?;
            capabilities.sort();
            capabilities.dedup();
            let config_sha256 = manifest.config.as_ref().map(|config| {
                let bytes = serde_json::to_vec(config).expect("plugin config is serializable");
                format!("{:x}", Sha256::digest(bytes))
            });
            let installed = InstalledDlc {
                id: manifest.id.clone(),
                requested_version: requirement.clone(),
                version: manifest.version.clone(),
                core_compatibility: manifest
                    .core_compatibility
                    .clone()
                    .unwrap_or_else(|| format!("={}", env!("CARGO_PKG_VERSION"))),
                manifest_version: manifest.manifest_version,
                plugin_api: manifest.plugin_api.clone(),
                source_kind: InstalledDlcSourceKind::Bundled,
                source: official.source.into(),
                manifest_sha256: format!("{:x}", Sha256::digest(official.manifest.as_bytes())),
                artifact_sha256: None,
                enabled: true,
                approved_capabilities: capabilities,
                config_sha256,
                state_version: manifest.state.as_ref().map(|state| state.version),
                applied_migrations: Vec::new(),
            };
            Ok(ResolvedDlc {
                requested: RequestedDlc {
                    id,
                    version: requirement,
                    enabled: true,
                },
                installed,
                runtime_feature: official.runtime_feature,
            })
        })
        .collect()
}

pub fn bootstrap(args: BootstrapArgs) -> Result<()> {
    let args = resolve_prompted_args(args)?;
    if let Some(project) = args.compose_project.as_deref() {
        validate_compose_project(project)?;
    }
    let cache_choice = resolve_cache(&args)?;
    let style = resolve_style(&args)?;
    let references = resolve_references_source(&args)?;
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
    let auth = args.auth.unwrap_or(match args.intent {
        Intent::Community => AuthChoice::Local,
        Intent::Personal | Intent::Delivery => AuthChoice::Disabled,
    });
    ensure!(
        auth != AuthChoice::Oauth,
        "--auth oauth is not operational until a verified member OAuth adapter ships; use --auth local-and-oauth for local login plus reserved OAuth intent, or choose local/disabled"
    );
    let admin_auth = args.admin_auth.unwrap_or_else(|| {
        if args.intent == Intent::Delivery {
            AdminAuthChoice::Disabled
        } else {
            AdminAuthChoice::AccessKey
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
    let dlcs = resolve_dlcs(&args, auth, admin_auth, comments)?;
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
        !delivery || admin_auth == AdminAuthChoice::Disabled,
        "delivery intent requires --admin-auth disabled"
    );
    if admin_auth == AdminAuthChoice::External {
        ensure!(
            args.external_issuer_url.is_some()
                && args.external_client_id.is_some()
                && args.external_owner_subject.is_some(),
            "external administrator auth requires --external-issuer-url, --external-client-id, and --external-owner-subject"
        );
        let issuer = Url::parse(args.external_issuer_url.as_deref().unwrap())
            .context("--external-issuer-url must be an absolute URL")?;
        ensure!(
            external_issuer_url_is_safe(&issuer),
            "--external-issuer-url must be HTTPS without credentials, query, or fragment (localhost may use HTTP)"
        );
        validate_cli_text(
            "--external-client-id",
            args.external_client_id.as_deref().unwrap(),
            512,
        )?;
        validate_cli_text(
            "--external-owner-subject",
            args.external_owner_subject.as_deref().unwrap(),
            512,
        )?;
        validate_cli_text(
            "--external-label",
            args.external_label.as_deref().unwrap(),
            80,
        )?;
    } else {
        ensure!(
            args.external_issuer_url.is_none()
                && args.external_client_id.is_none()
                && args.external_owner_subject.is_none(),
            "external OIDC options require --admin-auth external"
        );
    }
    validate_cli_character_text(
        "--references-label",
        args.references_label.as_deref().unwrap(),
        40,
    )?;
    ensure!(
        admin_auth == AdminAuthChoice::Disabled
            || public_url.scheme() == "https"
            || matches!(
                public_url.host_str(),
                Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]")
            ),
        "remote administrator authentication requires an https --public-url; localhost may use http for development"
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
    let mut generated_targets = vec![
        "config.toml",
        ".env",
        "custom.css",
        REFERENCES_FILE,
        "osb.intent.json",
        INSTALL_MANIFEST,
        INSTALL_LOCK,
    ];
    if admin_auth == AdminAuthChoice::AccessKey {
        generated_targets.push("admin-access-key.txt");
    }
    for name in generated_targets {
        let target = args.directory.join(name);
        ensure!(
            !target.exists(),
            "refusing to overwrite existing file {}",
            target.display()
        );
    }
    let generated_gitignore = args.directory.join(".gitignore");
    if generated_gitignore.exists() {
        validate_existing_gitignore(&generated_gitignore)?;
    } else {
        write_new(&generated_gitignore, GENERATED_GITIGNORE.as_bytes())?;
    }
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
    let compose_project = args
        .compose_project
        .clone()
        .unwrap_or_else(|| format!("osb-{}", deployment_id.simple()));
    let data_volume = format!("osb-data-{}", deployment_id.simple());
    let (admin_access_key, admin_access_key_phc_b64) = if admin_auth == AdminAuthChoice::AccessKey {
        let key = random_access_key();
        let phc = hash_admin_access_key(&key)?;
        (Some(key), Some(BASE64_STANDARD.encode(phc.as_bytes())))
    } else {
        (None, None)
    };
    let selection = InstallationSelection {
        admin_auth: match admin_auth {
            AdminAuthChoice::AccessKey => InstallationAdminAuth::AccessKey,
            AdminAuthChoice::External => InstallationAdminAuth::External,
            AdminAuthChoice::Disabled => InstallationAdminAuth::Disabled,
        },
        style: style.installation.clone(),
        cache: cache_choice.installation(),
    };
    let installation_intent = InstallationIntent {
        schema_version: INSTALL_INTENT_SCHEMA_VERSION.into(),
        installation_id: deployment_id.to_string(),
        site_id: site_id.to_string(),
        created_with: env!("CARGO_PKG_VERSION").into(),
        selection: selection.clone(),
        dlcs: dlcs.iter().map(|dlc| dlc.requested.clone()).collect(),
    };
    let history = dlcs
        .iter()
        .enumerate()
        .map(|(index, dlc)| DlcHistoryRecord {
            sequence: u64::try_from(index).expect("bounded DLC count") + 1,
            action: DlcHistoryAction::Installed,
            dlc_id: dlc.installed.id.clone(),
            from_version: None,
            to_version: Some(dlc.installed.version.clone()),
            engine_version: env!("CARGO_PKG_VERSION").into(),
        })
        .collect();
    let mut installation_lock = InstallationLock {
        schema_version: INSTALL_LOCK_SCHEMA_VERSION.into(),
        installation_id: deployment_id.to_string(),
        engine: LockedEngine {
            version: env!("CARGO_PKG_VERSION").into(),
            config_schema_version: CONFIG_SCHEMA.into(),
            database_schema_version: DATABASE_SCHEMA_VERSION,
            plugin_api: PLUGIN_API_VERSION.into(),
            source: "source-checkout".into(),
            artifact_sha256: None,
        },
        selection: selection.clone(),
        dlcs: dlcs.iter().map(|dlc| dlc.installed.clone()).collect(),
        retained_dlcs: Vec::new(),
        history,
        lock_digest: String::new(),
    };
    installation_lock
        .refresh_digest()
        .map_err(anyhow::Error::msg)?;
    installation_lock.validate().map_err(anyhow::Error::msg)?;
    let installation_toml = installation_intent
        .to_toml_pretty()
        .map_err(anyhow::Error::msg)?;
    let lock_json = installation_lock
        .to_pretty_json()
        .map_err(anyhow::Error::msg)?;
    let config_render = ConfigRender {
        public_url: normalized_public_url,
        site_id,
        content_release,
        auth,
        admin_auth,
        comments,
        delivery,
        cache: cache_choice,
        custom_css: style.installation.kind == InstallationStyleKind::Custom,
        dlcs: &dlcs,
    };
    let config = render_config(&args, &config_render);
    write_new(&args.directory.join("config.toml"), config.as_bytes())?;
    let redis_password = cache_choice
        .installation()
        .redis_enabled()
        .then(random_hex_secret);
    let cache_signing_key = cache_choice
        .installation()
        .redis_enabled()
        .then(random_hex_secret);
    let environment = EnvironmentRender {
        auth,
        admin_auth,
        admin_access_key_phc_b64: admin_access_key_phc_b64.as_deref(),
        comments,
        collaboration,
        delivery,
        cache: cache_choice,
        style: &style,
        dlcs: &dlcs,
        lock_digest: &installation_lock.lock_digest,
        redis_password: redis_password.as_deref(),
        cache_signing_key: cache_signing_key.as_deref(),
        deployment_root: &deployment_root,
        public_url: normalized_public_url,
        compose_project: &compose_project,
        data_volume: &data_volume,
    };
    write_new_secret(
        &args.directory.join(".env"),
        render_env(&args, &environment).as_bytes(),
    )?;
    if let Some(access_key) = admin_access_key.as_deref() {
        let mut bytes = access_key.as_bytes().to_vec();
        bytes.push(b'\n');
        write_new_secret(&args.directory.join("admin-access-key.txt"), &bytes)?;
    }
    // Compose bind-mounts this path even when the feature is disabled. Keeping a
    // harmless first-party template avoids Docker creating a directory at the
    // file mount while the semantic flag still controls whether it is served.
    write_new(
        &args.directory.join("custom.css"),
        style
            .css_bytes
            .as_deref()
            .unwrap_or(include_bytes!("../../../deploy/custom.css")),
    )?;
    // Compose mounts a real regular file with create_host_path disabled. Keep
    // an editable deployment-local copy so generated environments work from
    // any current directory and never fall back to a Docker-created directory.
    write_new(&args.directory.join(REFERENCES_FILE), &references.bytes)?;
    write_new(
        &args.directory.join(INSTALL_MANIFEST),
        installation_toml.as_bytes(),
    )?;
    write_new(&args.directory.join(INSTALL_LOCK), lock_json.as_bytes())?;
    let runtime_profile = cache_choice
        .compose_profile()
        .map(|profile| format!("--profile {profile} "))
        .unwrap_or_default();
    let start_command = compose_command(
        &compose_file,
        &deployment_root,
        &compose_project,
        &format!("{runtime_profile}up -d --build --wait"),
    );
    let doctor_command = compose_command(
        &compose_file,
        &deployment_root,
        &compose_project,
        &format!("{runtime_profile}exec -T blog osb doctor --config /config/config.toml"),
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
        installation_manifest: INSTALL_MANIFEST,
        installation_lock: INSTALL_LOCK,
        references_source: references.contract.clone(),
        guarantees: vec![
            "Markdown remains exportable",
            "SQLite and first-party blobs remain authoritative",
            if cache_choice == CacheChoice::None {
                "Redis is deliberately absent and public reads use the authoritative origin"
            } else {
                "Redis accelerates the hot path but is never the only copy"
            },
            if cache_choice == CacheChoice::None {
                "disabled cache modules require no cache credential"
            } else {
                "Redis cache bodies require an application-only integrity signature"
            },
            "unknown configuration keys fail closed",
            "delivery intent rejects mutations",
        ],
        features: ManifestFeatures {
            member_auth: auth,
            admin_auth,
            registration_open,
            comments,
            collaboration,
            custom_css: style.installation.kind == InstallationStyleKind::Custom,
            agent_discovery: args.agent_discovery.enabled(),
        },
        data: ManifestData {
            source_of_truth: "sqlite_and_content_addressed_blobs",
            cache: if cache_choice == CacheChoice::None {
                "none"
            } else {
                "redis"
            },
            redis_required: cache_choice != CacheChoice::None,
            redis_topology: cache_choice.redis_topology(),
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
    println!("  administrator auth: {}", admin_auth.as_str());
    println!("  cache: {}", cache_choice.installation().as_str());
    println!("  style: {}", style.environment_value);
    println!("  DLCs: {}", installation_lock.dlcs.len());
    println!("  config: {}", args.directory.join("config.toml").display());
    println!(
        "  installation: {} / {}",
        args.directory.join(INSTALL_MANIFEST).display(),
        args.directory.join(INSTALL_LOCK).display()
    );
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
        if admin_auth == AdminAuthChoice::AccessKey {
            println!(
                "  protected administrator access key: {}",
                args.directory.join("admin-access-key.txt").display()
            );
        }
        println!("Next: {start_command}");
    }
    Ok(())
}

fn validate_cli_text(name: &str, value: &str, maximum: usize) -> Result<()> {
    ensure!(
        value.trim() == value
            && !value.is_empty()
            && value.len() <= maximum
            && !value.chars().any(char::is_control),
        "{name} must be 1-{maximum} trimmed non-control bytes"
    );
    Ok(())
}

fn validate_cli_character_text(name: &str, value: &str, maximum: usize) -> Result<()> {
    ensure!(
        value.trim() == value
            && !value.is_empty()
            && value.chars().count() <= maximum
            && !value.chars().any(char::is_control),
        "{name} must be 1-{maximum} trimmed non-control characters"
    );
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

fn is_loopback_url(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]")
    )
}

fn external_issuer_url_is_safe(url: &Url) -> bool {
    url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (url.scheme() == "https" || (url.scheme() == "http" && is_loopback_url(url)))
}

fn validate_deployment_path(path: &Path) -> Result<()> {
    let value = path.to_string_lossy();
    ensure!(
        !value.chars().any(char::is_control) && !value.contains('\''),
        "deployment directory path cannot contain control characters or apostrophes"
    );
    Ok(())
}

fn validate_compose_project(value: &str) -> Result<()> {
    ensure!(
        (1..=63).contains(&value.len())
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            }),
        "--compose-project must contain 1-63 lowercase ASCII letters, digits, hyphens, or underscores and start with a letter or digit"
    );
    Ok(())
}

fn validate_existing_gitignore(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect existing {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "existing {} must be a regular file before bootstrap can create secrets",
        path.display()
    );
    ensure!(
        metadata.len() <= GITIGNORE_LIMIT,
        "existing {} exceeds the 256 KiB safety limit",
        path.display()
    );
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read existing {}", path.display()))?;
    let entries = source
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let missing = REQUIRED_SECRET_IGNORES
        .iter()
        .filter(|required| {
            !entries
                .iter()
                .any(|entry| *entry == **required || entry.strip_prefix('/') == Some(**required))
        })
        .copied()
        .collect::<Vec<_>>();
    ensure!(
        missing.is_empty(),
        "existing {} must ignore {} before bootstrap can create secrets; add the exact entr{} and retry",
        path.display(),
        missing.join(" and "),
        if missing.len() == 1 { "y" } else { "ies" }
    );
    let unsafe_negations = REQUIRED_SECRET_IGNORES
        .iter()
        .flat_map(|required| {
            let required = *required;
            let last_exact_ignore = entries
                .iter()
                .rposition(|entry| *entry == required || entry.strip_prefix('/') == Some(required))
                .expect("missing exact ignores were rejected above");
            entries
                .iter()
                .enumerate()
                .skip(last_exact_ignore + 1)
                .filter_map(move |(index, entry)| {
                    entry.strip_prefix('!').and_then(|negation| {
                        negation_may_restore_protected_path(required, negation)
                            .then(|| format!("line {} ({entry})", index + 1))
                    })
                })
        })
        .collect::<Vec<_>>();
    ensure!(
        unsafe_negations.is_empty(),
        "existing {} has later negation rule(s) that may re-include protected secrets or backups: {}; put the exact protective entries after those rules and retry",
        path.display(),
        unsafe_negations.join(", ")
    );
    Ok(())
}

fn negation_may_restore_protected_path(required: &str, negation: &str) -> bool {
    let protected_name = required.trim_matches('/');
    negation
        .trim_matches('/')
        .split('/')
        .any(|component| glob_component_may_match(component, protected_name))
}

fn glob_component_may_match(pattern: &str, value: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Token {
        AnySequence,
        AnyOne,
        Literal(char),
    }

    let characters = pattern.chars().collect::<Vec<_>>();
    let mut tokens = Vec::with_capacity(characters.len());
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '*' => {
                if !matches!(tokens.last(), Some(Token::AnySequence)) {
                    tokens.push(Token::AnySequence);
                }
                index += 1;
            }
            '?' => {
                tokens.push(Token::AnyOne);
                index += 1;
            }
            '[' => {
                // Git character classes consume one path-component character.
                // Treat their contents conservatively as AnyOne; exact class
                // membership is unnecessary for a fail-closed overlap check.
                tokens.push(Token::AnyOne);
                index = characters[index + 1..]
                    .iter()
                    .position(|character| *character == ']')
                    .map_or(index + 1, |offset| index + offset + 2);
            }
            '\\' if index + 1 < characters.len() => {
                tokens.push(Token::Literal(characters[index + 1]));
                index += 2;
            }
            literal => {
                tokens.push(Token::Literal(literal));
                index += 1;
            }
        }
    }

    let value = value.chars().collect::<Vec<_>>();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in tokens {
        let mut current = vec![false; value.len() + 1];
        if matches!(token, Token::AnySequence) {
            current[0] = previous[0];
        }
        for value_index in 1..=value.len() {
            current[value_index] = match token {
                Token::AnySequence => previous[value_index] || current[value_index - 1],
                Token::AnyOne => previous[value_index - 1],
                Token::Literal(literal) => {
                    previous[value_index - 1] && literal == value[value_index - 1]
                }
            };
        }
        previous = current;
    }
    previous[value.len()]
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

fn random_access_key() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn hash_admin_access_key(access_key: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(access_key.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| anyhow::anyhow!("failed to hash administrator access key: {error}"))
}

struct ConfigRender<'a> {
    public_url: &'a str,
    site_id: Uuid,
    content_release: &'a str,
    auth: AuthChoice,
    admin_auth: AdminAuthChoice,
    comments: bool,
    delivery: bool,
    cache: CacheChoice,
    custom_css: bool,
    dlcs: &'a [ResolvedDlc],
}

fn render_config(args: &BootstrapArgs, config: &ConfigRender<'_>) -> String {
    let (redis_enabled, redis_topology, redis_url, sentinel_urls) = match config.cache {
        CacheChoice::None => (
            false,
            "standalone",
            "redis://redis-primary:6379/",
            "sentinel_urls = []",
        ),
        CacheChoice::RedisStandalone => (
            true,
            "standalone",
            "redis://redis-primary:6379/",
            "sentinel_urls = []",
        ),
        CacheChoice::RedisManaged => (
            true,
            "sentinel",
            "redis://redis-primary:6379/",
            "sentinel_urls = [\"redis://redis-sentinel-1:26379/\", \"redis://redis-sentinel-2:26379/\", \"redis://redis-sentinel-3:26379/\"]",
        ),
    };
    let feature_enabled = |name: &str| {
        config
            .dlcs
            .iter()
            .any(|dlc| dlc.runtime_feature == name && dlc.installed.enabled)
    };
    format!(
        r#"schema_version = "{CONFIG_SCHEMA}"

[semantic]
intent = "{intent}"

[server]
bind = "0.0.0.0:8787"
public_url = "{public_url}"
article_base_path = "blog"
language = "{language}"
site_id = "{site_id}"
no_index = {no_index}

[storage]
database = "/data/open-soverign-blog.db"
blob_directory = "/data/blobs"
profile = "{database_profile}"

[security]
# Secrets are environment-only. Never put OAuth or owner credentials here.

[admin]
auth = "{admin_auth}"
session_days = 30
{external_admin}

[community]
auth = "{auth}"
registration_open = {registration_open}
comments = {comments}
collaboration = {collaboration}

[deployment]
delivery_only = {delivery}

[redis]
enabled = {redis_enabled}
topology = "{redis_topology}"
url = "{redis_url}"
{sentinel_urls}
sentinel_master = "osb-primary"
namespace = "osb"
content_release = "{content_release}"
required = {redis_enabled}
response_ttl_seconds = 60
connect_timeout_ms = 2000

[appearance]
custom_css = {custom_css}
custom_css_file = "/config/custom.css"

[references]
enabled = true
label = {references_label}
markdown_file = "/config/references.md"

[discovery]
agent_txt = {agent_discovery}

[operations]
managed_backups = {managed_backups}
backup_directory = "/backups"
backup_interval_minutes = {backup_interval}
backup_retention = {backup_retention}

[features]
external_auth = {external_auth}
rbac = {rbac}
comments = {comments_feature}
seo = {seo}
code_runner = {code_runner}
ads = {ads}
ai_summary = {ai_summary}
"#,
        intent = args.intent.as_str(),
        public_url = config.public_url,
        language = args.language.unwrap_or_default().as_str(),
        site_id = config.site_id,
        no_index = !feature_enabled("seo"),
        database_profile = args.database_profile.as_str(),
        auth = config.auth.as_str(),
        admin_auth = config.admin_auth.as_str(),
        external_admin = render_external_admin(args, config.admin_auth),
        registration_open = args.registration.enabled(),
        collaboration = args.collaboration.enabled(),
        custom_css = config.custom_css,
        references_label = toml::Value::String(
            args.references_label
                .clone()
                .expect("language defaults are resolved before rendering"),
        ),
        agent_discovery = args.agent_discovery.enabled(),
        managed_backups = args.managed_backups.enabled() && !config.delivery,
        backup_interval = args.backup_interval_minutes,
        backup_retention = args.backup_retention,
        external_auth = feature_enabled("external_auth"),
        rbac = feature_enabled("rbac"),
        comments_feature = feature_enabled("comments"),
        seo = feature_enabled("seo"),
        code_runner = feature_enabled("code_runner"),
        ads = feature_enabled("ads"),
        ai_summary = feature_enabled("ai_summary"),
        redis_enabled = redis_enabled,
        content_release = config.content_release,
        comments = config.comments,
        delivery = config.delivery,
    )
}

fn render_external_admin(args: &BootstrapArgs, admin_auth: AdminAuthChoice) -> String {
    if admin_auth != AdminAuthChoice::External {
        return String::new();
    }
    let issuer = toml::Value::String(args.external_issuer_url.clone().unwrap()).to_string();
    let client_id = toml::Value::String(args.external_client_id.clone().unwrap()).to_string();
    let owner_subject =
        toml::Value::String(args.external_owner_subject.clone().unwrap()).to_string();
    let label = toml::Value::String(
        args.external_label
            .clone()
            .expect("language defaults are resolved before rendering"),
    )
    .to_string();
    format!(
        "\n[admin.external]\nadapter = \"oidc\"\nissuer_url = {issuer}\nclient_id = {client_id}\nowner_subject = {owner_subject}\nlabel = {label}\n"
    )
}

struct EnvironmentRender<'a> {
    auth: AuthChoice,
    admin_auth: AdminAuthChoice,
    admin_access_key_phc_b64: Option<&'a str>,
    comments: bool,
    collaboration: bool,
    delivery: bool,
    cache: CacheChoice,
    style: &'a ResolvedStyle,
    dlcs: &'a [ResolvedDlc],
    lock_digest: &'a str,
    redis_password: Option<&'a str>,
    cache_signing_key: Option<&'a str>,
    deployment_root: &'a Path,
    public_url: &'a str,
    compose_project: &'a str,
    data_volume: &'a str,
}

fn render_env(args: &BootstrapArgs, environment: &EnvironmentRender<'_>) -> String {
    let features = environment
        .dlcs
        .iter()
        .filter(|dlc| dlc.installed.enabled)
        .map(|dlc| dlc.runtime_feature)
        .filter(|name| {
            matches!(
                *name,
                "seo"
                    | "home_curation"
                    | "ai_authorship"
                    | "ai_summary"
                    | "social_embeds"
                    | "release_check"
                    | "comments"
                    | "rbac"
                    | "external_auth"
                    | "code_runner"
                    | "ads"
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let features = if features.is_empty() {
        "none".to_owned()
    } else {
        features
    };
    let dlc_ids = environment
        .dlcs
        .iter()
        .filter(|dlc| dlc.installed.enabled)
        .map(|dlc| dlc.installed.id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let redis_enabled = environment.cache != CacheChoice::None;
    let redis_topology = match environment.cache {
        CacheChoice::None | CacheChoice::RedisStandalone => "standalone",
        CacheChoice::RedisManaged => "sentinel",
    };
    format!(
        "COMPOSE_PROJECT_NAME={}\nOSB_DATA_VOLUME={}\nOSB_CONFIG=/config/config.toml\nOSB_CONFIG_SOURCE='{}'\nOSB_HANDOFF_SOURCE='{}'\nOSB_CUSTOM_CSS_SOURCE='{}'\nOSB_REFERENCES_SOURCE='{}'\nOSB_INSTALL_MANIFEST=/config/osb.install.toml\nOSB_INSTALL_LOCK=/config/osb.lock.json\nOSB_INSTALL_SOURCE='{}'\nOSB_LOCK_SOURCE='{}'\nOSB_INSTALL_LOCK_DIGEST={}\nOSB_ALLOW_UNTRACKED_INSTALLATION=false\nOSB_STYLE={}\nOSB_CACHE={}\nOSB_DLC_IDS={}\nOSB_PUBLIC_URL='{}'\nOSB_LANGUAGE={}\nOSB_INTENT={}\nOSB_AUTH_MODE={}\nOSB_ADMIN_AUTH={}\nOSB_ADMIN_ACCESS_KEY_PHC_B64={}\nOSB_ADMIN_AUTH_ROTATE=false\nOSB_EXTERNAL_CLIENT_SECRET=\nOSB_REGISTRATION_OPEN={}\nOSB_COMMENTS={}\nOSB_COLLABORATION={}\nOSB_CUSTOM_CSS={}\nOSB_AGENT_DISCOVERY={}\nOSB_DELIVERY_ONLY={}\nOSB_FEATURES={}\nOSB_REDIS_ENABLED={}\nOSB_REDIS_TOPOLOGY={}\nOSB_REDIS_REQUIRED={}\nOSB_REDIS_PASSWORD={}\nOSB_CACHE_SIGNING_KEY={}\nOSB_MANAGED_BACKUPS={}\nOSB_BACKUP_VOLUME='{}'\nRUST_LOG=info\n",
        environment.compose_project,
        environment.data_volume,
        environment.deployment_root.join("config.toml").display(),
        environment
            .deployment_root
            .join("osb.intent.json")
            .display(),
        environment.deployment_root.join("custom.css").display(),
        environment.deployment_root.join(REFERENCES_FILE).display(),
        environment.deployment_root.join(INSTALL_MANIFEST).display(),
        environment.deployment_root.join(INSTALL_LOCK).display(),
        environment.lock_digest,
        environment.style.environment_value,
        environment.cache.installation().as_str(),
        dlc_ids,
        environment.public_url,
        args.language.unwrap_or_default().as_str(),
        args.intent.as_str(),
        environment.auth.as_str(),
        environment.admin_auth.as_str(),
        environment.admin_access_key_phc_b64.unwrap_or_default(),
        args.registration.enabled(),
        environment.comments,
        environment.collaboration,
        environment.style.installation.kind == InstallationStyleKind::Custom,
        args.agent_discovery.enabled(),
        environment.delivery,
        features,
        redis_enabled,
        redis_topology,
        redis_enabled,
        environment.redis_password.unwrap_or_default(),
        environment.cache_signing_key.unwrap_or_default(),
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

pub fn installation(args: InstallationArgs) -> Result<()> {
    match args.action {
        InstallationAction::Verify { intent, lock } => {
            let (intent_contract, lock_contract) = read_installation_pair(&intent, &lock)?;
            verify_intent_lock_pair(&intent_contract, &lock_contract)?;
            println!(
                "installation verified: {} · engine {} · {} DLC(s)",
                lock_contract.lock_digest,
                lock_contract.engine.version,
                lock_contract.dlcs.len()
            );
            Ok(())
        }
        InstallationAction::RecordEngineUpgrade {
            intent,
            lock,
            from,
            to,
            source,
            artifact_sha256,
        } => record_engine_upgrade(&intent, &lock, &from, &to, source, artifact_sha256),
        InstallationAction::Adopt { directory } => adopt_installation(&directory),
        InstallationAction::Dlc(args) => dlc_lifecycle::run(args),
    }
}

fn read_installation_pair(
    intent_path: &Path,
    lock_path: &Path,
) -> Result<(InstallationIntent, InstallationLock)> {
    ensure_regular_contract_file(intent_path, INSTALL_MANIFEST)?;
    ensure_regular_contract_file(lock_path, INSTALL_LOCK)?;
    let intent = InstallationIntent::from_toml(&fs::read_to_string(intent_path)?)
        .map_err(anyhow::Error::msg)?;
    let lock =
        InstallationLock::from_json(&fs::read_to_string(lock_path)?).map_err(anyhow::Error::msg)?;
    Ok((intent, lock))
}

fn verify_intent_lock_pair(intent: &InstallationIntent, lock: &InstallationLock) -> Result<()> {
    intent.validate().map_err(anyhow::Error::msg)?;
    lock.validate().map_err(anyhow::Error::msg)?;
    verify_bundled_official_manifest_bytes(lock)?;
    ensure!(
        intent.installation_id == lock.installation_id,
        "installation id differs between intent and lock"
    );
    ensure!(
        intent.selection == lock.selection,
        "structural selection differs between intent and lock"
    );
    let requested = intent
        .dlcs
        .iter()
        .map(|dlc| (dlc.id.as_str(), (dlc.version.as_str(), dlc.enabled)))
        .collect::<BTreeMap<_, _>>();
    let installed = lock
        .dlcs
        .iter()
        .map(|dlc| {
            (
                dlc.id.as_str(),
                (dlc.requested_version.as_str(), dlc.enabled),
            )
        })
        .collect::<BTreeMap<_, _>>();
    ensure!(
        requested == installed,
        "requested DLC set differs from exact installed records"
    );
    Ok(())
}

fn verify_bundled_official_manifest_bytes(lock: &InstallationLock) -> Result<()> {
    for installed in &lock.dlcs {
        if installed.source_kind != InstalledDlcSourceKind::Bundled {
            continue;
        }
        let official = find_official_dlc(&installed.id).with_context(|| {
            format!(
                "bundled DLC {} is not present in this CLI's official catalog",
                installed.id
            )
        })?;
        let manifest = PluginManifest::from_toml(official.manifest)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("bundled official manifest {} is invalid", official.id))?;
        let actual_digest = format!("{:x}", Sha256::digest(official.manifest.as_bytes()));
        ensure!(
            installed.source == official.source
                && installed.manifest_sha256 == actual_digest
                && installed.id == manifest.id
                && installed.version == manifest.version
                && installed.manifest_version == manifest.manifest_version
                && installed.plugin_api == manifest.plugin_api
                && manifest.core_compatibility.as_deref()
                    == Some(installed.core_compatibility.as_str()),
            "bundled DLC {} lock metadata does not match the manifest bytes compiled into this CLI",
            installed.id
        );
    }
    Ok(())
}

fn record_engine_upgrade(
    intent_path: &Path,
    lock_path: &Path,
    from: &str,
    to: &str,
    source: String,
    artifact_sha256: Option<String>,
) -> Result<()> {
    ensure!(
        to == env!("CARGO_PKG_VERSION"),
        "record-engine-upgrade must run from the target CLI {}; received --to {to}",
        env!("CARGO_PKG_VERSION")
    );
    let (intent, mut lock) = read_installation_pair(intent_path, lock_path)?;
    verify_intent_lock_pair(&intent, &lock)?;
    lock.record_engine_upgrade(from, to, source, artifact_sha256)
        .map_err(anyhow::Error::msg)?;
    // The candidate CLI owns the engine-side compatibility tuple. Updating all
    // four values together lets a future schema migration produce a lock the
    // candidate server will accept after the updater swaps the release.
    lock.engine.config_schema_version = CONFIG_SCHEMA.into();
    lock.engine.database_schema_version = DATABASE_SCHEMA_VERSION;
    lock.engine.plugin_api = PLUGIN_API_VERSION.into();
    lock.refresh_digest().map_err(anyhow::Error::msg)?;
    verify_intent_lock_pair(&intent, &lock)?;
    let rendered = lock.to_pretty_json().map_err(anyhow::Error::msg)?;
    atomic_replace_regular_file(lock_path, rendered.as_bytes())?;
    println!(
        "engine upgrade recorded: {from} -> {to} · lockDigest={}",
        lock.lock_digest
    );
    Ok(())
}

fn atomic_replace_regular_file(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_regular_contract_file(path, "installation lock")?;
    let metadata = fs::metadata(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("installation lock file name is not UTF-8")?;
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::now_v7().simple()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(metadata.permissions().mode() & 0o777);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        fs::set_permissions(&temporary, metadata.permissions())?;
        fs::rename(&temporary, path)?;
        let directory = OpenOptions::new().read(true).open(parent)?;
        directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn adopt_installation(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect deployment {}", directory.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "adoption directory must be a real directory and cannot be a symlink"
    );
    let root = directory
        .canonicalize()
        .with_context(|| format!("failed to resolve deployment {}", directory.display()))?;
    validate_deployment_path(&root)?;

    let config_path = root.join("config.toml");
    let env_path = root.join(".env");
    let intent_path = root.join(INSTALL_MANIFEST);
    let lock_path = root.join(INSTALL_LOCK);
    ensure_regular_contract_file(&config_path, "deployment config")?;
    ensure_regular_contract_file(&env_path, "protected deployment environment")?;
    #[cfg(unix)]
    ensure!(
        fs::metadata(&env_path)?.permissions().mode() & 0o077 == 0,
        "refusing to read an unprotected .env; remove group/world permissions first"
    );
    ensure!(
        !intent_path.exists() && !lock_path.exists(),
        "refusing to overwrite an existing installation manifest or lock"
    );

    let config_source = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut config: DoctorConfig = toml::from_str(&config_source)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let environment = read_environment_file(&env_path)?;
    apply_environment_overrides_with(&mut config, |name| environment.get(name).cloned())?;
    let mut checks = Vec::new();
    check_semantics(&config, &mut checks);
    let failures = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Fail)
        .map(|check| check.id)
        .collect::<Vec<_>>();
    ensure!(
        failures.is_empty(),
        "deployment cannot be adopted until these semantic checks pass: {}",
        failures.join(", ")
    );

    let site_id = Uuid::parse_str(&config.server.site_id)
        .context("effective server.site_id/OSB_SITE_ID must be a UUID before adoption")?;
    let installation_id = infer_adopted_installation_id(&root, &environment)?;
    let selection = InstallationSelection {
        admin_auth: match config.admin.auth.as_str() {
            "access_key" => InstallationAdminAuth::AccessKey,
            "external" => InstallationAdminAuth::External,
            "disabled" => InstallationAdminAuth::Disabled,
            _ => bail!("effective administrator authentication is not adoptable"),
        },
        style: infer_adopted_style(&root, &config, &environment)?,
        cache: if !config.redis.enabled {
            InstallationCache::None
        } else {
            match config.redis.topology.as_str() {
                "standalone" => InstallationCache::RedisStandalone,
                "sentinel" => InstallationCache::RedisManaged,
                _ => bail!("effective Redis topology is not adoptable"),
            }
        },
    };
    let dlcs = infer_adopted_dlcs(&config, &environment)?;
    let intent = InstallationIntent {
        schema_version: INSTALL_INTENT_SCHEMA_VERSION.into(),
        installation_id: installation_id.to_string(),
        site_id: site_id.to_string(),
        created_with: env!("CARGO_PKG_VERSION").into(),
        selection: selection.clone(),
        dlcs: dlcs.iter().map(|dlc| dlc.requested.clone()).collect(),
    };
    let history = dlcs
        .iter()
        .enumerate()
        .map(|(index, dlc)| DlcHistoryRecord {
            sequence: u64::try_from(index).expect("bounded DLC count") + 1,
            action: DlcHistoryAction::Installed,
            dlc_id: dlc.installed.id.clone(),
            from_version: None,
            to_version: Some(dlc.installed.version.clone()),
            engine_version: env!("CARGO_PKG_VERSION").into(),
        })
        .collect();
    let mut lock = InstallationLock {
        schema_version: INSTALL_LOCK_SCHEMA_VERSION.into(),
        installation_id: installation_id.to_string(),
        engine: LockedEngine {
            version: env!("CARGO_PKG_VERSION").into(),
            config_schema_version: CONFIG_SCHEMA.into(),
            database_schema_version: DATABASE_SCHEMA_VERSION,
            plugin_api: PLUGIN_API_VERSION.into(),
            source: "adopted-v2".into(),
            artifact_sha256: None,
        },
        selection,
        dlcs: dlcs.into_iter().map(|dlc| dlc.installed).collect(),
        retained_dlcs: Vec::new(),
        history,
        lock_digest: String::new(),
    };
    lock.refresh_digest().map_err(anyhow::Error::msg)?;
    verify_intent_lock_pair(&intent, &lock)?;
    let rendered_intent = intent.to_toml_pretty().map_err(anyhow::Error::msg)?;
    let rendered_lock = lock.to_pretty_json().map_err(anyhow::Error::msg)?;
    write_contract_pair_new(
        &intent_path,
        rendered_intent.as_bytes(),
        &lock_path,
        rendered_lock.as_bytes(),
    )?;
    println!(
        "deployment adopted: {} · lockDigest={} · {} DLC(s)",
        root.display(),
        lock.lock_digest,
        lock.dlcs.len()
    );
    println!(
        "existing config, .env, and CSS were left byte-for-byte unchanged; set OSB_INSTALL_LOCK_DIGEST={} when you are ready to enforce the tracked contract at startup",
        lock.lock_digest
    );
    Ok(())
}

fn infer_adopted_installation_id(
    root: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<Uuid> {
    let handoff_path = root.join("osb.intent.json");
    let handoff_id = if handoff_path.exists() {
        ensure_regular_contract_file(&handoff_path, "legacy intent handoff")?;
        let metadata = fs::metadata(&handoff_path)?;
        ensure!(
            metadata.len() <= 256 * 1024,
            "legacy intent handoff exceeds 256 KiB"
        );
        let handoff: serde_json::Value = serde_json::from_slice(&fs::read(&handoff_path)?)
            .context("legacy osb.intent.json is invalid JSON")?;
        handoff
            .get("deploymentId")
            .and_then(serde_json::Value::as_str)
            .map(|value| Uuid::parse_str(value).context("legacy deploymentId is not a UUID"))
            .transpose()?
    } else {
        None
    };
    let compose_id = environment
        .get("COMPOSE_PROJECT_NAME")
        .and_then(|value| value.strip_prefix("osb-"))
        .map(|value| Uuid::parse_str(value).context("COMPOSE_PROJECT_NAME does not contain a UUID"))
        .transpose()?;
    if let (Some(handoff), Some(compose)) = (handoff_id, compose_id) {
        ensure!(
            handoff == compose,
            "legacy deploymentId and COMPOSE_PROJECT_NAME disagree"
        );
    }
    Ok(handoff_id.or(compose_id).unwrap_or_else(Uuid::now_v7))
}

fn infer_adopted_style(
    root: &Path,
    config: &DoctorConfig,
    environment: &BTreeMap<String, String>,
) -> Result<InstallationStyle> {
    let declared = environment.get("OSB_STYLE").map(String::as_str);
    if config.appearance.custom_css {
        ensure!(
            !declared.is_some_and(|value| value == "none" || value.starts_with("builtin:")),
            "OSB_STYLE contradicts the effective custom CSS switch"
        );
        let css_path = deployment_path(root, &config.appearance.custom_css_file);
        ensure_regular_contract_file(&css_path, "adopted custom CSS")?;
        let canonical_css = css_path
            .canonicalize()
            .with_context(|| format!("failed to resolve CSS {}", css_path.display()))?;
        ensure!(
            canonical_css.starts_with(root),
            "adoption only accepts custom CSS stored inside the deployment directory"
        );
        let relative = canonical_css
            .strip_prefix(root)
            .expect("CSS containment checked")
            .to_string_lossy()
            .trim_start_matches('/')
            .to_owned();
        let bytes = fs::read(&canonical_css)?;
        ensure!(bytes.len() <= 256 * 1024, "custom CSS exceeds 256 KiB");
        let digest = format!("{:x}", Sha256::digest(bytes));
        if let Some(declared) = declared {
            ensure!(
                declared == format!("custom:{digest}"),
                "OSB_STYLE custom digest differs from installed CSS"
            );
        }
        let style = InstallationStyle {
            kind: InstallationStyleKind::Custom,
            id: None,
            file: Some(relative),
            sha256: Some(digest),
        };
        style.validate().map_err(anyhow::Error::msg)?;
        return Ok(style);
    }

    match declared {
        None | Some("none") => Ok(InstallationStyle {
            kind: InstallationStyleKind::None,
            id: None,
            file: None,
            sha256: None,
        }),
        Some(value) => {
            let id = value
                .strip_prefix("builtin:")
                .context("OSB_STYLE is ambiguous; expected none or builtin:STYLE")?;
            ensure!(
                matches!(id, "paper" | "ink" | "forest" | "terminal"),
                "OSB_STYLE names a built-in style this engine does not supply"
            );
            Ok(InstallationStyle {
                kind: InstallationStyleKind::Builtin,
                id: Some(id.into()),
                file: None,
                sha256: None,
            })
        }
    }
}

fn infer_adopted_dlcs(
    config: &DoctorConfig,
    environment: &BTreeMap<String, String>,
) -> Result<Vec<ResolvedDlc>> {
    let exact_ids = environment.get("OSB_DLC_IDS").map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    });
    let runtime = environment.get("OSB_FEATURES").map(|raw| {
        if raw.eq_ignore_ascii_case("none") {
            Vec::new()
        } else {
            raw.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        }
    });
    let mut required = Vec::new();
    if config.community.comments {
        required.push("comments");
    }
    if config.community.collaboration {
        required.push("rbac");
    }
    if matches!(config.community.auth.as_str(), "oauth" | "local_and_oauth")
        || config.admin.auth == "external"
    {
        required.push("external-auth");
    }

    let names = if let Some(ids) = exact_ids {
        ensure_unique_values(&ids, "OSB_DLC_IDS")?;
        if let Some(runtime) = &runtime {
            ensure_unique_values(runtime, "OSB_FEATURES")?;
            let runtime_ids = runtime
                .iter()
                .map(|name| {
                    find_official_dlc(name)
                        .map(|dlc| dlc.id)
                        .with_context(|| format!("unknown OSB_FEATURES module {name:?}"))
                })
                .collect::<Result<Vec<_>>>()?;
            let exact_set = ids
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            let runtime_set = runtime_ids
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            ensure!(
                exact_set == runtime_set,
                "OSB_DLC_IDS and OSB_FEATURES describe different official modules"
            );
        }
        for required in required {
            let required_id = find_official_dlc(required).expect("known implied DLC").id;
            ensure!(
                ids.iter().any(|id| id == required_id),
                "effective community/admin configuration requires DLC {required_id}"
            );
        }
        ids
    } else {
        let mut names = runtime.unwrap_or_else(|| config.features.enabled_aliases());
        names.extend(required.into_iter().map(str::to_owned));
        names.sort();
        names.dedup();
        names
    };

    let mut selected = BTreeMap::new();
    for name in names {
        let official = find_official_dlc(&name)
            .with_context(|| format!("unknown official module {name:?} cannot be adopted"))?;
        let manifest = PluginManifest::from_toml(official.manifest).map_err(anyhow::Error::msg)?;
        selected.insert(official.id.into(), format!("={}", manifest.version));
    }
    resolve_selected_dlcs(selected)
}

fn ensure_unique_values(values: &[String], name: &str) -> Result<()> {
    let unique = values
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        unique.len() == values.len(),
        "{name} contains a duplicate value"
    );
    Ok(())
}

fn read_environment_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let metadata = fs::metadata(path)?;
    ensure!(metadata.len() <= 1024 * 1024, ".env exceeds 1 MiB");
    let mut values = BTreeMap::new();
    for (line_number, line) in fs::read_to_string(path)?.lines().enumerate() {
        ensure!(
            line.len() <= 16 * 1024,
            ".env line {} is too long",
            line_number + 1
        );
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, raw) = line
            .split_once('=')
            .with_context(|| format!("invalid .env line {}", line_number + 1))?;
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'),
            "invalid .env name on line {}",
            line_number + 1
        );
        ensure!(
            !values.contains_key(name),
            ".env contains duplicate value {name}"
        );
        values.insert(name.into(), unquote_env(raw)?.into());
    }
    Ok(values)
}

fn write_contract_pair_new(
    intent_path: &Path,
    intent_bytes: &[u8],
    lock_path: &Path,
    lock_bytes: &[u8],
) -> Result<()> {
    ensure!(
        intent_path.parent() == lock_path.parent(),
        "installation contract files must share a directory"
    );
    let parent = intent_path.parent().unwrap_or_else(|| Path::new("."));
    let nonce = Uuid::now_v7().simple().to_string();
    let staged_intent = parent.join(format!(".{INSTALL_MANIFEST}.{nonce}.tmp"));
    let staged_lock = parent.join(format!(".{INSTALL_LOCK}.{nonce}.tmp"));
    let result = (|| -> Result<()> {
        write_new(&staged_intent, intent_bytes)?;
        write_new(&staged_lock, lock_bytes)?;
        fs::hard_link(&staged_intent, intent_path)
            .with_context(|| format!("refusing to overwrite {}", intent_path.display()))?;
        if let Err(error) = fs::hard_link(&staged_lock, lock_path) {
            let _ = fs::remove_file(intent_path);
            return Err(error)
                .with_context(|| format!("refusing to overwrite {}", lock_path.display()));
        }
        fs::remove_file(&staged_intent)?;
        fs::remove_file(&staged_lock)?;
        let directory = OpenOptions::new().read(true).open(parent)?;
        directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged_intent);
        let _ = fs::remove_file(&staged_lock);
    }
    result
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorConfig {
    schema_version: Option<String>,
    semantic: DoctorSemantic,
    server: DoctorServer,
    storage: DoctorStorage,
    admin: DoctorAdmin,
    community: DoctorCommunity,
    deployment: DoctorDeployment,
    redis: DoctorRedis,
    appearance: DoctorAppearance,
    references: DoctorReferences,
    discovery: DoctorDiscovery,
    operations: DoctorOperations,
    features: DoctorFeatures,
    #[serde(skip)]
    cache_signing_key_present: bool,
    #[serde(skip)]
    admin_access_key_phc_present: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorSemantic {
    intent: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct DoctorServer {
    public_url: String,
    article_base_path: String,
    language: String,
    site_id: String,
}

impl Default for DoctorServer {
    fn default() -> Self {
        Self {
            public_url: String::new(),
            article_base_path: "blog".into(),
            language: LanguageChoice::Ko.as_str().into(),
            site_id: String::new(),
        }
    }
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

#[derive(Debug, Deserialize)]
#[serde(default)]
struct DoctorAdmin {
    auth: String,
    session_days: i64,
    external: Option<DoctorExternalAdmin>,
}

impl Default for DoctorAdmin {
    fn default() -> Self {
        Self {
            auth: String::new(),
            session_days: 30,
            external: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorExternalAdmin {
    adapter: Option<String>,
    issuer_url: Option<String>,
    client_id: Option<String>,
    owner_subject: Option<String>,
    label: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorDeployment {
    delivery_only: bool,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct DoctorRedis {
    enabled: bool,
    topology: String,
    url: String,
    sentinel_urls: Vec<String>,
    required: bool,
}

impl Default for DoctorRedis {
    fn default() -> Self {
        Self {
            enabled: true,
            topology: String::new(),
            url: String::new(),
            sentinel_urls: Vec::new(),
            required: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorAppearance {
    custom_css: bool,
    custom_css_file: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct DoctorReferences {
    enabled: bool,
    markdown_file: String,
}

impl Default for DoctorReferences {
    fn default() -> Self {
        Self {
            enabled: true,
            markdown_file: String::new(),
        }
    }
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

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DoctorFeatures {
    external_auth: bool,
    rbac: bool,
    comments: bool,
    seo: bool,
    code_runner: bool,
    ads: bool,
    ai_authorship: bool,
    ai_summary: bool,
    home_curation: bool,
    release_check: bool,
    social_embeds: bool,
}

impl DoctorFeatures {
    fn enabled_aliases(&self) -> Vec<String> {
        [
            ("external-auth", self.external_auth),
            ("rbac", self.rbac),
            ("comments", self.comments),
            ("seo", self.seo),
            ("code-runner", self.code_runner),
            ("ads", self.ads),
            ("ai-authorship", self.ai_authorship),
            ("ai-summary", self.ai_summary),
            ("home-curation", self.home_curation),
            ("release-check", self.release_check),
            ("social-embeds", self.social_embeds),
        ]
        .into_iter()
        .filter(|(_, enabled)| *enabled)
        .map(|(name, _)| name.to_owned())
        .collect()
    }
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
    check_references_contract(&args.config, &parsed, &mut checks);
    check_installation_contract(&args, &parsed, &mut checks);
    if !parsed.redis.enabled {
        checks.push(DoctorCheck {
            id: "redis.connectivity",
            status: CheckStatus::Pass,
            summary: "Redis is deliberately disabled by the installation contract".into(),
            remediation: None,
        });
    } else if args.offline {
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
    if let Some(item) = value("OSB_ARTICLE_BASE_PATH") {
        config.server.article_base_path = item;
    }
    if let Some(item) = value("OSB_LANGUAGE") {
        config.server.language = item;
    }
    if let Some(item) = value("OSB_SITE_ID") {
        config.server.site_id = item;
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
    if let Some(item) = value("OSB_ADMIN_AUTH") {
        config.admin.auth = match item.to_ascii_lowercase().replace('-', "_").as_str() {
            "key" => "access_key".into(),
            "oauth" | "oidc" => "external".into(),
            "off" | "none" => "disabled".into(),
            _ => item.to_ascii_lowercase().replace('-', "_"),
        };
    }
    if let Some(item) = value("OSB_ADMIN_SESSION_DAYS") {
        config.admin.session_days = item
            .parse::<i64>()
            .context("OSB_ADMIN_SESSION_DAYS must be an integer")?;
    }
    if let Some(item) = value("OSB_ADMIN_ACCESS_KEY_PHC_B64") {
        ensure!(
            item.len() <= 8_192,
            "OSB_ADMIN_ACCESS_KEY_PHC_B64 is too large"
        );
        let decoded = BASE64_STANDARD
            .decode(&item)
            .context("OSB_ADMIN_ACCESS_KEY_PHC_B64 must be valid Base64")?;
        let decoded = String::from_utf8(decoded)
            .context("OSB_ADMIN_ACCESS_KEY_PHC_B64 must decode to UTF-8 PHC text")?;
        ensure!(
            (32..=4_096).contains(&decoded.len())
                && decoded.starts_with("$argon2id$")
                && !decoded.chars().any(char::is_control),
            "OSB_ADMIN_ACCESS_KEY_PHC_B64 must decode to a bounded Argon2id PHC credential"
        );
        config.admin_access_key_phc_present = true;
    }
    let external_adapter = value("OSB_EXTERNAL_ADAPTER");
    let external_issuer_url = value("OSB_EXTERNAL_ISSUER_URL");
    let external_client_id = value("OSB_EXTERNAL_CLIENT_ID");
    let external_owner_subject = value("OSB_EXTERNAL_OWNER_SUBJECT");
    let external_client_secret = value("OSB_EXTERNAL_CLIENT_SECRET");
    let external_label = value("OSB_EXTERNAL_LABEL");
    let external_requested = config.admin.external.is_some()
        || external_adapter.is_some()
        || external_issuer_url.is_some()
        || external_client_id.is_some()
        || external_owner_subject.is_some()
        || external_client_secret.is_some();
    if external_requested {
        if let Some(secret) = external_client_secret {
            ensure!(
                (32..=4_096).contains(&secret.len()) && !secret.chars().any(char::is_control),
                "OSB_EXTERNAL_CLIENT_SECRET must be 32-4096 non-control bytes"
            );
        }
        let external = config.admin.external.get_or_insert_with(Default::default);
        if let Some(item) = external_adapter {
            external.adapter = Some(item.to_ascii_lowercase());
        }
        if let Some(item) = external_issuer_url {
            external.issuer_url = Some(item);
        }
        if let Some(item) = external_client_id {
            external.client_id = Some(item);
        }
        if let Some(item) = external_owner_subject {
            external.owner_subject = Some(item);
        }
        if let Some(item) = external_label {
            external.label = Some(item);
        }
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
    if let Some(item) = value("OSB_REDIS_ENABLED") {
        config.redis.enabled = doctor_bool("OSB_REDIS_ENABLED", &item)?;
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
    if let Some(item) = value("OSB_REFERENCES_ENABLED") {
        config.references.enabled = doctor_bool("OSB_REFERENCES_ENABLED", &item)?;
    }
    if let Some(item) = value("OSB_REFERENCES_MARKDOWN_FILE") {
        config.references.markdown_file = item;
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

fn doctor_external_admin_is_valid(external: &DoctorExternalAdmin) -> bool {
    external
        .adapter
        .as_deref()
        .unwrap_or("oidc")
        .eq_ignore_ascii_case("oidc")
        && external
            .issuer_url
            .as_deref()
            .and_then(|raw| Url::parse(raw).ok())
            .is_some_and(|url| external_issuer_url_is_safe(&url))
        && external
            .client_id
            .as_deref()
            .is_some_and(|value| bounded_external_text_is_valid(value, 512))
        && external
            .owner_subject
            .as_deref()
            .is_some_and(|value| bounded_external_text_is_valid(value, 512))
        && bounded_external_text_is_valid(
            external
                .label
                .as_deref()
                .unwrap_or("외부 계정으로 계속하기"),
            80,
        )
}

fn bounded_external_text_is_valid(value: &str, maximum: usize) -> bool {
    value.trim() == value
        && !value.is_empty()
        && value.len() <= maximum
        && !value.chars().any(char::is_control)
}

fn doctor_article_base_path_collision(value: &str, references_enabled: bool) -> Option<&str> {
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
    (RESERVED.contains(&first_segment) || (references_enabled && first_segment == "references"))
        .then_some(first_segment)
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
    let language_ok = matches!(config.server.language.as_str(), "ko" | "en");
    checks.push(DoctorCheck {
        id: "server.language",
        status: if language_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: if language_ok {
            format!("human-facing language is {}", config.server.language)
        } else {
            format!(
                "human-facing language '{}' is unsupported",
                config.server.language
            )
        },
        remediation: (!language_ok)
            .then(|| "set server.language or OSB_LANGUAGE to exactly ko or en".into()),
    });
    let article_collision = doctor_article_base_path_collision(
        &config.server.article_base_path,
        config.references.enabled,
    );
    checks.push(DoctorCheck {
        id: "server.article_base_path",
        status: if article_collision.is_none() {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: match article_collision {
            Some(segment) => format!(
                "effective article base path '{}' starts with reserved route segment {segment}",
                config.server.article_base_path
            ),
            None => format!(
                "effective article base path is {}",
                config.server.article_base_path
            ),
        },
        remediation: article_collision.map(|segment| {
            if segment == "references" {
                "choose another article base, or disable references before using the references segment"
                    .into()
            } else {
                format!("choose an article base whose first segment is not '{segment}'")
            }
        }),
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
    let redis_semantic = if config.redis.enabled {
        config.redis.required
            && matches!(config.redis.topology.as_str(), "standalone" | "sentinel")
            && !config.redis.url.is_empty()
    } else {
        !config.redis.required
    };
    checks.push(DoctorCheck {
        id: "redis.required_hot_path",
        status: if redis_semantic {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: if redis_semantic && config.redis.enabled {
            format!("Redis is required with {} topology", config.redis.topology)
        } else if redis_semantic {
            "Redis is deliberately disabled; SQLite/blobs serve the origin".into()
        } else {
            "Redis enablement, required flag, topology, or URL contradict each other".into()
        },
        remediation: (!redis_semantic)
            .then(|| "set redis.enabled=false with required=false, or enable and configure a required topology/URL".into()),
    });
    checks.push(DoctorCheck {
        id: "redis.cache_integrity",
        status: if !config.redis.enabled || config.cache_signing_key_present {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        summary: if !config.redis.enabled {
            "cache signing is not applicable while Redis is disabled".into()
        } else if config.cache_signing_key_present {
            "application-only cache response signing is deployment-stable".into()
        } else {
            "cache signing will use a process-local key".into()
        },
        remediation: (config.redis.enabled && !config.cache_signing_key_present).then(|| {
            "set a 64-hex OSB_CACHE_SIGNING_KEY; osb bootstrap generates it automatically".into()
        }),
    });
    let sentinel_ok = !config.redis.enabled
        || config.redis.topology != "sentinel"
        || config.redis.sentinel_urls.len() >= 3;
    checks.push(DoctorCheck {
        id: "redis.failure_domains",
        status: if sentinel_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        },
        summary: if !config.redis.enabled {
            "Redis failure domains are not applicable".into()
        } else if config.redis.topology == "sentinel" {
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
    let admin_mode_ok = matches!(
        config.admin.auth.as_str(),
        "access_key" | "external" | "disabled"
    );
    let admin_material_ok = match config.admin.auth.as_str() {
        "access_key" => config.admin_access_key_phc_present && config.admin.external.is_none(),
        "external" => {
            !config.admin_access_key_phc_present
                && config
                    .admin
                    .external
                    .as_ref()
                    .is_some_and(doctor_external_admin_is_valid)
        }
        "disabled" => !config.admin_access_key_phc_present && config.admin.external.is_none(),
        _ => false,
    };
    let admin_delivery_ok = config.semantic.intent != "delivery" || config.admin.auth == "disabled";
    let admin_transport_ok = config.admin.auth == "disabled"
        || Url::parse(&config.server.public_url)
            .is_ok_and(|url| url.scheme() == "https" || is_loopback_url(&url));
    let admin_ok = admin_mode_ok
        && admin_material_ok
        && admin_delivery_ok
        && admin_transport_ok
        && (1..=365).contains(&config.admin.session_days);
    checks.push(DoctorCheck {
        id: "admin.control_plane",
        status: if admin_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        summary: if admin_ok {
            format!(
                "admin auth={} with {} day sessions",
                config.admin.auth, config.admin.session_days
            )
        } else {
            "administrator auth mode and its credential/provider material disagree".into()
        },
        remediation: (!admin_ok).then(|| {
            "choose access_key, external, or disabled and provide only that module's required settings"
                .into()
        }),
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

fn check_installation_contract(
    args: &DoctorArgs,
    config: &DoctorConfig,
    checks: &mut Vec<DoctorCheck>,
) {
    let root = args.config.parent().unwrap_or_else(|| Path::new("."));
    let intent_path = args
        .install_manifest
        .clone()
        .unwrap_or_else(|| root.join(INSTALL_MANIFEST));
    let lock_path = args
        .install_lock
        .clone()
        .unwrap_or_else(|| root.join(INSTALL_LOCK));
    let expected_digest = match installation_tracking(
        args,
        &intent_path,
        config.deployment.delivery_only,
    ) {
        Ok(InstallationTracking::Tracked(digest)) => digest,
        Ok(InstallationTracking::Untracked) => {
            checks.push(DoctorCheck {
                id: "installation.contract",
                status: CheckStatus::Warn,
                summary: "explicitly untracked source/legacy installation; adjacent example contract is not enforced".into(),
                remediation: Some(
                    "bootstrap or adopt this deployment, set OSB_INSTALL_LOCK_DIGEST to its canonical lock digest, then set OSB_ALLOW_UNTRACKED_INSTALLATION=false".into(),
                ),
            });
            return;
        }
        Err(error) => {
            checks.push(DoctorCheck {
                id: "installation.contract",
                status: CheckStatus::Fail,
                summary: error.to_string(),
                remediation: Some(
                    "supply the canonical OSB_INSTALL_LOCK_DIGEST, or temporarily set OSB_ALLOW_UNTRACKED_INSTALLATION=true only for a writable pre-contract source/legacy deployment".into(),
                ),
            });
            return;
        }
    };
    match verify_installation_contract(
        args,
        config,
        &intent_path,
        &lock_path,
        Some(&expected_digest),
    ) {
        Ok(summary) => checks.push(DoctorCheck {
            id: "installation.contract",
            status: CheckStatus::Pass,
            summary,
            remediation: None,
        }),
        Err(error) => checks.push(DoctorCheck {
            id: "installation.contract",
            status: CheckStatus::Fail,
            summary: error.to_string(),
            remediation: Some(
                "restore matching osb.install.toml, osb.lock.json, .env, CSS, and config; never hand-edit lockDigest".into(),
            ),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallationTracking {
    Tracked(String),
    Untracked,
}

fn installation_tracking(
    args: &DoctorArgs,
    intent_path: &Path,
    delivery_only: bool,
) -> Result<InstallationTracking> {
    let default_env = root_for_contract(intent_path).join(".env");
    let env_file = args
        .env_file
        .as_deref()
        .or_else(|| default_env.exists().then_some(default_env.as_path()));
    let mut values = if let Some(path) = env_file {
        ensure_regular_contract_file(path, "deployment environment")?;
        read_environment_file(path)?
    } else {
        BTreeMap::new()
    };
    for name in [
        "OSB_INSTALL_LOCK_DIGEST",
        "OSB_ALLOW_UNTRACKED_INSTALLATION",
    ] {
        if let Some(value) = std::env::var_os(name) {
            values.insert(
                name.into(),
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("{name} must be valid UTF-8"))?,
            );
        }
    }
    installation_tracking_from_values(
        values.get("OSB_INSTALL_LOCK_DIGEST").map(String::as_str),
        values
            .get("OSB_ALLOW_UNTRACKED_INSTALLATION")
            .map(String::as_str),
        delivery_only,
    )
}

fn installation_tracking_from_values(
    lock_digest: Option<&str>,
    allow_untracked: Option<&str>,
    delivery_only: bool,
) -> Result<InstallationTracking> {
    let allow_untracked = match allow_untracked {
        None | Some("") | Some("false") => false,
        Some("true") => true,
        Some(_) => {
            bail!("OSB_ALLOW_UNTRACKED_INSTALLATION must be exactly true or false when non-empty")
        }
    };
    let lock_digest = lock_digest.map(str::trim).filter(|value| !value.is_empty());
    if let Some(lock_digest) = lock_digest {
        ensure!(
            lock_digest.len() == 64
                && lock_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "OSB_INSTALL_LOCK_DIGEST must be one lowercase SHA-256 digest"
        );
        return Ok(InstallationTracking::Tracked(lock_digest.into()));
    }
    ensure!(
        allow_untracked,
        "OSB_INSTALL_LOCK_DIGEST is required; only a pre-contract source/legacy installation may temporarily set OSB_ALLOW_UNTRACKED_INSTALLATION=true"
    );
    ensure!(
        !delivery_only,
        "OSB_ALLOW_UNTRACKED_INSTALLATION cannot bypass the installation lock on a delivery-only deployment"
    );
    Ok(InstallationTracking::Untracked)
}

fn verify_installation_contract(
    args: &DoctorArgs,
    config: &DoctorConfig,
    intent_path: &Path,
    lock_path: &Path,
    expected_digest: Option<&str>,
) -> Result<String> {
    ensure_regular_contract_file(intent_path, INSTALL_MANIFEST)?;
    ensure_regular_contract_file(lock_path, INSTALL_LOCK)?;
    let intent = InstallationIntent::from_toml(&fs::read_to_string(intent_path)?)
        .map_err(anyhow::Error::msg)?;
    let lock =
        InstallationLock::from_json(&fs::read_to_string(lock_path)?).map_err(anyhow::Error::msg)?;
    verify_intent_lock_pair(&intent, &lock)?;
    if let Some(expected_digest) = expected_digest {
        ensure!(
            lock.lock_digest == expected_digest,
            "OSB_INSTALL_LOCK_DIGEST does not match osb.lock.json"
        );
    }
    ensure!(
        intent.site_id == config.server.site_id,
        "site id differs between installation intent and effective config"
    );
    ensure!(
        lock.engine.version == env!("CARGO_PKG_VERSION"),
        "lock engine version {} differs from running CLI {}",
        lock.engine.version,
        env!("CARGO_PKG_VERSION")
    );
    ensure!(
        lock.engine.config_schema_version == CONFIG_SCHEMA,
        "lock config schema differs from this CLI"
    );
    let requested = intent
        .dlcs
        .iter()
        .map(|dlc| (dlc.id.as_str(), (dlc.version.as_str(), dlc.enabled)))
        .collect::<BTreeMap<_, _>>();
    let installed = lock
        .dlcs
        .iter()
        .map(|dlc| {
            (
                dlc.id.as_str(),
                (dlc.requested_version.as_str(), dlc.enabled),
            )
        })
        .collect::<BTreeMap<_, _>>();
    ensure!(
        requested == installed,
        "requested DLC set differs from the exact installed lock"
    );

    let expected_admin = match lock.selection.admin_auth {
        InstallationAdminAuth::AccessKey => "access_key",
        InstallationAdminAuth::External => "external",
        InstallationAdminAuth::Disabled => "disabled",
    };
    ensure!(
        config.admin.auth == expected_admin,
        "effective administrator auth differs from installation lock"
    );
    let effective_cache = if !config.redis.enabled {
        InstallationCache::None
    } else if config.redis.topology == "standalone" {
        InstallationCache::RedisStandalone
    } else if config.redis.topology == "sentinel" {
        InstallationCache::RedisManaged
    } else {
        bail!("effective Redis topology is unknown")
    };
    ensure!(
        effective_cache == lock.selection.cache,
        "effective cache module differs from installation lock"
    );
    let custom_selected = lock.selection.style.kind == InstallationStyleKind::Custom;
    ensure!(
        config.appearance.custom_css == custom_selected,
        "effective custom CSS flag differs from installation lock"
    );
    if custom_selected {
        let file = lock
            .selection
            .style
            .file
            .as_deref()
            .context("custom style lock has no file")?;
        let expected_digest = lock
            .selection
            .style
            .sha256
            .as_deref()
            .context("custom style lock has no digest")?;
        let installed_file = root_for_contract(intent_path).join(file);
        ensure_regular_contract_file(&installed_file, "installed CSS")?;
        let actual = format!("{:x}", Sha256::digest(fs::read(installed_file)?));
        ensure!(
            actual == expected_digest,
            "installed CSS digest differs from lock"
        );
    }

    let structural = structural_environment(args, intent_path)?;
    let require_structural_environment = lock.engine.source != "adopted-v2";
    let required = [
        "OSB_ADMIN_AUTH",
        "OSB_STYLE",
        "OSB_CACHE",
        "OSB_DLC_IDS",
        "OSB_INSTALL_LOCK_DIGEST",
        "OSB_REDIS_ENABLED",
        "OSB_REDIS_TOPOLOGY",
    ];
    if require_structural_environment {
        for name in required {
            ensure!(
                structural.contains_key(name),
                "generated environment does not remember structural value {name}"
            );
        }
    }
    verify_structural_value(
        &structural,
        "OSB_ADMIN_AUTH",
        expected_admin,
        require_structural_environment,
    )?;
    let expected_style = style_environment_value(&lock.selection.style);
    verify_structural_value(
        &structural,
        "OSB_STYLE",
        &expected_style,
        require_structural_environment,
    )?;
    verify_structural_value(
        &structural,
        "OSB_CACHE",
        lock.selection.cache.as_str(),
        require_structural_environment,
    )?;
    let dlc_ids = lock
        .dlcs
        .iter()
        .filter(|dlc| dlc.enabled)
        .map(|dlc| dlc.id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    verify_structural_value(
        &structural,
        "OSB_DLC_IDS",
        &dlc_ids,
        require_structural_environment,
    )?;
    verify_structural_value(
        &structural,
        "OSB_INSTALL_LOCK_DIGEST",
        &lock.lock_digest,
        require_structural_environment,
    )?;
    let redis_enabled = if lock.selection.cache.redis_enabled() {
        "true"
    } else {
        "false"
    };
    let redis_topology = match lock.selection.cache {
        InstallationCache::None | InstallationCache::RedisStandalone => "standalone",
        InstallationCache::RedisManaged => "sentinel",
    };
    verify_structural_value(
        &structural,
        "OSB_REDIS_ENABLED",
        redis_enabled,
        require_structural_environment,
    )?;
    verify_structural_value(
        &structural,
        "OSB_REDIS_TOPOLOGY",
        redis_topology,
        require_structural_environment,
    )?;
    Ok(format!(
        "lock {} · engine {} · {} DLC(s)",
        &lock.lock_digest[..12],
        lock.engine.version,
        lock.dlcs.len()
    ))
}

fn verify_structural_value(
    values: &BTreeMap<String, String>,
    name: &str,
    expected: &str,
    required: bool,
) -> Result<()> {
    match values.get(name) {
        Some(actual) => ensure!(actual == expected, "{name} differs from installation lock"),
        None if required => {
            bail!("generated environment does not remember structural value {name}")
        }
        None => {}
    }
    Ok(())
}

fn ensure_regular_contract_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("missing or unreadable {label}: {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{label} must be a regular file and cannot be a symlink"
    );
    Ok(())
}

fn root_for_contract(intent_path: &Path) -> &Path {
    intent_path.parent().unwrap_or_else(|| Path::new("."))
}

fn structural_environment(
    args: &DoctorArgs,
    intent_path: &Path,
) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    let default_env = root_for_contract(intent_path).join(".env");
    let env_file = args
        .env_file
        .as_deref()
        .or_else(|| default_env.exists().then_some(default_env.as_path()));
    if let Some(path) = env_file {
        ensure_regular_contract_file(path, "deployment environment")?;
        for (line_number, line) in fs::read_to_string(path)?.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (name, raw) = line.split_once('=').with_context(|| {
                format!("invalid generated environment line {}", line_number + 1)
            })?;
            if is_structural_environment_name(name) {
                ensure!(
                    !values.contains_key(name),
                    "duplicate structural environment value {name}"
                );
                values.insert(name.into(), unquote_env(raw)?.into());
            }
        }
    }
    for name in [
        "OSB_ADMIN_AUTH",
        "OSB_STYLE",
        "OSB_CACHE",
        "OSB_DLC_IDS",
        "OSB_INSTALL_LOCK_DIGEST",
        "OSB_REDIS_ENABLED",
        "OSB_REDIS_TOPOLOGY",
    ] {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            values.insert(name.into(), value);
        }
    }
    Ok(values)
}

fn is_structural_environment_name(name: &str) -> bool {
    matches!(
        name,
        "OSB_ADMIN_AUTH"
            | "OSB_STYLE"
            | "OSB_CACHE"
            | "OSB_DLC_IDS"
            | "OSB_INSTALL_LOCK_DIGEST"
            | "OSB_REDIS_ENABLED"
            | "OSB_REDIS_TOPOLOGY"
    )
}

fn unquote_env(value: &str) -> Result<&str> {
    if value.starts_with('\'') || value.starts_with('"') {
        let quote = value.as_bytes()[0];
        ensure!(
            value.len() >= 2 && value.as_bytes()[value.len() - 1] == quote,
            "generated environment contains an unterminated quote"
        );
        Ok(&value[1..value.len() - 1])
    } else {
        Ok(value)
    }
}

fn style_environment_value(style: &InstallationStyle) -> String {
    match style.kind {
        InstallationStyleKind::None => "none".into(),
        InstallationStyleKind::Builtin => {
            format!("builtin:{}", style.id.as_deref().unwrap_or_default())
        }
        InstallationStyleKind::Custom => {
            format!("custom:{}", style.sha256.as_deref().unwrap_or_default())
        }
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReferencesHandoff {
    schema_version: String,
    references_source: ReferencesSourceContract,
}

fn check_references_contract(
    config_path: &Path,
    config: &DoctorConfig,
    checks: &mut Vec<DoctorCheck>,
) {
    if !config.references.enabled {
        checks.push(DoctorCheck {
            id: "references.source_contract",
            status: CheckStatus::Pass,
            summary: "global references are deliberately disabled".into(),
            remediation: None,
        });
        return;
    }
    let root = config_path.parent().unwrap_or_else(|| Path::new("."));
    let handoff_path = root.join("osb.intent.json");
    match fs::symlink_metadata(&handoff_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            checks.push(DoctorCheck {
                id: "references.source_contract",
                status: CheckStatus::Warn,
                summary: "no sibling osb.intent.json; the runtime references source has no pinned path/SHA-256 contract".into(),
                remediation: Some(
                    "bootstrap the deployment to create a references source handoff before relying on doctor for source-integrity verification, or back up the direct config and references source together as deployment controls"
                        .into(),
                ),
            });
            return;
        }
        Err(error) => {
            checks.push(DoctorCheck {
                id: "references.source_contract",
                status: CheckStatus::Fail,
                summary: format!(
                    "failed to inspect deployment handoff {}: {error}",
                    handoff_path.display()
                ),
                remediation: Some(
                    "restore references.md and its matching osb.intent.json from the deployment control backup, or bootstrap again with --references-file"
                        .into(),
                ),
            });
            return;
        }
        Ok(_) => {}
    }
    match verify_references_source_contract(config_path, config) {
        Ok(summary) => checks.push(DoctorCheck {
            id: "references.source_contract",
            status: CheckStatus::Pass,
            summary,
            remediation: None,
        }),
        Err(error) => checks.push(DoctorCheck {
            id: "references.source_contract",
            status: CheckStatus::Fail,
            summary: error.to_string(),
            remediation: Some(
                "restore references.md and its matching osb.intent.json from the deployment control backup, or bootstrap again with --references-file"
                    .into(),
            ),
        }),
    }
}

fn verify_references_source_contract(config_path: &Path, config: &DoctorConfig) -> Result<String> {
    let root = config_path.parent().unwrap_or_else(|| Path::new("."));
    let handoff_path = root.join("osb.intent.json");
    ensure_regular_contract_file(&handoff_path, "deployment handoff")?;
    let handoff_metadata = fs::metadata(&handoff_path)?;
    ensure!(
        handoff_metadata.len() <= 256 * 1024,
        "deployment handoff exceeds 256 KiB"
    );
    let handoff: ReferencesHandoff = serde_json::from_slice(&fs::read(&handoff_path)?)
        .context("osb.intent.json does not contain a valid references source contract")?;
    ensure!(
        handoff.schema_version == INTENT_SCHEMA,
        "osb.intent.json schema is not supported by this CLI"
    );
    let contract_path = Path::new(&handoff.references_source.path);
    ensure!(
        !contract_path.as_os_str().is_empty()
            && !contract_path.is_absolute()
            && contract_path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "references source contract path must be a safe relative path"
    );
    let expected = handoff
        .references_source
        .sha256
        .strip_prefix("sha256:")
        .context("references source contract digest must use sha256:<hex>")?;
    ensure!(
        expected.len() == 64
            && expected
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "references source contract digest must contain 64 lowercase hexadecimal characters"
    );
    ensure!(
        !config.references.markdown_file.is_empty(),
        "enabled references must configure markdown_file for a durable editable source"
    );
    let contracted_file = root.join(contract_path);
    let configured_file = deployment_path(root, &config.references.markdown_file);
    ensure_regular_contract_file(&contracted_file, "contracted references source")?;
    ensure_regular_contract_file(&configured_file, "configured references source")?;
    ensure!(
        contracted_file.canonicalize()? == configured_file.canonicalize()?,
        "configured references source path differs from osb.intent.json"
    );
    let metadata = fs::metadata(&contracted_file)?;
    ensure!(
        metadata.len() <= REFERENCES_MAX_BYTES,
        "contracted references source exceeds 1 MiB"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(&contracted_file)?
        .take(REFERENCES_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= REFERENCES_MAX_BYTES,
        "contracted references source exceeds 1 MiB"
    );
    let actual = format!("{:x}", Sha256::digest(&bytes));
    ensure!(
        actual == expected,
        "references source digest differs from osb.intent.json"
    );
    Ok(format!(
        "{} · sha256:{}",
        configured_file.display(),
        &actual[..12]
    ))
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
            non_interactive: true,
            language: None,
            compose_file: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compose.yaml"),
            ),
            compose_project: None,
            site_id: None,
            content_release: None,
            intent: Intent::Personal,
            public_url: "http://localhost:8787".into(),
            auth: None,
            admin_auth: None,
            external_issuer_url: None,
            external_client_id: None,
            external_owner_subject: None,
            external_label: None,
            registration: Toggle::Disabled,
            comments: None,
            collaboration: Toggle::Disabled,
            custom_css: Some(Toggle::Enabled),
            style: None,
            css_file: None,
            references_file: None,
            references_label: None,
            seo: Toggle::Enabled,
            agent_discovery: Toggle::Enabled,
            redis_topology: Some(RedisTopologyChoice::Managed),
            cache: None,
            dlcs: Vec::new(),
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
        assert!(config.contains("schema_version = \"open-soverign-blog/2\""));
        assert!(config.contains("[admin]\nauth = \"access_key\""));
        assert!(config.contains("language = \"ko\""));
        assert!(config.contains("required = true"));
        assert!(config.contains("managed_backups = true"));
        assert!(config.contains("label = \"레퍼런스\""));
        assert!(config.contains("markdown_file = \"/config/references.md\""));
        assert!(!config.to_ascii_lowercase().contains("password"));
        let environment = fs::read_to_string(root.path().join(".env")).unwrap();
        assert!(environment.contains(&format!(
            "OSB_CONFIG_SOURCE='{}'",
            root.path().join("config.toml").display()
        )));
        assert!(environment.contains(&format!(
            "OSB_HANDOFF_SOURCE='{}'",
            root.path().join("osb.intent.json").display()
        )));
        assert!(environment.contains(&format!(
            "OSB_REFERENCES_SOURCE='{}'",
            root.path().join("references.md").display()
        )));
        assert!(environment.contains("OSB_LANGUAGE=ko\n"));
        assert_eq!(
            fs::read(root.path().join("references.md")).unwrap(),
            include_bytes!("../../../deploy/references.md")
        );
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
        let access_phc = environment
            .lines()
            .find_map(|line| line.strip_prefix("OSB_ADMIN_ACCESS_KEY_PHC_B64="))
            .unwrap();
        assert!(!access_phc.is_empty());
        let decoded = BASE64_STANDARD.decode(access_phc).unwrap();
        assert!(
            String::from_utf8(decoded)
                .unwrap()
                .starts_with("$argon2id$")
        );
        let plaintext_key = fs::read_to_string(root.path().join("admin-access-key.txt")).unwrap();
        assert_eq!(plaintext_key.trim().len(), 43);
        assert!(!environment.contains(plaintext_key.trim()));
        assert_eq!(
            fs::read_to_string(root.path().join(".gitignore")).unwrap(),
            GENERATED_GITIGNORE
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in [".env", "admin-access-key.txt"] {
                assert_eq!(
                    fs::metadata(root.path().join(name))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("osb.intent.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["intent"], "personal");
        assert_eq!(manifest["data"]["redisRequired"], true);
        assert_eq!(manifest["referencesSource"]["path"], REFERENCES_FILE);
        assert_eq!(
            manifest["referencesSource"]["sha256"],
            format!(
                "sha256:{:x}",
                Sha256::digest(include_bytes!("../../../deploy/references.md"))
            )
        );
        assert!(
            manifest["nextCommands"][0]
                .as_str()
                .unwrap()
                .contains("--env-file")
        );
        let intent = InstallationIntent::from_toml(
            &fs::read_to_string(root.path().join(INSTALL_MANIFEST)).unwrap(),
        )
        .unwrap();
        let lock = InstallationLock::from_json(
            &fs::read_to_string(root.path().join(INSTALL_LOCK)).unwrap(),
        )
        .unwrap();
        verify_intent_lock_pair(&intent, &lock).unwrap();
        assert_eq!(lock.engine.database_schema_version, DATABASE_SCHEMA_VERSION);
        assert_eq!(lock.dlcs.len(), RECOMMENDED_PERSONAL_DLCS.len());
        assert!(RECOMMENDED_PERSONAL_DLCS.iter().all(|alias| {
            let id = find_official_dlc(alias).unwrap().id;
            lock.dlcs.iter().any(|dlc| dlc.id == id && dlc.enabled)
        }));
        assert!(environment.contains(&format!("OSB_INSTALL_LOCK_DIGEST={}", lock.lock_digest)));
        assert!(environment.contains("OSB_ALLOW_UNTRACKED_INSTALLATION=false\n"));
        assert!(environment.contains(&format!(
            "OSB_DATA_VOLUME=osb-data-{}",
            lock.installation_id.replace('-', "")
        )));
    }

    #[test]
    fn english_language_generates_english_defaults_and_starter_references() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.language = Some(LanguageChoice::En);
        args.admin_auth = Some(AdminAuthChoice::External);
        args.public_url = "https://blog.example".into();
        args.external_issuer_url = Some("https://identity.example/realm/blog".into());
        args.external_client_id = Some("open-soverign-blog".into());
        args.external_owner_subject = Some("stable-owner-subject".into());

        bootstrap(args).unwrap();

        let config = fs::read_to_string(root.path().join("config.toml")).unwrap();
        assert!(config.contains("language = \"en\""));
        assert!(config.contains("label = \"Continue with external account\""));
        assert!(config.contains("[references]\nenabled = true\nlabel = \"References\""));
        let environment = fs::read_to_string(root.path().join(".env")).unwrap();
        assert!(environment.contains("OSB_LANGUAGE=en\n"));
        assert_eq!(
            fs::read(root.path().join(REFERENCES_FILE)).unwrap(),
            include_bytes!("../../../deploy/references.en.md")
        );
    }

    #[test]
    fn bootstrap_rejects_references_labels_over_the_runtime_limit() {
        assert!(validate_cli_character_text("--references-label", &"가".repeat(40), 40).is_ok());
        assert!(validate_cli_character_text("--references-label", &"가".repeat(41), 40).is_err());

        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.references_label = Some("x".repeat(41));

        let error = bootstrap(args).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("--references-label must be 1-40")
        );
        assert!(!root.path().join("config.toml").exists());
        assert!(!root.path().join(".env").exists());
    }

    #[test]
    fn references_source_contract_detects_changed_and_missing_policy_files() {
        let root = tempdir().unwrap();
        let source = root.path().join("operator-references.md");
        fs::write(&source, "## 출처\n\n운영자 정책\n").unwrap();
        let deployment = root.path().join("deployment");
        let mut args = personal(deployment.clone());
        args.language = Some(LanguageChoice::En);
        args.references_file = Some(source);
        args.references_label = Some("Source policy".into());
        bootstrap(args).unwrap();

        let generated_config = fs::read_to_string(deployment.join("config.toml")).unwrap();
        assert!(generated_config.contains("language = \"en\""));
        assert!(generated_config.contains("label = \"Source policy\""));
        assert_eq!(
            fs::read_to_string(deployment.join(REFERENCES_FILE)).unwrap(),
            "## 출처\n\n운영자 정책\n"
        );
        let config: DoctorConfig = toml::from_str(&generated_config).unwrap();
        let config_path = deployment.join("config.toml");
        assert!(verify_references_source_contract(&config_path, &config).is_ok());

        fs::write(deployment.join(REFERENCES_FILE), "## changed\n").unwrap();
        let changed = verify_references_source_contract(&config_path, &config).unwrap_err();
        assert!(changed.to_string().contains("digest differs"));

        fs::remove_file(deployment.join(REFERENCES_FILE)).unwrap();
        let missing = verify_references_source_contract(&config_path, &config).unwrap_err();
        assert!(missing.to_string().contains("missing or unreadable"));
    }

    #[test]
    fn references_source_contract_warns_without_a_handoff_for_direct_runtime_config() {
        let root = tempdir().unwrap();
        let config_path = root.path().join("config.toml");
        let direct_file = root.path().join("references.md");
        fs::write(&direct_file, "## Direct file\n").unwrap();
        let configurations = [
            "[references]\nenabled = true\n".to_owned(),
            "[references]\nenabled = true\nmarkdown = \"Inline policy\"\n".to_owned(),
            format!(
                "[references]\nenabled = true\nmarkdown_file = {:?}\n",
                direct_file.display().to_string()
            ),
        ];

        for source in configurations {
            let config: DoctorConfig = toml::from_str(&source).unwrap();
            let mut checks = Vec::new();
            check_references_contract(&config_path, &config, &mut checks);
            assert_eq!(checks.len(), 1);
            assert_eq!(checks[0].status, CheckStatus::Warn);
            assert!(checks[0].summary.contains("no sibling osb.intent.json"));
            assert!(
                checks[0]
                    .remediation
                    .as_deref()
                    .is_some_and(|value| value.contains("deployment controls"))
            );
        }
    }

    #[test]
    fn references_source_contract_fails_when_an_existing_handoff_lacks_the_contract() {
        let root = tempdir().unwrap();
        let config_path = root.path().join("config.toml");
        fs::write(
            root.path().join("osb.intent.json"),
            format!(r#"{{"schemaVersion":"{INTENT_SCHEMA}"}}"#),
        )
        .unwrap();
        let config: DoctorConfig =
            toml::from_str("[references]\nenabled = true\nmarkdown = \"Inline policy\"\n").unwrap();

        let mut checks = Vec::new();
        check_references_contract(&config_path, &config, &mut checks);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Fail);
        assert!(
            checks[0]
                .summary
                .contains("valid references source contract")
        );
    }

    #[test]
    fn bootstrap_supports_a_zero_redis_builtin_style_profile() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.admin_auth = Some(AdminAuthChoice::Disabled);
        args.redis_topology = None;
        args.cache = Some(CacheChoice::None);
        args.custom_css = None;
        args.style = Some("builtin:forest".into());
        args.seo = Toggle::Disabled;
        args.dlcs = vec!["none".into()];
        bootstrap(args).unwrap();

        let config = fs::read_to_string(root.path().join("config.toml")).unwrap();
        assert!(config.contains("[redis]\nenabled = false"));
        assert!(config.contains("required = false"));
        assert!(config.contains("custom_css = false"));
        let environment = fs::read_to_string(root.path().join(".env")).unwrap();
        assert!(environment.contains("OSB_STYLE=builtin:forest\n"));
        assert!(environment.contains("OSB_CACHE=none\n"));
        assert!(environment.contains("OSB_DLC_IDS=\n"));
        assert!(environment.contains("OSB_REDIS_ENABLED=false\n"));
        assert!(environment.contains("OSB_REDIS_PASSWORD=\n"));
        assert!(environment.contains("OSB_CACHE_SIGNING_KEY=\n"));
        let handoff: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("osb.intent.json")).unwrap())
                .unwrap();
        assert!(
            !handoff["nextCommands"][0]
                .as_str()
                .unwrap()
                .contains("--profile redis-")
        );
        let lock = InstallationLock::from_json(
            &fs::read_to_string(root.path().join(INSTALL_LOCK)).unwrap(),
        )
        .unwrap();
        assert_eq!(lock.selection.cache, InstallationCache::None);
        assert_eq!(lock.selection.style.kind, InstallationStyleKind::Builtin);
        assert_eq!(lock.selection.style.id.as_deref(), Some("forest"));
        assert!(lock.dlcs.is_empty());
    }

    #[test]
    fn bootstrap_uses_one_explicit_compose_project_in_every_handoff() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.compose_project = Some("eff0rtchung".into());
        bootstrap(args).unwrap();

        let environment = fs::read_to_string(root.path().join(".env")).unwrap();
        assert!(environment.starts_with("COMPOSE_PROJECT_NAME=eff0rtchung\n"));
        let handoff: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("osb.intent.json")).unwrap())
                .unwrap();
        assert_eq!(handoff["composeProject"], "eff0rtchung");
        let deployment_id = Uuid::parse_str(handoff["deploymentId"].as_str().unwrap()).unwrap();
        let expected_volume = format!("osb-data-{}", deployment_id.simple());
        assert!(environment.contains(&format!("OSB_DATA_VOLUME={expected_volume}\n")));
        assert_ne!(expected_volume, "osb-data-eff0rtchung");
        for command in handoff["nextCommands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|command| command.starts_with("docker compose"))
        {
            assert!(command.contains(" -p eff0rtchung "), "{command}");
        }

        let invalid_root = tempdir().unwrap();
        let mut invalid = personal(invalid_root.path().to_owned());
        invalid.compose_project = Some("Invalid Project".into());
        let error = bootstrap(invalid).unwrap_err();
        assert!(error.to_string().contains("--compose-project"));
        assert!(!invalid_root.path().join("config.toml").exists());
    }

    #[test]
    fn installation_verify_rejects_a_rehashed_forged_bundled_manifest_digest() {
        let intent =
            InstallationIntent::from_toml(include_str!("../../../osb.install.example.toml"))
                .unwrap();
        let mut lock =
            InstallationLock::from_json(include_str!("../../../osb.lock.example.json")).unwrap();
        lock.dlcs[0].manifest_sha256 = "f".repeat(64);
        lock.refresh_digest().unwrap();
        lock.validate().unwrap();

        let error = verify_intent_lock_pair(&intent, &lock).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("manifest bytes compiled into this CLI")
        );
    }

    #[test]
    fn explicit_dlc_none_disables_all_recommended_defaults() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.dlcs = vec!["none".into()];
        bootstrap(args).unwrap();
        let lock = InstallationLock::from_json(
            &fs::read_to_string(root.path().join(INSTALL_LOCK)).unwrap(),
        )
        .unwrap();
        assert!(lock.dlcs.is_empty());
        let environment = fs::read_to_string(root.path().join(".env")).unwrap();
        assert!(environment.contains("OSB_DLC_IDS=\n"));
        assert!(environment.contains("OSB_FEATURES=none\n"));
        let config = fs::read_to_string(root.path().join("config.toml")).unwrap();
        assert!(config.contains("no_index = true"));
        assert!(config.contains("seo = false"));
        let handoff: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("osb.intent.json")).unwrap())
                .unwrap();
        assert!(handoff["features"]["seo"].is_null());
        assert!(handoff["installationLockDigest"].is_null());
        assert!(handoff["installedDlcs"].is_null());
    }

    #[test]
    fn seo_disabled_excludes_the_recommended_seo_dlc_everywhere() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.seo = Toggle::Disabled;
        bootstrap(args).unwrap();
        let lock = InstallationLock::from_json(
            &fs::read_to_string(root.path().join(INSTALL_LOCK)).unwrap(),
        )
        .unwrap();
        assert_eq!(lock.dlcs.len(), RECOMMENDED_PERSONAL_DLCS.len() - 1);
        assert!(
            lock.dlcs
                .iter()
                .all(|dlc| dlc.id != find_official_dlc("seo").unwrap().id)
        );
        let config = fs::read_to_string(root.path().join("config.toml")).unwrap();
        assert!(config.contains("no_index = true"));
        assert!(config.contains("seo = false"));
        let handoff: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("osb.intent.json")).unwrap())
                .unwrap();
        assert!(handoff["features"]["seo"].is_null());
    }

    #[test]
    fn explicit_dlc_none_rejects_required_runtime_modules() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.dlcs = vec!["none".into()];
        args.comments = Some(Toggle::Enabled);
        let error = bootstrap(args).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conflicts with enabled comments")
        );
        assert!(!root.path().join(INSTALL_LOCK).exists());
    }

    #[test]
    fn bootstrap_rejects_oauth_only_before_creating_deployment_files() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.auth = Some(AuthChoice::Oauth);

        let error = bootstrap(args).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--auth oauth is not operational")
        );
        assert!(error.to_string().contains("local-and-oauth"));
        for name in [
            ".gitignore",
            "config.toml",
            ".env",
            "admin-access-key.txt",
            ".osb-backups",
            INSTALL_MANIFEST,
            INSTALL_LOCK,
        ] {
            assert!(!root.path().join(name).exists(), "unexpected {name}");
        }
    }

    #[test]
    fn bootstrap_pins_custom_css_and_repeated_official_dlcs() {
        let root = tempdir().unwrap();
        let css = root.path().join("operator.css");
        fs::write(&css, b":root { --accent: #123456; }\n").unwrap();
        let deployment = root.path().join("deployment");
        let mut args = personal(deployment.clone());
        args.custom_css = None;
        args.css_file = Some(css.clone());
        args.seo = Toggle::Disabled;
        args.dlcs = vec![
            "ai-authorship@>=0.1.0, <0.2.0".into(),
            "home-curation".into(),
            "release-check".into(),
            "social-embeds".into(),
        ];
        bootstrap(args).unwrap();
        assert_eq!(
            fs::read(deployment.join("custom.css")).unwrap(),
            fs::read(&css).unwrap()
        );
        let lock = InstallationLock::from_json(
            &fs::read_to_string(deployment.join(INSTALL_LOCK)).unwrap(),
        )
        .unwrap();
        assert_eq!(lock.dlcs.len(), 4);
        assert!(lock.dlcs.windows(2).all(|pair| pair[0].id < pair[1].id));
        assert_eq!(
            lock.dlcs
                .iter()
                .find(|dlc| dlc.id.ends_with("ai-authorship"))
                .unwrap()
                .requested_version,
            ">=0.1.0, <0.2.0"
        );
        assert_eq!(
            lock.selection.style.sha256.as_deref(),
            Some(format!("{:x}", Sha256::digest(fs::read(css).unwrap())).as_str())
        );
    }

    #[test]
    fn interactive_bootstrap_prompts_only_for_unspecified_structural_choices() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.non_interactive = false;
        args.language = Some(LanguageChoice::En);
        args.admin_auth = None;
        args.custom_css = None;
        args.style = None;
        args.css_file = None;
        args.redis_topology = None;
        args.cache = None;
        args.dlcs.clear();
        let input = b"disabled\nbuiltin:terminal\nnone\nai-authorship,social-embeds\n";
        let mut reader = io::Cursor::new(input);
        let mut output = Vec::new();
        resolve_prompted_args_with(&mut args, true, &mut reader, &mut output).unwrap();
        assert_eq!(args.admin_auth, Some(AdminAuthChoice::Disabled));
        assert_eq!(args.style.as_deref(), Some("builtin:terminal"));
        assert_eq!(args.cache, Some(CacheChoice::None));
        assert_eq!(args.dlcs, ["ai-authorship", "social-embeds"]);
        let prompts = String::from_utf8(output).unwrap();
        assert!(prompts.contains("Administrator auth"));
        assert!(prompts.contains("Style"));
        assert!(prompts.contains("Cache"));
        assert!(prompts.contains("Optional DLC"));
        assert!(!prompts.contains("Language / 언어"));
        assert!(!prompts.to_ascii_lowercase().contains("secret"));
    }

    #[test]
    fn interactive_language_prompt_is_first_and_localizes_following_prompts() {
        let root = tempdir().unwrap();
        let mut english = personal(root.path().join("english"));
        english.non_interactive = false;
        english.dlcs = vec!["none".into()];
        let mut reader = io::Cursor::new(b"en\ndisabled\n");
        let mut output = Vec::new();
        resolve_prompted_args_with(&mut english, true, &mut reader, &mut output).unwrap();
        let prompts = String::from_utf8(output).unwrap();
        assert!(prompts.starts_with("Language / 언어 (ko=한국어, en=English) [ko]: "));
        assert!(prompts.contains("Administrator auth"));
        assert_eq!(english.language, Some(LanguageChoice::En));
        assert_eq!(
            english.external_label.as_deref(),
            Some("Continue with external account")
        );
        assert_eq!(english.references_label.as_deref(), Some("References"));

        let mut korean = personal(root.path().join("korean"));
        korean.non_interactive = false;
        korean.dlcs = vec!["none".into()];
        let mut reader = io::Cursor::new(b"\ndisabled\n");
        let mut output = Vec::new();
        resolve_prompted_args_with(&mut korean, true, &mut reader, &mut output).unwrap();
        let prompts = String::from_utf8(output).unwrap();
        assert!(prompts.starts_with("Language / 언어 (ko=한국어, en=English) [ko]: "));
        assert!(prompts.contains("관리자 인증"));
        assert_eq!(korean.language, Some(LanguageChoice::Ko));
        assert_eq!(
            korean.external_label.as_deref(),
            Some("외부 계정으로 계속하기")
        );
        assert_eq!(korean.references_label.as_deref(), Some("레퍼런스"));

        let mut invalid = personal(root.path().join("invalid"));
        invalid.non_interactive = false;
        let error = resolve_prompted_args_with(
            &mut invalid,
            true,
            &mut io::Cursor::new(b"fr\n"),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("must be ko or en"));
        assert!(!invalid.directory.exists());
    }

    #[test]
    fn interactive_bootstrap_defaults_to_the_recommended_personal_dlc_set() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.non_interactive = false;
        args.language = Some(LanguageChoice::En);
        args.admin_auth = None;
        args.custom_css = None;
        args.style = None;
        args.css_file = None;
        args.redis_topology = None;
        args.cache = None;
        args.dlcs.clear();
        let mut reader = io::Cursor::new(b"\n\n\n\n");
        let mut output = Vec::new();
        resolve_prompted_args_with(&mut args, true, &mut reader, &mut output).unwrap();
        assert_eq!(args.dlcs, RECOMMENDED_PERSONAL_DLCS);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("seo,home-curation,ai-authorship,social-embeds,release-check")
        );

        let mut no_seo = personal(root.path().join("no-seo"));
        no_seo.non_interactive = false;
        no_seo.language = Some(LanguageChoice::En);
        no_seo.admin_auth = None;
        no_seo.custom_css = None;
        no_seo.style = None;
        no_seo.css_file = None;
        no_seo.redis_topology = None;
        no_seo.cache = None;
        no_seo.dlcs.clear();
        no_seo.seo = Toggle::Disabled;
        let mut reader = io::Cursor::new(b"\n\n\n\n");
        resolve_prompted_args_with(&mut no_seo, true, &mut reader, &mut Vec::new()).unwrap();
        assert_eq!(
            no_seo.dlcs,
            [
                "home-curation",
                "ai-authorship",
                "social-embeds",
                "release-check"
            ]
        );
    }

    #[test]
    fn adopt_creates_only_a_valid_contract_pair_for_a_legacy_v2_deployment() {
        let root = tempdir().unwrap();
        bootstrap(personal(root.path().to_owned())).unwrap();
        fs::remove_file(root.path().join(INSTALL_MANIFEST)).unwrap();
        fs::remove_file(root.path().join(INSTALL_LOCK)).unwrap();
        let environment = fs::read_to_string(root.path().join(".env")).unwrap();
        let environment = environment
            .lines()
            .filter(|line| {
                !line.starts_with("OSB_STYLE=")
                    && !line.starts_with("OSB_CACHE=")
                    && !line.starts_with("OSB_DLC_IDS=")
                    && !line.starts_with("OSB_INSTALL_LOCK_DIGEST=")
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(root.path().join(".env"), &environment).unwrap();
        let config_before = fs::read(root.path().join("config.toml")).unwrap();
        let env_before = fs::read(root.path().join(".env")).unwrap();
        let css_before = fs::read(root.path().join("custom.css")).unwrap();

        adopt_installation(root.path()).unwrap();
        assert_eq!(
            fs::read(root.path().join("config.toml")).unwrap(),
            config_before
        );
        assert_eq!(fs::read(root.path().join(".env")).unwrap(), env_before);
        assert_eq!(
            fs::read(root.path().join("custom.css")).unwrap(),
            css_before
        );
        let (intent, lock) = read_installation_pair(
            &root.path().join(INSTALL_MANIFEST),
            &root.path().join(INSTALL_LOCK),
        )
        .unwrap();
        verify_intent_lock_pair(&intent, &lock).unwrap();
        assert_eq!(lock.engine.source, "adopted-v2");
        assert_eq!(lock.engine.database_schema_version, DATABASE_SCHEMA_VERSION);
        assert_eq!(lock.dlcs.len(), RECOMMENDED_PERSONAL_DLCS.len());

        let mut effective: DoctorConfig =
            toml::from_str(&fs::read_to_string(root.path().join("config.toml")).unwrap()).unwrap();
        let values = read_environment_file(&root.path().join(".env")).unwrap();
        apply_environment_overrides_with(&mut effective, |name| values.get(name).cloned()).unwrap();
        let doctor_args = DoctorArgs {
            config: root.path().join("config.toml"),
            install_manifest: None,
            install_lock: None,
            env_file: Some(root.path().join(".env")),
            offline: true,
            json: false,
        };
        verify_installation_contract(
            &doctor_args,
            &effective,
            &root.path().join(INSTALL_MANIFEST),
            &root.path().join(INSTALL_LOCK),
            None,
        )
        .unwrap();

        let intent_bytes = fs::read(root.path().join(INSTALL_MANIFEST)).unwrap();
        let error = adopt_installation(root.path()).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            fs::read(root.path().join(INSTALL_MANIFEST)).unwrap(),
            intent_bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn record_engine_upgrade_is_target_bound_atomic_and_mode_preserving() {
        let root = tempdir().unwrap();
        let selection = InstallationSelection {
            admin_auth: InstallationAdminAuth::Disabled,
            style: InstallationStyle {
                kind: InstallationStyleKind::None,
                id: None,
                file: None,
                sha256: None,
            },
            cache: InstallationCache::None,
        };
        let intent = InstallationIntent {
            schema_version: INSTALL_INTENT_SCHEMA_VERSION.into(),
            installation_id: "018f0000-0000-7000-8000-000000000001".into(),
            site_id: "018f0000-0000-7000-8000-000000000002".into(),
            created_with: env!("CARGO_PKG_VERSION").into(),
            selection: selection.clone(),
            dlcs: Vec::new(),
        };
        let mut lock = InstallationLock {
            schema_version: INSTALL_LOCK_SCHEMA_VERSION.into(),
            installation_id: intent.installation_id.clone(),
            engine: LockedEngine {
                version: "0.0.1".into(),
                config_schema_version: "open-soverign-blog/1".into(),
                database_schema_version: 1,
                plugin_api: PLUGIN_API_VERSION.into(),
                source: "legacy-release".into(),
                artifact_sha256: None,
            },
            selection,
            dlcs: Vec::new(),
            retained_dlcs: Vec::new(),
            history: Vec::new(),
            lock_digest: String::new(),
        };
        lock.refresh_digest().unwrap();
        let intent_path = root.path().join(INSTALL_MANIFEST);
        let lock_path = root.path().join(INSTALL_LOCK);
        fs::write(&intent_path, intent.to_toml_pretty().unwrap()).unwrap();
        fs::write(&lock_path, lock.to_pretty_json().unwrap()).unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o640)).unwrap();
        let original = fs::read(&lock_path).unwrap();
        let error = record_engine_upgrade(
            &intent_path,
            &lock_path,
            "0.0.1",
            "9.9.9",
            "candidate".into(),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("target CLI"));
        assert_eq!(fs::read(&lock_path).unwrap(), original);

        record_engine_upgrade(
            &intent_path,
            &lock_path,
            "0.0.1",
            env!("CARGO_PKG_VERSION"),
            "candidate-release".into(),
            Some("a".repeat(64)),
        )
        .unwrap();
        let updated =
            InstallationLock::from_json(&fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(updated.engine.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(updated.engine.config_schema_version, CONFIG_SCHEMA);
        assert_eq!(
            updated.engine.database_schema_version,
            DATABASE_SCHEMA_VERSION
        );
        assert_eq!(updated.engine.plugin_api, PLUGIN_API_VERSION);
        assert_eq!(updated.engine.source, "candidate-release");
        assert_eq!(
            fs::metadata(lock_path).unwrap().permissions().mode() & 0o777,
            0o640
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
    fn bootstrap_preserves_an_existing_safe_operator_gitignore() {
        let root = tempdir().unwrap();
        let existing = "!.*\noperator-rule\n/.env\n/admin-access-key.txt\n/.osb-backups/\n/.osb-update/\n!README.md\n";
        fs::write(root.path().join(".gitignore"), existing).unwrap();
        bootstrap(personal(root.path().to_owned())).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join(".gitignore")).unwrap(),
            existing
        );
    }

    #[test]
    fn bootstrap_fails_before_creating_secrets_when_gitignore_is_unsafe() {
        for (existing, missing) in [
            (
                ".env\n.osb-backups/\n.osb-update/\noperator-rule\n",
                "admin-access-key.txt",
            ),
            (
                "admin-access-key.txt\n.osb-backups/\n.osb-update/\noperator-rule\n",
                ".env",
            ),
            (
                ".env\nadmin-access-key.txt\n.osb-backups/\noperator-rule\n",
                ".osb-update/",
            ),
            (
                ".env\nadmin-access-key.txt\n.osb-update/\noperator-rule\n",
                ".osb-backups/",
            ),
        ] {
            let root = tempdir().unwrap();
            fs::write(root.path().join(".gitignore"), existing).unwrap();
            let error = bootstrap(personal(root.path().to_owned())).unwrap_err();
            assert!(error.to_string().contains(missing));
            for name in [
                "config.toml",
                ".env",
                "admin-access-key.txt",
                "custom.css",
                "osb.intent.json",
                ".osb-backups",
            ] {
                assert!(!root.path().join(name).exists(), "unexpected {name}");
            }
        }
    }

    #[test]
    fn bootstrap_rejects_later_gitignore_negations_that_restore_protected_paths() {
        for negation in [
            "!/.env",
            "!*.txt",
            "!/.osb-backups/**",
            "!/.osb-update/private.env",
            "!.*",
        ] {
            let root = tempdir().unwrap();
            let existing =
                format!(".env\nadmin-access-key.txt\n.osb-backups/\n.osb-update/\n{negation}\n");
            fs::write(root.path().join(".gitignore"), existing).unwrap();
            let error = bootstrap(personal(root.path().to_owned())).unwrap_err();
            assert!(
                error.to_string().contains("later negation"),
                "unexpected error for {negation}: {error:#}"
            );
            assert!(!root.path().join(".env").exists());
            assert!(!root.path().join("admin-access-key.txt").exists());
            assert!(!root.path().join(".osb-backups").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_rejects_a_symlinked_gitignore_before_creating_secrets() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let target = root.path().join("operator-gitignore");
        fs::write(&target, GENERATED_GITIGNORE).unwrap();
        symlink(&target, root.path().join(".gitignore")).unwrap();
        let error = bootstrap(personal(root.path().to_owned())).unwrap_err();
        assert!(error.to_string().contains("regular file"));
        assert!(!root.path().join(".env").exists());
        assert!(!root.path().join("admin-access-key.txt").exists());
    }

    #[test]
    fn administrator_auth_profiles_are_explicit_and_non_overlapping() {
        let disabled_root = tempdir().unwrap();
        let mut disabled = personal(disabled_root.path().to_owned());
        disabled.admin_auth = Some(AdminAuthChoice::Disabled);
        bootstrap(disabled).unwrap();
        let disabled_config = fs::read_to_string(disabled_root.path().join("config.toml")).unwrap();
        assert!(disabled_config.contains("[admin]\nauth = \"disabled\""));
        assert!(!disabled_root.path().join("admin-access-key.txt").exists());
        let disabled_env = fs::read_to_string(disabled_root.path().join(".env")).unwrap();
        assert!(disabled_env.contains("OSB_ADMIN_ACCESS_KEY_PHC_B64=\n"));

        let external_root = tempdir().unwrap();
        let mut external = personal(external_root.path().to_owned());
        external.admin_auth = Some(AdminAuthChoice::External);
        external.public_url = "https://blog.example".into();
        external.external_issuer_url = Some("https://identity.example/realm/blog".into());
        external.external_client_id = Some("open-soverign-blog".into());
        external.external_owner_subject = Some("stable-owner-subject".into());
        external.external_label = Some("Identity login".into());
        bootstrap(external).unwrap();
        let external_config = fs::read_to_string(external_root.path().join("config.toml")).unwrap();
        assert!(external_config.contains("auth = \"external\""));
        assert!(external_config.contains("[admin.external]"));
        assert!(external_config.contains("owner_subject = \"stable-owner-subject\""));
        assert!(external_config.contains("label = \"Identity login\""));
        assert!(!external_root.path().join("admin-access-key.txt").exists());
    }

    #[test]
    fn external_options_are_rejected_outside_the_external_profile() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.external_issuer_url = Some("https://identity.example".into());
        assert!(bootstrap(args).is_err());
        assert!(!root.path().join("config.toml").exists());
    }

    #[test]
    fn external_issuer_rejects_embedded_credentials_before_writing_files() {
        let root = tempdir().unwrap();
        let mut args = personal(root.path().to_owned());
        args.admin_auth = Some(AdminAuthChoice::External);
        args.public_url = "https://blog.example".into();
        args.external_issuer_url = Some("https://user:secret@identity.example/realm/blog".into());
        args.external_client_id = Some("open-soverign-blog".into());
        args.external_owner_subject = Some("stable-owner-subject".into());
        let error = bootstrap(args).unwrap_err();
        assert!(error.to_string().contains("without credentials"));
        assert!(!root.path().join("config.toml").exists());
        assert!(!root.path().join(".env").exists());
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
        args.custom_css = Some(Toggle::Disabled);
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
                [admin]
                auth = "disabled"
                session_days = 30
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
            ("OSB_ARTICLE_BASE_PATH", "writing/articles"),
            ("OSB_LANGUAGE", "en"),
            ("OSB_DATABASE", "/tmp/osb/blog.sqlite3"),
            ("OSB_BLOB_DIRECTORY", "/tmp/osb/blobs"),
            ("OSB_REDIS_TOPOLOGY", "standalone"),
            ("OSB_REDIS_URL", "redis://127.0.0.1:6389/"),
            ("OSB_REDIS_SENTINELS", ""),
            ("OSB_COMMENTS", "yes"),
            ("OSB_COLLABORATION", "on"),
            ("OSB_ADMIN_AUTH", "external"),
            ("OSB_ADMIN_SESSION_DAYS", "45"),
            ("OSB_EXTERNAL_ADAPTER", "OIDC"),
            ("OSB_EXTERNAL_ISSUER_URL", "https://identity.example/realm"),
            ("OSB_EXTERNAL_CLIENT_ID", "blog-client"),
            ("OSB_EXTERNAL_OWNER_SUBJECT", "stable-owner-subject"),
            ("OSB_EXTERNAL_LABEL", "Identity login"),
            (
                "OSB_EXTERNAL_CLIENT_SECRET",
                "0123456789abcdef0123456789abcdef",
            ),
        ]);
        apply_environment_overrides_with(&mut config, |name| {
            overrides.get(name).map(|value| (*value).to_owned())
        })
        .unwrap();
        assert_eq!(config.server.public_url, "http://127.0.0.1:18787/base");
        assert_eq!(config.server.article_base_path, "writing/articles");
        assert_eq!(config.server.language, "en");
        assert_eq!(config.storage.database, "/tmp/osb/blog.sqlite3");
        assert_eq!(config.storage.blob_directory, "/tmp/osb/blobs");
        assert_eq!(config.redis.topology, "standalone");
        assert_eq!(config.redis.url, "redis://127.0.0.1:6389/");
        // Empty environment values are ignored, matching RuntimeConfig.
        assert_eq!(config.redis.sentinel_urls, ["redis://sentinel:26379/"]);
        assert!(config.community.comments);
        assert!(config.community.collaboration);
        assert_eq!(config.admin.auth, "external");
        assert_eq!(config.admin.session_days, 45);
        let external = config.admin.external.as_ref().unwrap();
        assert_eq!(external.adapter.as_deref(), Some("oidc"));
        assert_eq!(
            external.issuer_url.as_deref(),
            Some("https://identity.example/realm")
        );
        assert_eq!(external.client_id.as_deref(), Some("blog-client"));
        assert_eq!(
            external.owner_subject.as_deref(),
            Some("stable-owner-subject")
        );
        assert_eq!(external.label.as_deref(), Some("Identity login"));
        let mut checks = Vec::new();
        check_semantics(&config, &mut checks);
        assert_eq!(
            checks
                .iter()
                .find(|check| check.id == "admin.control_plane")
                .unwrap()
                .status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn doctor_accepts_only_supported_effective_languages() {
        fn language_check(config: &DoctorConfig) -> CheckStatus {
            let mut checks = Vec::new();
            check_semantics(config, &mut checks);
            checks
                .iter()
                .find(|check| check.id == "server.language")
                .expect("language check must exist")
                .status
        }

        for language in ["ko", "en"] {
            let config: DoctorConfig =
                toml::from_str(&format!("[server]\nlanguage = \"{language}\"\n")).unwrap();
            assert_eq!(language_check(&config), CheckStatus::Pass);
        }

        let missing: DoctorConfig = toml::from_str("").unwrap();
        assert_eq!(missing.server.language, "ko");
        assert_eq!(language_check(&missing), CheckStatus::Pass);

        let invalid: DoctorConfig = toml::from_str("[server]\nlanguage = \"fr\"\n").unwrap();
        assert_eq!(language_check(&invalid), CheckStatus::Fail);

        let mut overridden: DoctorConfig = toml::from_str("[server]\nlanguage = \"ko\"\n").unwrap();
        apply_environment_overrides_with(&mut overridden, |name| {
            (name == "OSB_LANGUAGE").then(|| "FR".into())
        })
        .unwrap();
        assert_eq!(overridden.server.language, "FR");
        assert_eq!(language_check(&overridden), CheckStatus::Fail);

        let mut uppercase: DoctorConfig = toml::from_str("[server]\nlanguage = \"ko\"\n").unwrap();
        apply_environment_overrides_with(&mut uppercase, |name| {
            (name == "OSB_LANGUAGE").then(|| "EN".into())
        })
        .unwrap();
        assert_eq!(uppercase.server.language, "EN");
        assert_eq!(language_check(&uppercase), CheckStatus::Fail);
    }

    #[test]
    fn doctor_rejects_the_runtime_effective_article_base_route_collisions() {
        fn article_check(config: &DoctorConfig) -> CheckStatus {
            let mut checks = Vec::new();
            check_semantics(config, &mut checks);
            checks
                .iter()
                .find(|check| check.id == "server.article_base_path")
                .expect("article-base check must exist")
                .status
        }

        let source = r#"
            schema_version = "open-soverign-blog/2"
            [semantic]
            intent = "personal"
            [server]
            public_url = "https://blog.example"
            article_base_path = "blog"
            [references]
            enabled = true
            [deployment]
            delivery_only = false
        "#;
        let mut enabled: DoctorConfig = toml::from_str(source).unwrap();
        apply_environment_overrides_with(&mut enabled, |name| {
            (name == "OSB_ARTICLE_BASE_PATH").then(|| "references/archive".into())
        })
        .unwrap();
        assert_eq!(enabled.server.article_base_path, "references/archive");
        assert_eq!(article_check(&enabled), CheckStatus::Fail);

        let mut disabled: DoctorConfig = toml::from_str(source).unwrap();
        let disabled_overrides = std::collections::BTreeMap::from([
            ("OSB_ARTICLE_BASE_PATH", "references/archive"),
            ("OSB_REFERENCES_ENABLED", "false"),
        ]);
        apply_environment_overrides_with(&mut disabled, |name| {
            disabled_overrides.get(name).map(|value| (*value).into())
        })
        .unwrap();
        assert!(!disabled.references.enabled);
        assert_eq!(article_check(&disabled), CheckStatus::Pass);

        let mut reserved: DoctorConfig = toml::from_str(source).unwrap();
        apply_environment_overrides_with(&mut reserved, |name| {
            (name == "OSB_ARTICLE_BASE_PATH").then(|| "api/articles".into())
        })
        .unwrap();
        assert_eq!(article_check(&reserved), CheckStatus::Fail);

        let mut empty_is_ignored: DoctorConfig = toml::from_str(source).unwrap();
        apply_environment_overrides_with(&mut empty_is_ignored, |name| {
            (name == "OSB_ARTICLE_BASE_PATH").then(String::new)
        })
        .unwrap();
        assert_eq!(empty_is_ignored.server.article_base_path, "blog");
        assert_eq!(article_check(&empty_is_ignored), CheckStatus::Pass);
    }

    #[test]
    fn doctor_installation_tracking_matches_runtime_fail_closed_rules() {
        let digest = "ab".repeat(32);
        assert_eq!(
            installation_tracking_from_values(Some(&digest), Some("false"), false).unwrap(),
            InstallationTracking::Tracked(digest.clone())
        );
        assert_eq!(
            installation_tracking_from_values(None, Some("true"), false).unwrap(),
            InstallationTracking::Untracked
        );

        let missing = installation_tracking_from_values(None, None, false).unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("OSB_INSTALL_LOCK_DIGEST is required")
        );
        let delivery = installation_tracking_from_values(None, Some("true"), true).unwrap_err();
        assert!(delivery.to_string().contains("delivery-only"));

        for invalid in [" ", "TRUE", "1", "yes", "on", " true ", "false "] {
            assert!(
                installation_tracking_from_values(Some(&digest), Some(invalid), false).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(installation_tracking_from_values(Some(&"AB".repeat(32)), None, false).is_err());
    }

    #[test]
    fn doctor_rejects_the_same_admin_module_conflicts_as_runtime() {
        let mut config: DoctorConfig = toml::from_str(
            r#"
                schema_version = "open-soverign-blog/2"
                [semantic]
                intent = "personal"
                [server]
                public_url = "https://blog.example"
                [admin]
                auth = "access_key"
                session_days = 30
            "#,
        )
        .unwrap();
        let phc = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA";
        let encoded = BASE64_STANDARD.encode(phc);
        let overrides = std::collections::BTreeMap::from([
            ("OSB_ADMIN_ACCESS_KEY_PHC_B64", encoded.as_str()),
            ("OSB_EXTERNAL_ISSUER_URL", "https://identity.example"),
            ("OSB_EXTERNAL_CLIENT_ID", "blog-client"),
            ("OSB_EXTERNAL_OWNER_SUBJECT", "stable-owner-subject"),
            ("OSB_ADMIN_SESSION_DAYS", "999"),
        ]);
        apply_environment_overrides_with(&mut config, |name| {
            overrides.get(name).map(|value| (*value).to_owned())
        })
        .unwrap();
        let mut checks = Vec::new();
        check_semantics(&config, &mut checks);
        assert_eq!(
            checks
                .iter()
                .find(|check| check.id == "admin.control_plane")
                .unwrap()
                .status,
            CheckStatus::Fail
        );
    }
}
