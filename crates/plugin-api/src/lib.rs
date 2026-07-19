//! Versioned, language-neutral plugin manifest contracts.
//!
//! A manifest requests capabilities; it never grants them. Runtime enforcement
//! belongs to the host. Parsing a manifest does not load or execute its runtime.

use std::collections::{BTreeMap, BTreeSet};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

pub const PLUGIN_MANIFEST_VERSION: u8 = 1;
pub const PLUGIN_API_VERSION: &str = "1";
pub const INSTALL_INTENT_SCHEMA_VERSION: &str = "open-soverign-blog-install/1";
pub const INSTALL_LOCK_SCHEMA_VERSION: &str = "open-soverign-blog-lock/1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginManifest {
    #[serde(default, rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema_uri: Option<String>,
    pub manifest_version: u8,
    pub id: String,
    pub name: String,
    pub version: String,
    pub license: String,
    pub provenance: PluginProvenance,
    pub plugin_api: String,
    pub kinds: Vec<PluginKind>,
    pub runtime: PluginRuntime,
    pub permissions: Vec<PermissionRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_compatibility: Option<String>,
    #[serde(default)]
    pub authors: Vec<Author>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PluginConfig>,
    #[serde(default)]
    pub hooks: Vec<Hook>,
    #[serde(default)]
    pub routes: Vec<PluginRoute>,
    #[serde(default)]
    pub render_slots: Vec<RenderSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<PluginUi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<PluginState>,
    #[serde(default)]
    pub migrations: Vec<StateMigration>,
    #[serde(default)]
    pub dependencies: Vec<PluginDependency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<RuntimeLimits>,
    #[serde(default)]
    pub metadata: BTreeMap<String, MetadataValue>,
}

/// Human-owned, secret-free installation intent. This is deliberately distinct
/// from runtime configuration: it remembers why a deployment was assembled so
/// a later release can resolve the same engine modules again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationIntent {
    pub schema_version: String,
    pub installation_id: String,
    pub site_id: String,
    pub created_with: String,
    pub selection: InstallationSelection,
    #[serde(default)]
    pub dlcs: Vec<RequestedDlc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationSelection {
    pub admin_auth: InstallationAdminAuth,
    pub style: InstallationStyle,
    pub cache: InstallationCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationAdminAuth {
    AccessKey,
    External,
    Disabled,
}

impl InstallationAdminAuth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccessKey => "access_key",
            Self::External => "external",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationCache {
    None,
    RedisStandalone,
    RedisManaged,
}

impl InstallationCache {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RedisStandalone => "redis_standalone",
            Self::RedisManaged => "redis_managed",
        }
    }

    pub const fn redis_enabled(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationStyleKind {
    None,
    Builtin,
    Custom,
}

/// A style is either absent, a stable built-in identifier, or an installed CSS
/// file whose bytes are pinned by SHA-256. The original workstation path is not
/// part of the durable contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationStyle {
    pub kind: InstallationStyleKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedDlc {
    pub id: String,
    pub version: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Exact, machine-generated installation state. The digest covers canonical
/// JSON for every field except `lockDigest`, including DLC history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallationLock {
    pub schema_version: String,
    pub installation_id: String,
    pub engine: LockedEngine,
    pub selection: InstallationSelection,
    pub dlcs: Vec<InstalledDlc>,
    /// State retained for DLCs removed from active composition. Re-adding the
    /// same stable id restores this host-owned migration ledger instead of
    /// pretending the surviving database state is a fresh installation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_dlcs: Vec<RetainedDlcState>,
    #[serde(default)]
    pub history: Vec<DlcHistoryRecord>,
    pub lock_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockedEngine {
    pub version: String,
    pub config_schema_version: String,
    pub database_schema_version: u64,
    pub plugin_api: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstalledDlcSourceKind {
    Bundled,
    File,
    Https,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstalledDlc {
    pub id: String,
    pub requested_version: String,
    pub version: String,
    pub core_compatibility: String,
    pub manifest_version: u8,
    pub plugin_api: String,
    pub source_kind: InstalledDlcSourceKind,
    pub source: String,
    pub manifest_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    pub enabled: bool,
    #[serde(default)]
    pub approved_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_version: Option<u64>,
    #[serde(default)]
    pub applied_migrations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetainedDlcState {
    pub id: String,
    pub removed_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_version: Option<u64>,
    #[serde(default)]
    pub applied_migrations: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DlcHistoryAction {
    Installed,
    Enabled,
    Disabled,
    Upgraded,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DlcHistoryRecord {
    pub sequence: u64,
    pub action: DlcHistoryAction,
    pub dlc_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_version: Option<String>,
    pub engine_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginProvenance {
    pub origin: ProvenanceOrigin,
    #[serde(default)]
    pub generated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
    #[serde(default)]
    pub source_materials: Vec<String>,
    #[serde(default)]
    pub notice_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvenanceOrigin {
    Original,
    CleanRoom,
    ThirdParty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginKind {
    AuthProvider,
    AuthorizationPolicy,
    Comments,
    Seo,
    AdProvider,
    RenderExtension,
    AiProvider,
    AiAgent,
    AiTool,
    CodeRunnerClient,
    Importer,
    Exporter,
    SearchProvider,
    StorageProvider,
    Automation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PluginRuntime {
    BuiltIn {
        #[serde(rename = "crate")]
        crate_name: String,
        extension: String,
    },
    WasiComponent {
        abi: String,
        component: String,
        world: String,
        sha256: String,
    },
    #[serde(rename = "jsonrpc-stdio")]
    JsonRpcStdio {
        abi: String,
        executable: String,
        #[serde(default)]
        arguments: Vec<String>,
        sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionRequest {
    pub capability: Capability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<PermissionScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "content.read")]
    ContentRead,
    #[serde(rename = "content.write")]
    ContentWrite,
    #[serde(rename = "content.publish")]
    ContentPublish,
    #[serde(rename = "assets.read")]
    AssetsRead,
    #[serde(rename = "assets.write")]
    AssetsWrite,
    #[serde(rename = "ontology.read")]
    OntologyRead,
    #[serde(rename = "ontology.write")]
    OntologyWrite,
    #[serde(rename = "comments.read")]
    CommentsRead,
    #[serde(rename = "comments.write")]
    CommentsWrite,
    #[serde(rename = "comments.moderate")]
    CommentsModerate,
    #[serde(rename = "ai.invoke")]
    AiInvoke,
    #[serde(rename = "ai.tool")]
    AiTool,
    #[serde(rename = "render.slot")]
    RenderSlot,
    #[serde(rename = "route.public")]
    RoutePublic,
    #[serde(rename = "route.admin")]
    RouteAdmin,
    #[serde(rename = "jobs.schedule")]
    JobsSchedule,
    #[serde(rename = "network.connect")]
    NetworkConnect,
    #[serde(rename = "secret.use")]
    SecretUse,
    #[serde(rename = "state.read")]
    StateRead,
    #[serde(rename = "state.write")]
    StateWrite,
    #[serde(rename = "code.execute")]
    CodeExecute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    Public,
    Published,
    Drafts,
    Own,
    Site,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Author {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SchemaReference {
    PackagePath(String),
    Inline(BTreeMap<String, Value>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginConfig {
    pub schema: SchemaReference,
    #[serde(default)]
    pub defaults: BTreeMap<String, Value>,
    #[serde(default)]
    pub secret_bindings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDelivery {
    Sync,
    Async,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailurePolicy {
    Continue,
    Retry,
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hook {
    pub event: String,
    pub handler: String,
    #[serde(default = "default_hook_delivery")]
    pub delivery: HookDelivery,
    #[serde(default = "default_hook_failure_policy")]
    pub failure_policy: HookFailurePolicy,
    #[serde(default)]
    pub priority: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HttpMethod {
    GET,
    POST,
    PUT,
    PATCH,
    DELETE,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAudience {
    Public,
    Authenticated,
    Admin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginRoute {
    pub path: String,
    pub methods: Vec<HttpMethod>,
    pub audience: RouteAudience,
    pub handler: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_schema: Option<SchemaReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<SchemaReference>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderOutput {
    Model,
    SanitizedHtml,
    SandboxedFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderCache {
    None,
    Revision,
    Site,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderSlot {
    pub slot: String,
    pub handler: String,
    pub output: RenderOutput,
    #[serde(default = "default_render_cache")]
    pub cache: RenderCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiPanelLocation {
    Settings,
    EditorSidebar,
    EditorBlock,
    Dashboard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiPanel {
    pub id: String,
    pub location: UiPanelLocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginUi {
    pub sandbox: UiSandbox,
    pub entrypoint: String,
    pub sha256: String,
    #[serde(default)]
    pub panels: Vec<UiPanel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSandbox {
    Iframe,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateCollection {
    pub name: String,
    pub schema: SchemaReference,
    #[serde(default)]
    pub indexes: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginState {
    pub version: u64,
    #[serde(default)]
    pub collections: Vec<StateCollection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateMigration {
    pub id: String,
    pub from: u64,
    pub to: u64,
    pub handler: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginDependency {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u64>,
    #[serde(default, rename = "memoryMiB", skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u32>,
    #[serde(default, rename = "outputKiB", skip_serializing_if = "Option::is_none")]
    pub output_kib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MetadataValue {
    String(String),
    Number(serde_json::Number),
    Boolean(bool),
    Null,
}

impl PluginManifest {
    pub fn from_toml(source: &str) -> Result<Self, ManifestError> {
        let value: Self =
            toml::from_str(source).map_err(|error| ManifestError::Parse(error.to_string()))?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.manifest_version != PLUGIN_MANIFEST_VERSION || self.plugin_api != PLUGIN_API_VERSION
        {
            return Err(ManifestError::UnsupportedVersion);
        }
        if !valid_plugin_id(&self.id) {
            return Err(ManifestError::InvalidPluginId);
        }
        bounded_text(&self.name, 1, 100, "name")?;
        bounded_text(&self.license, 1, 200, "license")?;
        if Version::parse(&self.version).is_err() {
            return Err(ManifestError::InvalidVersion);
        }
        if self
            .schema_uri
            .as_deref()
            .is_some_and(|value| !valid_uri_reference(value))
        {
            return Err(ManifestError::InvalidField("$schema"));
        }
        optional_bounded_text(self.description.as_deref(), 0, 2000, "description")?;
        optional_bounded_text(
            self.core_compatibility.as_deref(),
            1,
            128,
            "coreCompatibility",
        )?;
        if self
            .core_compatibility
            .as_deref()
            .is_some_and(|requirement| VersionReq::parse(requirement).is_err())
        {
            return Err(ManifestError::InvalidField("coreCompatibility"));
        }
        optional_absolute_uri(self.homepage.as_deref(), 2048, "homepage")?;
        optional_absolute_uri(self.repository.as_deref(), 2048, "repository")?;

        if self.kinds.is_empty() || self.kinds.len() > 16 || !all_unique(&self.kinds) {
            return Err(ManifestError::DuplicateOrExcessValue("kinds"));
        }
        if self.permissions.len() > 128 || !all_unique(&self.permissions) {
            return Err(ManifestError::DuplicateOrExcessValue("permissions"));
        }
        self.provenance.validate()?;
        self.runtime.validate()?;
        for permission in &self.permissions {
            permission.validate()?;
        }
        validate_collection(&self.authors, 32, Author::validate, "authors")?;
        if let Some(config) = &self.config {
            config.validate()?;
        }
        validate_collection(&self.hooks, 128, Hook::validate, "hooks")?;
        validate_collection(&self.routes, 64, PluginRoute::validate, "routes")?;
        validate_collection(&self.render_slots, 64, RenderSlot::validate, "renderSlots")?;
        if let Some(ui) = &self.ui {
            ui.validate()?;
        }
        if let Some(state) = &self.state {
            state.validate()?;
        }
        validate_collection(
            &self.migrations,
            256,
            StateMigration::validate,
            "migrations",
        )?;
        validate_collection(
            &self.dependencies,
            64,
            PluginDependency::validate,
            "dependencies",
        )?;
        if let Some(limits) = &self.limits {
            limits.validate()?;
        }
        if self.metadata.len() > 64 || self.metadata.keys().any(|key| !valid_metadata_key(key)) {
            return Err(ManifestError::InvalidField("metadata"));
        }
        Ok(())
    }

    pub fn supports_core(&self, core_version: &str) -> Result<bool, ManifestError> {
        let core = Version::parse(core_version).map_err(|_| ManifestError::InvalidVersion)?;
        self.core_compatibility
            .as_deref()
            .map(VersionReq::parse)
            .transpose()
            .map(|requirement| requirement.is_none_or(|requirement| requirement.matches(&core)))
            .map_err(|_| ManifestError::InvalidField("coreCompatibility"))
    }
}

impl InstallationIntent {
    pub fn from_toml(source: &str) -> Result<Self, InstallContractError> {
        let value: Self = toml::from_str(source)
            .map_err(|error| InstallContractError::Parse(error.to_string()))?;
        value.validate()?;
        Ok(value)
    }

    pub fn to_toml_pretty(&self) -> Result<String, InstallContractError> {
        self.validate()?;
        let mut rendered = toml::to_string_pretty(self)
            .map_err(|error| InstallContractError::Serialize(error.to_string()))?;
        if !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        Ok(rendered)
    }

    pub fn validate(&self) -> Result<(), InstallContractError> {
        if self.schema_version != INSTALL_INTENT_SCHEMA_VERSION {
            return Err(InstallContractError::UnsupportedSchema(
                self.schema_version.clone(),
            ));
        }
        validate_uuid("installation_id", &self.installation_id)?;
        validate_uuid("site_id", &self.site_id)?;
        parse_version("created_with", &self.created_with)?;
        self.selection.validate()?;
        if self.dlcs.len() > 256 {
            return Err(invalid_install("dlcs exceeds 256 records"));
        }
        let mut ids = BTreeSet::new();
        for dlc in &self.dlcs {
            dlc.validate()?;
            if !ids.insert(dlc.id.as_str()) {
                return Err(invalid_install("dlcs contains a duplicate id"));
            }
        }
        Ok(())
    }
}

impl InstallationSelection {
    pub fn validate(&self) -> Result<(), InstallContractError> {
        self.style.validate()
    }
}

impl InstallationStyle {
    pub fn validate(&self) -> Result<(), InstallContractError> {
        match self.kind {
            InstallationStyleKind::None => {
                if self.id.is_some() || self.file.is_some() || self.sha256.is_some() {
                    return Err(invalid_install(
                        "style kind none cannot include id, file, or sha256",
                    ));
                }
            }
            InstallationStyleKind::Builtin => {
                if !self.id.as_deref().is_some_and(valid_style_id)
                    || self.file.is_some()
                    || self.sha256.is_some()
                {
                    return Err(invalid_install(
                        "builtin style requires only a lowercase stable id",
                    ));
                }
            }
            InstallationStyleKind::Custom => {
                if self.id.is_some()
                    || !self.file.as_deref().is_some_and(safe_relative_path)
                    || !self.sha256.as_deref().is_some_and(valid_sha256)
                {
                    return Err(invalid_install(
                        "custom style requires only a safe installed file and sha256",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl RequestedDlc {
    pub fn validate(&self) -> Result<(), InstallContractError> {
        if !valid_plugin_id(&self.id) {
            return Err(invalid_install("requested DLC id is invalid"));
        }
        parse_requirement("requested DLC version", &self.version)?;
        Ok(())
    }
}

impl InstallationLock {
    pub fn from_json(source: &str) -> Result<Self, InstallContractError> {
        let value: Self = serde_json::from_str(source)
            .map_err(|error| InstallContractError::Parse(error.to_string()))?;
        value.validate()?;
        Ok(value)
    }

    pub fn refresh_digest(&mut self) -> Result<(), InstallContractError> {
        self.lock_digest.clear();
        self.lock_digest = self.expected_digest()?;
        Ok(())
    }

    /// Applies the exact engine transition used by the Linux updater. A caller
    /// must still replace the lock file atomically after this succeeds.
    pub fn record_engine_upgrade(
        &mut self,
        from: &str,
        to: &str,
        source: String,
        artifact_sha256: Option<String>,
    ) -> Result<(), InstallContractError> {
        self.validate()?;
        if self.engine.version != from {
            return Err(invalid_install(
                "engine upgrade source version differs from the current lock",
            ));
        }
        let from_version = parse_version("engine upgrade from", from)?;
        let to_version = parse_version("engine upgrade to", to)?;
        if to_version <= from_version {
            return Err(invalid_install(
                "engine upgrade target must be greater than its source",
            ));
        }
        self.engine.version = to.into();
        self.engine.source = source;
        self.engine.artifact_sha256 = artifact_sha256;
        self.refresh_digest()?;
        // Re-validating here checks every installed DLC's coreCompatibility
        // against the target engine before the updater can persist the lock.
        self.validate()
    }

    pub fn expected_digest(&self) -> Result<String, InstallContractError> {
        let mut payload = self.clone();
        payload.lock_digest.clear();
        let value = serde_json::to_value(payload)
            .map_err(|error| InstallContractError::Serialize(error.to_string()))?;
        let canonical = canonical_json(value);
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|error| InstallContractError::Serialize(error.to_string()))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    pub fn to_pretty_json(&self) -> Result<String, InstallContractError> {
        self.validate()?;
        let mut rendered = serde_json::to_string_pretty(self)
            .map_err(|error| InstallContractError::Serialize(error.to_string()))?;
        rendered.push('\n');
        Ok(rendered)
    }

    pub fn validate(&self) -> Result<(), InstallContractError> {
        if self.schema_version != INSTALL_LOCK_SCHEMA_VERSION {
            return Err(InstallContractError::UnsupportedSchema(
                self.schema_version.clone(),
            ));
        }
        validate_uuid("installation_id", &self.installation_id)?;
        self.engine.validate()?;
        self.selection.validate()?;
        if self.dlcs.len() > 256 {
            return Err(invalid_install("DLC lock exceeds 256 records"));
        }
        if self.retained_dlcs.len() > 256 {
            return Err(invalid_install("retained DLC state exceeds 256 records"));
        }
        let engine_version = parse_version("engine.version", &self.engine.version)?;
        let mut previous_id: Option<&str> = None;
        for dlc in &self.dlcs {
            if previous_id.is_some_and(|previous| previous >= dlc.id.as_str()) {
                return Err(invalid_install(
                    "DLC lock records must be uniquely sorted by id",
                ));
            }
            dlc.validate(&engine_version)?;
            previous_id = Some(&dlc.id);
        }
        let active_ids = self
            .dlcs
            .iter()
            .map(|dlc| dlc.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut previous_retained_id: Option<&str> = None;
        for retained in &self.retained_dlcs {
            if previous_retained_id.is_some_and(|previous| previous >= retained.id.as_str()) {
                return Err(invalid_install(
                    "retained DLC state must be uniquely sorted by id",
                ));
            }
            if active_ids.contains(retained.id.as_str()) {
                return Err(invalid_install(
                    "active and retained DLC records cannot share an id",
                ));
            }
            retained.validate()?;
            previous_retained_id = Some(&retained.id);
        }
        for (index, record) in self.history.iter().enumerate() {
            let expected = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            if record.sequence != expected {
                return Err(invalid_install(
                    "DLC history sequence must be contiguous and start at one",
                ));
            }
            record.validate()?;
        }
        if !valid_sha256(&self.lock_digest) || self.lock_digest != self.expected_digest()? {
            return Err(InstallContractError::DigestMismatch);
        }
        Ok(())
    }
}

impl LockedEngine {
    fn validate(&self) -> Result<(), InstallContractError> {
        parse_version("engine.version", &self.version)?;
        if self.plugin_api != PLUGIN_API_VERSION {
            return Err(invalid_install("engine plugin API is unsupported"));
        }
        if self.database_schema_version == 0
            || !bounded_install_text(&self.config_schema_version, 1, 128)
            || !bounded_install_text(&self.source, 1, 2048)
            || self
                .artifact_sha256
                .as_deref()
                .is_some_and(|digest| !valid_sha256(digest))
        {
            return Err(invalid_install("engine lock record is invalid"));
        }
        Ok(())
    }
}

impl InstalledDlc {
    fn validate(&self, engine_version: &Version) -> Result<(), InstallContractError> {
        if !valid_plugin_id(&self.id) {
            return Err(invalid_install("installed DLC id is invalid"));
        }
        let requested =
            parse_requirement("installed DLC requested_version", &self.requested_version)?;
        let version = parse_version("installed DLC version", &self.version)?;
        let core = parse_requirement("installed DLC core_compatibility", &self.core_compatibility)?;
        if !requested.matches(&version) || !core.matches(engine_version) {
            return Err(invalid_install(
                "installed DLC does not satisfy its requested or core version range",
            ));
        }
        if self.manifest_version != PLUGIN_MANIFEST_VERSION
            || self.plugin_api != PLUGIN_API_VERSION
            || !valid_sha256(&self.manifest_sha256)
            || self
                .artifact_sha256
                .as_deref()
                .is_some_and(|digest| !valid_sha256(digest))
            || self
                .config_sha256
                .as_deref()
                .is_some_and(|digest| !valid_sha256(digest))
            || self.state_version == Some(0)
        {
            return Err(invalid_install("installed DLC lock material is invalid"));
        }
        match self.source_kind {
            InstalledDlcSourceKind::Bundled => {
                if !bounded_install_text(&self.source, 1, 512) {
                    return Err(invalid_install("bundled DLC source is invalid"));
                }
            }
            InstalledDlcSourceKind::File => {
                if !safe_relative_path(&self.source) || self.artifact_sha256.is_none() {
                    return Err(invalid_install(
                        "file DLC requires a safe relative source and artifact digest",
                    ));
                }
            }
            InstalledDlcSourceKind::Https => {
                if !Url::parse(&self.source).is_ok_and(|url| {
                    url.scheme() == "https"
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none()
                }) || self.artifact_sha256.is_none()
                {
                    return Err(invalid_install(
                        "HTTPS DLC requires a safe URL and artifact digest",
                    ));
                }
            }
        }
        if !strictly_sorted_unique(&self.approved_capabilities)
            || self
                .approved_capabilities
                .iter()
                .any(|value| !valid_capability_name(value))
            || !all_unique(&self.applied_migrations)
            || self
                .applied_migrations
                .iter()
                .any(|value| !qualified_name(value))
        {
            return Err(invalid_install(
                "DLC capabilities or applied migrations are invalid",
            ));
        }
        Ok(())
    }
}

impl RetainedDlcState {
    fn validate(&self) -> Result<(), InstallContractError> {
        if !valid_plugin_id(&self.id)
            || Version::parse(&self.removed_version).is_err()
            || self.state_version == Some(0)
            || !all_unique(&self.applied_migrations)
            || self
                .applied_migrations
                .iter()
                .any(|value| !qualified_name(value))
        {
            return Err(invalid_install("retained DLC state is invalid"));
        }
        Ok(())
    }
}

impl DlcHistoryRecord {
    fn validate(&self) -> Result<(), InstallContractError> {
        if !valid_plugin_id(&self.dlc_id) {
            return Err(invalid_install("DLC history id is invalid"));
        }
        parse_version("DLC history engine_version", &self.engine_version)?;
        if let Some(version) = &self.from_version {
            parse_version("DLC history from_version", version)?;
        }
        if let Some(version) = &self.to_version {
            parse_version("DLC history to_version", version)?;
        }
        let versions_are_valid = match self.action {
            DlcHistoryAction::Installed => self.from_version.is_none() && self.to_version.is_some(),
            DlcHistoryAction::Upgraded => match (&self.from_version, &self.to_version) {
                (Some(from), Some(to)) => {
                    let from = Version::parse(from).expect("history versions were parsed above");
                    let to = Version::parse(to).expect("history versions were parsed above");
                    to > from
                }
                _ => false,
            },
            DlcHistoryAction::Removed => self.from_version.is_some() && self.to_version.is_none(),
            DlcHistoryAction::Enabled | DlcHistoryAction::Disabled => {
                self.from_version.is_some() && self.from_version == self.to_version
            }
        };
        if !versions_are_valid {
            return Err(invalid_install("DLC history version transition is invalid"));
        }
        Ok(())
    }
}

fn canonical_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted = object
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        value => value,
    }
}

fn validate_uuid(name: &'static str, value: &str) -> Result<(), InstallContractError> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| InstallContractError::Invalid(name.into()))
}

fn parse_version(name: &'static str, value: &str) -> Result<Version, InstallContractError> {
    Version::parse(value).map_err(|_| InstallContractError::Invalid(name.into()))
}

fn parse_requirement(name: &'static str, value: &str) -> Result<VersionReq, InstallContractError> {
    VersionReq::parse(value).map_err(|_| InstallContractError::Invalid(name.into()))
}

fn invalid_install(message: &'static str) -> InstallContractError {
    InstallContractError::Invalid(message.into())
}

fn bounded_install_text(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.len())
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_style_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte.is_ascii_lowercase()
            } else {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            }
        })
}

fn valid_capability_name(value: &str) -> bool {
    (3..=128).contains(&value.len())
        && value.contains('.')
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn strictly_sorted_unique(values: &[String]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

impl PluginProvenance {
    fn validate(&self) -> Result<(), ManifestError> {
        optional_bounded_text(self.generator.as_deref(), 0, 500, "provenance.generator")?;
        optional_bounded_text(
            self.attestation.as_deref(),
            0,
            2000,
            "provenance.attestation",
        )?;
        if self.source_materials.len() > 64
            || !all_unique(&self.source_materials)
            || self
                .source_materials
                .iter()
                .any(|uri| char_len(uri) > 2048 || Url::parse(uri).is_err())
            || self.notice_files.len() > 64
            || !all_unique(&self.notice_files)
            || self
                .notice_files
                .iter()
                .any(|path| !safe_relative_path(path))
        {
            return Err(ManifestError::InvalidField("provenance"));
        }
        Ok(())
    }
}

impl PluginRuntime {
    fn validate(&self) -> Result<(), ManifestError> {
        match self {
            Self::BuiltIn {
                crate_name,
                extension,
            } => {
                if !valid_crate_name(crate_name) || !qualified_name(extension) {
                    return Err(ManifestError::InvalidRuntime);
                }
            }
            Self::WasiComponent {
                abi,
                component,
                world,
                sha256,
            } => {
                if abi != "wasi-component@1"
                    || !safe_relative_path(component)
                    || !qualified_name(world)
                    || !valid_sha256(sha256)
                {
                    return Err(ManifestError::InvalidRuntime);
                }
            }
            Self::JsonRpcStdio {
                abi,
                executable,
                arguments,
                sha256,
            } => {
                if abi != "jsonrpc-stdio@1"
                    || !safe_relative_path(executable)
                    || !valid_sha256(sha256)
                    || arguments.len() > 64
                    || arguments
                        .iter()
                        .any(|value| char_len(value) > 1024 || value.contains('\0'))
                {
                    return Err(ManifestError::InvalidRuntime);
                }
            }
        }
        Ok(())
    }
}

impl PermissionRequest {
    fn validate(&self) -> Result<(), ManifestError> {
        let requires_resources = matches!(
            self.capability,
            Capability::RenderSlot
                | Capability::RoutePublic
                | Capability::RouteAdmin
                | Capability::NetworkConnect
                | Capability::SecretUse
                | Capability::CodeExecute
        );
        if requires_resources && self.resources.as_ref().is_none_or(Vec::is_empty) {
            return Err(ManifestError::MissingCapabilityResource);
        }
        if let Some(resources) = &self.resources
            && (resources.is_empty()
                || resources.len() > 128
                || !all_unique(resources)
                || resources.iter().any(|value| {
                    char_len(value) > 512
                        || value.is_empty()
                        || value.contains(['\0', '*'])
                        || value.chars().any(char::is_whitespace)
                }))
        {
            return Err(ManifestError::UnsafeCapabilityResource);
        }
        optional_bounded_text(self.reason.as_deref(), 0, 1000, "permission.reason")
    }
}

impl Author {
    fn validate(&self) -> Result<(), ManifestError> {
        bounded_text(&self.name, 1, 200, "author.name")?;
        if self
            .email
            .as_deref()
            .is_some_and(|email| char_len(email) > 320 || !valid_email_shape(email))
        {
            return Err(ManifestError::InvalidField("author.email"));
        }
        optional_absolute_uri(self.url.as_deref(), 2048, "author.url")
    }
}

impl SchemaReference {
    fn validate(&self) -> Result<(), ManifestError> {
        match self {
            Self::PackagePath(path) if safe_relative_path(path) => Ok(()),
            Self::Inline(_) => Ok(()),
            Self::PackagePath(_) => Err(ManifestError::UnsafePath),
        }
    }
}

impl PluginConfig {
    fn validate(&self) -> Result<(), ManifestError> {
        self.schema.validate()?;
        if self.secret_bindings.len() > 64
            || !all_unique(&self.secret_bindings)
            || self
                .secret_bindings
                .iter()
                .any(|binding| !qualified_name(binding))
        {
            return Err(ManifestError::InvalidField("config.secretBindings"));
        }
        Ok(())
    }
}

impl Hook {
    fn validate(&self) -> Result<(), ManifestError> {
        if !valid_hook_event(&self.event)
            || !qualified_name(&self.handler)
            || !(-1000..=1000).contains(&self.priority)
        {
            return Err(ManifestError::InvalidField("hooks"));
        }
        Ok(())
    }
}

impl PluginRoute {
    fn validate(&self) -> Result<(), ManifestError> {
        if !safe_route_path(&self.path)
            || self.methods.is_empty()
            || !all_unique(&self.methods)
            || !qualified_name(&self.handler)
        {
            return Err(ManifestError::InvalidField("routes"));
        }
        if let Some(schema) = &self.request_schema {
            schema.validate()?;
        }
        if let Some(schema) = &self.response_schema {
            schema.validate()?;
        }
        Ok(())
    }
}

impl RenderSlot {
    fn validate(&self) -> Result<(), ManifestError> {
        if !qualified_name(&self.slot) || !qualified_name(&self.handler) {
            return Err(ManifestError::InvalidField("renderSlots"));
        }
        Ok(())
    }
}

impl PluginUi {
    fn validate(&self) -> Result<(), ManifestError> {
        if !safe_relative_path(&self.entrypoint)
            || !valid_sha256(&self.sha256)
            || self.panels.len() > 32
            || !all_unique(&self.panels)
            || self.panels.iter().any(|panel| !qualified_name(&panel.id))
        {
            return Err(ManifestError::InvalidField("ui"));
        }
        Ok(())
    }
}

impl StateCollection {
    fn validate(&self) -> Result<(), ManifestError> {
        if !qualified_name(&self.name)
            || self.indexes.len() > 16
            || !all_unique(&self.indexes)
            || self.indexes.iter().any(|index| {
                index.is_empty()
                    || index.len() > 8
                    || !all_unique(index)
                    || index
                        .iter()
                        .any(|field| field.is_empty() || char_len(field) > 128)
            })
        {
            return Err(ManifestError::InvalidField("state.collections"));
        }
        self.schema.validate()
    }
}

impl PluginState {
    fn validate(&self) -> Result<(), ManifestError> {
        if self.version == 0 || self.collections.len() > 64 || !all_unique(&self.collections) {
            return Err(ManifestError::InvalidField("state"));
        }
        for collection in &self.collections {
            collection.validate()?;
        }
        Ok(())
    }
}

impl StateMigration {
    fn validate(&self) -> Result<(), ManifestError> {
        if !qualified_name(&self.id) || self.to == 0 || !qualified_name(&self.handler) {
            return Err(ManifestError::InvalidField("migrations"));
        }
        Ok(())
    }
}

impl PluginDependency {
    fn validate(&self) -> Result<(), ManifestError> {
        if !valid_plugin_id(&self.id)
            || !bounded_plain_text(&self.version, 1, 128)
            || VersionReq::parse(&self.version).is_err()
        {
            return Err(ManifestError::InvalidField("dependencies"));
        }
        Ok(())
    }
}

impl RuntimeLimits {
    fn validate(&self) -> Result<(), ManifestError> {
        if self
            .timeout_ms
            .is_some_and(|value| !(1..=3_600_000).contains(&value))
            || self
                .cpu_millis
                .is_some_and(|value| !(1..=3_600_000).contains(&value))
            || self
                .memory_mib
                .is_some_and(|value| !(1..=65_536).contains(&value))
            || self.pids.is_some_and(|value| !(1..=4096).contains(&value))
            || self
                .output_kib
                .is_some_and(|value| !(1..=1_048_576).contains(&value))
            || self
                .concurrency
                .is_some_and(|value| !(1..=1024).contains(&value))
        {
            return Err(ManifestError::InvalidLimits);
        }
        Ok(())
    }
}

fn validate_collection<T: PartialEq>(
    values: &[T],
    maximum: usize,
    validate: fn(&T) -> Result<(), ManifestError>,
    field: &'static str,
) -> Result<(), ManifestError> {
    if values.len() > maximum || !all_unique(values) {
        return Err(ManifestError::DuplicateOrExcessValue(field));
    }
    for value in values {
        validate(value)?;
    }
    Ok(())
}

fn all_unique<T: PartialEq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .all(|(index, value)| !values[..index].contains(value))
}

fn valid_plugin_id(value: &str) -> bool {
    char_len(value) <= 190
        && value.split('.').count() >= 3
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

fn valid_crate_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte.is_ascii_lowercase()
            } else {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            }
        })
}

fn qualified_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte.is_ascii_lowercase()
            } else {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'.' | b'-')
            }
        })
}

fn valid_metadata_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte.is_ascii_lowercase()
            } else {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'.' | b'-')
            }
        })
}

fn safe_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.starts_with('/')
        && !value.contains(['\\', '\0'])
        && value.split('/').all(|segment| {
            !segment.is_empty()
                && segment != ".."
                && segment.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'@' | b'+' | b'-')
                })
        })
}

fn safe_route_path(value: &str) -> bool {
    !value.is_empty()
        && char_len(value) <= 256
        && value.starts_with('/')
        && !value.contains("//")
        && value.split('/').all(|segment| segment != "..")
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'_' | b'.' | b'{' | b'}' | b':' | b'-')
        })
}

fn valid_hook_event(value: &str) -> bool {
    if !(4..=160).contains(&value.len()) {
        return false;
    }
    let Some((prefix, version)) = value.rsplit_once(".v") else {
        return false;
    };
    prefix.bytes().enumerate().all(|(index, byte)| {
        if index == 0 {
            byte.is_ascii_lowercase()
        } else {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'-')
        }
    }) && !version.is_empty()
        && !version.starts_with('0')
        && version.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_email_shape(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !value.chars().any(char::is_whitespace)
}

fn valid_uri_reference(value: &str) -> bool {
    !value.is_empty()
        && char_len(value) <= 2048
        && !value.chars().any(char::is_whitespace)
        && Url::parse("https://manifest.invalid/base/")
            .and_then(|base| base.join(value))
            .is_ok()
}

fn char_len(value: &str) -> usize {
    value.chars().count()
}

fn bounded_plain_text(value: &str, minimum: usize, maximum: usize) -> bool {
    let length = char_len(value);
    (minimum..=maximum).contains(&length) && !value.contains('\0')
}

fn bounded_text(
    value: &str,
    minimum: usize,
    maximum: usize,
    field: &'static str,
) -> Result<(), ManifestError> {
    if bounded_plain_text(value, minimum, maximum) {
        Ok(())
    } else {
        Err(ManifestError::InvalidField(field))
    }
}

fn optional_bounded_text(
    value: Option<&str>,
    minimum: usize,
    maximum: usize,
    field: &'static str,
) -> Result<(), ManifestError> {
    value.map_or(Ok(()), |value| bounded_text(value, minimum, maximum, field))
}

fn optional_absolute_uri(
    value: Option<&str>,
    maximum: usize,
    field: &'static str,
) -> Result<(), ManifestError> {
    if value.is_some_and(|uri| char_len(uri) > maximum || Url::parse(uri).is_err()) {
        Err(ManifestError::InvalidField(field))
    } else {
        Ok(())
    }
}

const fn default_hook_delivery() -> HookDelivery {
    HookDelivery::Async
}

const fn default_hook_failure_policy() -> HookFailurePolicy {
    HookFailurePolicy::Retry
}

const fn default_render_cache() -> RenderCache {
    RenderCache::Revision
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("plugin manifest could not be parsed: {0}")]
    Parse(String),
    #[error("unsupported manifest or plugin API version")]
    UnsupportedVersion,
    #[error("plugin id must be a lowercase reverse-domain identifier")]
    InvalidPluginId,
    #[error("plugin version is not valid semantic versioning")]
    InvalidVersion,
    #[error("manifest field is invalid: {0}")]
    InvalidField(&'static str),
    #[error("manifest collection is duplicated or exceeds its v1 limit: {0}")]
    DuplicateOrExcessValue(&'static str),
    #[error("this capability requires an explicit resource allowlist")]
    MissingCapabilityResource,
    #[error("capability resource is empty, duplicated, wildcarded, or unsafe")]
    UnsafeCapabilityResource,
    #[error("plugin runtime declaration is invalid")]
    InvalidRuntime,
    #[error("plugin package path is unsafe")]
    UnsafePath,
    #[error("plugin runtime limits are invalid")]
    InvalidLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstallContractError {
    #[error("installation contract could not be parsed: {0}")]
    Parse(String),
    #[error("unsupported installation schema: {0}")]
    UnsupportedSchema(String),
    #[error("installation contract is invalid: {0}")]
    Invalid(String),
    #[error("installation lock digest does not match its canonical payload")]
    DigestMismatch,
    #[error("installation contract could not be serialized: {0}")]
    Serialize(String),
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    const BASE: &str = r#"
manifestVersion = 1
id = "org.open-soverign-blog.seo"
name = "SEO"
version = "0.1.0"
license = "Unlicense"
pluginApi = "1"
kinds = ["seo"]
permissions = []

[provenance]
origin = "original"

[runtime]
kind = "built-in"
crate = "osb-feature-seo"
extension = "seo"
"#;

    #[test]
    fn parses_a_built_in_manifest() {
        let manifest = PluginManifest::from_toml(BASE).unwrap();
        assert_eq!(manifest.id, "org.open-soverign-blog.seo");
    }

    #[test]
    fn rejects_ambient_network_access() {
        let source = BASE.replace(
            "permissions = []",
            "permissions = [{ capability = \"network.connect\", resources = [\"*\"] }]",
        );
        assert_eq!(
            PluginManifest::from_toml(&source),
            Err(ManifestError::UnsafeCapabilityResource)
        );
    }

    #[test]
    fn rejects_parent_directory_entrypoints() {
        let source = BASE
            .replace("kind = \"built-in\"", "kind = \"jsonrpc-stdio\"")
            .replace(
                "crate = \"osb-feature-seo\"\nextension = \"seo\"",
                "abi = \"jsonrpc-stdio@1\"\nexecutable = \"../escape\"\nsha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            );
        assert_eq!(
            PluginManifest::from_toml(&source),
            Err(ManifestError::InvalidRuntime)
        );
    }

    #[test]
    fn representative_rich_fixture_is_rust_valid_and_preserved() {
        let source = include_str!("../tests/fixtures/rich-plugin.toml");
        let manifest = PluginManifest::from_toml(source).unwrap();
        assert_eq!(manifest.routes.len(), 1);
        assert_eq!(manifest.ui.as_ref().unwrap().panels.len(), 1);
        assert_eq!(manifest.state.as_ref().unwrap().collections.len(), 1);
        assert_eq!(manifest.migrations.len(), 1);
        assert_eq!(manifest.dependencies.len(), 1);

        let projection = serde_json::to_value(&manifest).unwrap();
        assert_eq!(projection["limits"]["memoryMiB"], 64);
        assert_eq!(projection["limits"]["outputKiB"], 128);
        assert_eq!(
            projection["routes"][0]["requestSchema"],
            "schemas/request.json"
        );
    }

    #[test]
    fn limits_use_the_same_explicit_units_as_the_schema() {
        let source = format!("{BASE}\n[limits]\ntimeoutMs = 10\nmemoryMb = 64\nmaxOutputKb = 1\n");
        assert!(matches!(
            PluginManifest::from_toml(&source),
            Err(ManifestError::Parse(_))
        ));
    }

    #[test]
    fn every_official_manifest_matches_the_rust_contract() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/official");
        let mut parsed = 0;
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path().join("plugin.toml");
            if !path.exists() {
                continue;
            }
            let source = fs::read_to_string(&path).unwrap();
            PluginManifest::from_toml(&source)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            parsed += 1;
        }
        assert_eq!(
            parsed, 10,
            "all official optional modules must have manifests"
        );
    }

    fn sample_selection() -> InstallationSelection {
        InstallationSelection {
            admin_auth: InstallationAdminAuth::AccessKey,
            style: InstallationStyle {
                kind: InstallationStyleKind::Builtin,
                id: Some("namuwiki".into()),
                file: None,
                sha256: None,
            },
            cache: InstallationCache::RedisManaged,
        }
    }

    fn sample_lock() -> InstallationLock {
        let mut lock = InstallationLock {
            schema_version: INSTALL_LOCK_SCHEMA_VERSION.into(),
            installation_id: "018f0000-0000-7000-8000-000000000001".into(),
            engine: LockedEngine {
                version: "0.1.0".into(),
                config_schema_version: "open-soverign-blog/2".into(),
                database_schema_version: 6,
                plugin_api: PLUGIN_API_VERSION.into(),
                source: "source-checkout".into(),
                artifact_sha256: None,
            },
            selection: sample_selection(),
            dlcs: vec![InstalledDlc {
                id: "org.open-soverign-blog.seo".into(),
                requested_version: "^0.1".into(),
                version: "0.1.0".into(),
                core_compatibility: "^0.1".into(),
                manifest_version: PLUGIN_MANIFEST_VERSION,
                plugin_api: PLUGIN_API_VERSION.into(),
                source_kind: InstalledDlcSourceKind::Bundled,
                source: "plugins/official/seo/plugin.toml".into(),
                manifest_sha256: "a".repeat(64),
                artifact_sha256: None,
                enabled: true,
                approved_capabilities: vec!["content.read".into(), "route.public".into()],
                config_sha256: None,
                state_version: None,
                applied_migrations: Vec::new(),
            }],
            retained_dlcs: Vec::new(),
            history: vec![DlcHistoryRecord {
                sequence: 1,
                action: DlcHistoryAction::Installed,
                dlc_id: "org.open-soverign-blog.seo".into(),
                from_version: None,
                to_version: Some("0.1.0".into()),
                engine_version: "0.1.0".into(),
            }],
            lock_digest: String::new(),
        };
        lock.refresh_digest().unwrap();
        lock
    }

    #[test]
    fn installation_intent_round_trips_without_secrets() {
        let intent = InstallationIntent {
            schema_version: INSTALL_INTENT_SCHEMA_VERSION.into(),
            installation_id: "018f0000-0000-7000-8000-000000000001".into(),
            site_id: "018f0000-0000-7000-8000-000000000002".into(),
            created_with: "0.1.0".into(),
            selection: sample_selection(),
            dlcs: vec![RequestedDlc {
                id: "org.open-soverign-blog.seo".into(),
                version: "^0.1".into(),
                enabled: true,
            }],
        };
        let rendered = intent.to_toml_pretty().unwrap();
        assert!(!rendered.to_ascii_lowercase().contains("secret"));
        assert_eq!(InstallationIntent::from_toml(&rendered).unwrap(), intent);
    }

    #[test]
    fn checked_in_installation_examples_are_an_exact_valid_pair() {
        let intent =
            InstallationIntent::from_toml(include_str!("../../../osb.install.example.toml"))
                .unwrap();
        let lock_source = include_str!("../../../osb.lock.example.json");
        let unchecked: InstallationLock = serde_json::from_str(lock_source).unwrap();
        assert_eq!(unchecked.lock_digest, unchecked.expected_digest().unwrap());
        let lock = InstallationLock::from_json(lock_source).unwrap();
        assert_eq!(intent.installation_id, lock.installation_id);
        assert_eq!(intent.selection, lock.selection);
        assert_eq!(intent.dlcs.len(), lock.dlcs.len());
        for (requested, installed) in intent.dlcs.iter().zip(&lock.dlcs) {
            assert_eq!(requested.id, installed.id);
            assert_eq!(requested.version, installed.requested_version);
            assert_eq!(requested.enabled, installed.enabled);
        }
    }

    #[test]
    fn lock_digest_is_deterministic_and_detects_changes() {
        let lock = sample_lock();
        let rendered = lock.to_pretty_json().unwrap();
        let parsed = InstallationLock::from_json(&rendered).unwrap();
        assert_eq!(parsed.lock_digest, lock.lock_digest);
        assert_eq!(parsed.expected_digest().unwrap(), lock.lock_digest);

        let mut changed = parsed;
        changed.dlcs[0].enabled = false;
        assert_eq!(
            changed.validate(),
            Err(InstallContractError::DigestMismatch)
        );
        changed.refresh_digest().unwrap();
        assert_ne!(changed.lock_digest, lock.lock_digest);
        changed.validate().unwrap();
    }

    #[test]
    fn lock_rejects_incompatible_or_unsorted_dlcs() {
        let mut lock = sample_lock();
        lock.dlcs[0].core_compatibility = ">=1.0".into();
        lock.refresh_digest().unwrap();
        assert!(matches!(
            lock.validate(),
            Err(InstallContractError::Invalid(_))
        ));

        let mut lock = sample_lock();
        let mut earlier = lock.dlcs[0].clone();
        earlier.id = "org.open-soverign-blog.comments".into();
        lock.dlcs.push(earlier);
        lock.refresh_digest().unwrap();
        assert!(matches!(
            lock.validate(),
            Err(InstallContractError::Invalid(_))
        ));
    }

    #[test]
    fn manifest_compatibility_and_dependencies_use_real_semver_ranges() {
        let malformed_core = BASE.replace(
            "permissions = []",
            "coreCompatibility = \"definitely-not-semver\"\npermissions = []",
        );
        assert_eq!(
            PluginManifest::from_toml(&malformed_core),
            Err(ManifestError::InvalidField("coreCompatibility"))
        );

        let compatible = BASE.replace(
            "permissions = []",
            "coreCompatibility = \"^0.1\"\npermissions = []",
        );
        let manifest = PluginManifest::from_toml(&compatible).unwrap();
        assert!(manifest.supports_core("0.1.9").unwrap());
        assert!(!manifest.supports_core("0.2.0").unwrap());
    }
}
