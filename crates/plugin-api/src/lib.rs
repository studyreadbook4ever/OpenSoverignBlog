//! Versioned, language-neutral plugin manifest contracts.
//!
//! A manifest requests capabilities; it never grants them. Runtime enforcement
//! belongs to the host. Parsing a manifest does not load or execute its runtime.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

pub const PLUGIN_MANIFEST_VERSION: u8 = 1;
pub const PLUGIN_API_VERSION: &str = "1";

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
    #[serde(default)]
    pub macros: Vec<MacroDeclaration>,
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
    Macro,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacroPhase {
    Draft,
    Publish,
    Request,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacroCache {
    None,
    Inputs,
    Revision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MacroDeclaration {
    pub id: String,
    pub handler: String,
    pub input_schema: SchemaReference,
    pub output_schema: SchemaReference,
    pub phases: Vec<MacroPhase>,
    #[serde(default)]
    pub deterministic: bool,
    #[serde(default = "default_macro_cache")]
    pub cache: MacroCache,
    #[serde(default = "default_true")]
    pub requires_review: bool,
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
        if !valid_semver(&self.version) {
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
        validate_collection(&self.macros, 64, MacroDeclaration::validate, "macros")?;
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

impl MacroDeclaration {
    fn validate(&self) -> Result<(), ManifestError> {
        if !qualified_name(&self.id)
            || !qualified_name(&self.handler)
            || self.phases.is_empty()
            || !all_unique(&self.phases)
        {
            return Err(ManifestError::InvalidField("macros"));
        }
        self.input_schema.validate()?;
        self.output_schema.validate()
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
        if !valid_plugin_id(&self.id) || !bounded_plain_text(&self.version, 1, 128) {
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

fn valid_semver(value: &str) -> bool {
    let (before_build, build) = value
        .split_once('+')
        .map_or((value, None), |(core, build)| (core, Some(build)));
    if build.is_some_and(|part| part.contains('+') || !valid_semver_identifiers(part, false)) {
        return false;
    }
    let (core, prerelease) = before_build
        .split_once('-')
        .map_or((before_build, None), |(core, prerelease)| {
            (core, Some(prerelease))
        });
    if prerelease.is_some_and(|part| !valid_semver_identifiers(part, true)) {
        return false;
    }
    let components: Vec<_> = core.split('.').collect();
    components.len() == 3 && components.into_iter().all(valid_numeric_identifier)
}

fn valid_semver_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!reject_numeric_leading_zero
                    || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                    || valid_numeric_identifier(identifier))
        })
}

fn valid_numeric_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
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

const fn default_macro_cache() -> MacroCache {
    MacroCache::Inputs
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
        assert_eq!(manifest.macros.len(), 1);
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
            parsed, 6,
            "all official optional modules must have manifests"
        );
    }
}
