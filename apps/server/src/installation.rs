use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use osb_plugin_api::{
    INSTALL_INTENT_SCHEMA_VERSION, INSTALL_LOCK_SCHEMA_VERSION, InstallationAdminAuth,
    InstallationCache, InstallationIntent, InstallationLock, InstallationStyleKind,
    InstalledDlcSourceKind, PLUGIN_API_VERSION, PluginManifest,
};
use osb_storage_sqlite::{DATABASE_SCHEMA_VERSION, ThemeProfile};
use sha2::{Digest, Sha256};

use crate::config::{AdminAuthMode, AuthMode, CONFIG_SCHEMA_VERSION, RedisTopology, RuntimeConfig};

const INTENT_LIMIT: u64 = 256 * 1024;
const LOCK_LIMIT: u64 = 2 * 1024 * 1024;
const CSS_LIMIT: u64 = 256 * 1024;

const BUNDLED_OFFICIAL_MANIFESTS: [(&str, &str, &str); 11] = [
    (
        "org.open-soverign-blog.monetization-policy",
        "plugins/official/ads/plugin.toml",
        include_str!("../../../plugins/official/ads/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.ai-authorship",
        "plugins/official/ai-authorship/plugin.toml",
        include_str!("../../../plugins/official/ai-authorship/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.ai-summary",
        "plugins/official/ai-summary/plugin.toml",
        include_str!("../../../plugins/official/ai-summary/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.code-runner-client",
        "plugins/official/code-runner/plugin.toml",
        include_str!("../../../plugins/official/code-runner/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.comments",
        "plugins/official/comments/plugin.toml",
        include_str!("../../../plugins/official/comments/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.external-auth",
        "plugins/official/external-auth/plugin.toml",
        include_str!("../../../plugins/official/external-auth/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.home-curation",
        "plugins/official/home-curation/plugin.toml",
        include_str!("../../../plugins/official/home-curation/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.rbac",
        "plugins/official/rbac/plugin.toml",
        include_str!("../../../plugins/official/rbac/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.release-check",
        "plugins/official/release-check/plugin.toml",
        include_str!("../../../plugins/official/release-check/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.seo",
        "plugins/official/seo/plugin.toml",
        include_str!("../../../plugins/official/seo/plugin.toml"),
    ),
    (
        "org.open-soverign-blog.social-embeds",
        "plugins/official/social-embeds/plugin.toml",
        include_str!("../../../plugins/official/social-embeds/plugin.toml"),
    ),
];

/// A verified, secret-free deployment contract. Bootstrap-generated
/// installations set `OSB_INSTALL_LOCK_DIGEST`, which turns this contract into
/// a fail-closed startup boundary. A pre-contract source/legacy checkout must
/// explicitly opt into its temporary untracked state.
#[derive(Debug, Clone)]
pub struct InstallationRuntime {
    intent: InstallationIntent,
    lock: InstallationLock,
}

impl InstallationRuntime {
    pub fn load(config: &RuntimeConfig) -> Result<Option<Self>> {
        let allow_untracked = untracked_installation_opt_in_from_env()?;
        let expected_digest = optional_env("OSB_INSTALL_LOCK_DIGEST")?;
        let Some(expected_digest) = expected_digest else {
            ensure_untracked_installation_allowed(allow_untracked, config.delivery_only)?;
            tracing::warn!(
                "OSB_ALLOW_UNTRACKED_INSTALLATION=true: running an explicitly untracked legacy/source installation"
            );
            return Ok(None);
        };
        ensure!(
            is_sha256(&expected_digest),
            "OSB_INSTALL_LOCK_DIGEST must be one lowercase SHA-256 digest"
        );

        let intent_path = required_path("OSB_INSTALL_MANIFEST")?;
        let lock_path = required_path("OSB_INSTALL_LOCK")?;
        let intent_source =
            read_regular_bounded(&intent_path, INTENT_LIMIT, "installation intent")?;
        let lock_source = read_regular_bounded(&lock_path, LOCK_LIMIT, "installation lock")?;
        let intent = InstallationIntent::from_toml(&intent_source)
            .map_err(anyhow::Error::msg)
            .context("installation intent is invalid")?;
        let lock = InstallationLock::from_json(&lock_source)
            .map_err(anyhow::Error::msg)
            .context("installation lock is invalid")?;

        ensure!(
            intent.schema_version == INSTALL_INTENT_SCHEMA_VERSION
                && lock.schema_version == INSTALL_LOCK_SCHEMA_VERSION,
            "installation contract schema mismatch"
        );
        ensure!(
            lock.lock_digest == expected_digest,
            "OSB_INSTALL_LOCK_DIGEST does not match osb.lock.json"
        );
        ensure!(
            intent.installation_id == lock.installation_id,
            "installation intent and lock belong to different installations"
        );
        ensure!(
            intent.selection == lock.selection,
            "installation intent and lock contain different structural choices"
        );
        ensure!(
            intent.site_id == config.site_id.to_string(),
            "installation site_id contradicts the runtime site_id"
        );
        ensure!(
            lock.engine.version == env!("CARGO_PKG_VERSION"),
            "installation lock expects engine {}, but this binary is {}",
            lock.engine.version,
            env!("CARGO_PKG_VERSION")
        );
        ensure!(
            lock.engine.config_schema_version == CONFIG_SCHEMA_VERSION,
            "installation lock config schema does not match this engine"
        );
        ensure!(
            lock.engine.database_schema_version == DATABASE_SCHEMA_VERSION,
            "installation lock database schema {} does not match engine schema {}",
            lock.engine.database_schema_version,
            DATABASE_SCHEMA_VERSION
        );
        ensure!(
            lock.engine.plugin_api == PLUGIN_API_VERSION,
            "installation lock plugin API does not match this engine"
        );

        verify_bundled_dlc_bytes(&lock)?;
        verify_requested_dlcs(&intent, &lock)?;
        verify_admin_auth(config, intent.selection.admin_auth)?;
        verify_cache(config, intent.selection.cache)?;
        verify_style(config, &intent, &intent_path)?;
        verify_enabled_dlc_environment(&lock)?;
        verify_composed_dlc_dependencies(config, &lock)?;

        tracing::info!(
            installation_id = %intent.installation_id,
            lock_digest = %lock.lock_digest,
            dlc_count = lock.dlcs.len(),
            "verified the bootstrap installation manifest and exact lock"
        );
        Ok(Some(Self { intent, lock }))
    }

    pub fn installation_id(&self) -> &str {
        &self.intent.installation_id
    }

    pub fn enabled_dlc_ids(&self) -> impl Iterator<Item = &str> {
        self.lock
            .dlcs
            .iter()
            .filter(|dlc| dlc.enabled)
            .map(|dlc| dlc.id.as_str())
    }

    pub fn is_dlc_enabled(&self, id: &str) -> bool {
        self.lock.dlcs.iter().any(|dlc| dlc.id == id && dlc.enabled)
    }

    pub fn initial_theme_profile(&self) -> Result<ThemeProfile> {
        match self.intent.selection.style.kind {
            InstallationStyleKind::None | InstallationStyleKind::Custom => Ok(ThemeProfile::Paper),
            InstallationStyleKind::Builtin => match self
                .intent
                .selection
                .style
                .id
                .as_deref()
                .expect("validated built-in style")
            {
                "paper" => Ok(ThemeProfile::Paper),
                "ink" => Ok(ThemeProfile::Ink),
                "forest" => Ok(ThemeProfile::Forest),
                "terminal" => Ok(ThemeProfile::Terminal),
                id => anyhow::bail!(
                    "tracked built-in style {id} is not supplied by this engine; choose paper, ink, forest, terminal, or a pinned custom CSS file"
                ),
            },
        }
    }
}

fn verify_bundled_dlc_bytes(lock: &InstallationLock) -> Result<()> {
    for installed in &lock.dlcs {
        if installed.source_kind != InstalledDlcSourceKind::Bundled {
            continue;
        }
        let (source, manifest) = BUNDLED_OFFICIAL_MANIFESTS
            .iter()
            .find_map(|(id, source, manifest)| {
                (*id == installed.id).then_some((*source, *manifest))
            })
            .with_context(|| {
                format!(
                    "bundled DLC {} is not present in this server's official catalog",
                    installed.id
                )
            })?;
        let actual_digest = format!("{:x}", Sha256::digest(manifest.as_bytes()));
        let parsed = PluginManifest::from_toml(manifest)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("bundled official manifest {} is invalid", installed.id))?;
        ensure!(
            installed.source == source
                && installed.manifest_sha256 == actual_digest
                && installed.id == parsed.id
                && installed.version == parsed.version
                && installed.manifest_version == parsed.manifest_version
                && installed.plugin_api == parsed.plugin_api
                && parsed.core_compatibility.as_deref()
                    == Some(installed.core_compatibility.as_str()),
            "bundled DLC {} lock metadata does not match the manifest bytes compiled into this server",
            installed.id
        );
    }
    Ok(())
}

fn verify_requested_dlcs(intent: &InstallationIntent, lock: &InstallationLock) -> Result<()> {
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
        "installation intent DLC requests do not match the exact DLC lock"
    );
    Ok(())
}

fn verify_admin_auth(config: &RuntimeConfig, selected: InstallationAdminAuth) -> Result<()> {
    let runtime = match config.admin_auth.mode {
        AdminAuthMode::AccessKey => InstallationAdminAuth::AccessKey,
        AdminAuthMode::External => InstallationAdminAuth::External,
        AdminAuthMode::Disabled => InstallationAdminAuth::Disabled,
    };
    ensure!(
        runtime == selected,
        "runtime administrator authentication contradicts the installation manifest"
    );
    Ok(())
}

fn verify_cache(config: &RuntimeConfig, selected: InstallationCache) -> Result<()> {
    let runtime = match config.redis.as_ref().map(|redis| redis.topology) {
        None => InstallationCache::None,
        Some(RedisTopology::Standalone) => InstallationCache::RedisStandalone,
        Some(RedisTopology::Sentinel) => InstallationCache::RedisManaged,
    };
    ensure!(
        runtime == selected,
        "runtime cache topology contradicts the installation manifest"
    );
    let environment = optional_env("OSB_CACHE")?;
    ensure!(
        environment.as_deref() == Some(selected.as_str()),
        "OSB_CACHE must exactly match the tracked installation selection ({})",
        selected.as_str()
    );
    Ok(())
}

fn verify_style(
    config: &RuntimeConfig,
    intent: &InstallationIntent,
    intent_path: &Path,
) -> Result<()> {
    let style = &intent.selection.style;
    let expected_environment = match style.kind {
        InstallationStyleKind::None => "none".to_owned(),
        InstallationStyleKind::Builtin => format!(
            "builtin:{}",
            style.id.as_deref().expect("validated built-in style")
        ),
        InstallationStyleKind::Custom => format!(
            "custom:{}",
            style.sha256.as_deref().expect("validated custom style")
        ),
    };
    ensure!(
        optional_env("OSB_STYLE")?.as_deref() == Some(expected_environment.as_str()),
        "OSB_STYLE must exactly match the tracked installation style ({expected_environment})"
    );
    ensure!(
        config.custom_css_enabled == matches!(style.kind, InstallationStyleKind::Custom),
        "runtime custom-CSS switch contradicts the tracked installation style"
    );
    if style.kind == InstallationStyleKind::Custom {
        let css_path = &config.custom_css_file;
        let bytes = read_regular_bytes_bounded(css_path, CSS_LIMIT, "tracked custom CSS")?;
        let actual = format!("{:x}", Sha256::digest(bytes));
        ensure!(
            style.sha256.as_deref() == Some(actual.as_str()),
            "tracked custom CSS bytes do not match the installation manifest"
        );
        let installed_name = style.file.as_deref().expect("validated custom style file");
        let intent_parent = intent_path.parent().unwrap_or_else(|| Path::new("."));
        let host_adjacent = intent_parent.join(installed_name);
        // In containers the CSS file is mounted at /config/custom.css. Outside
        // containers, requiring the lock's basename to agree still prevents a
        // manifest from silently naming unrelated bytes.
        ensure!(
            css_path.file_name() == host_adjacent.file_name(),
            "runtime custom CSS filename contradicts the installation manifest"
        );
    }
    Ok(())
}

fn verify_enabled_dlc_environment(lock: &InstallationLock) -> Result<()> {
    let expected = lock
        .dlcs
        .iter()
        .filter(|dlc| dlc.enabled)
        .map(|dlc| dlc.id.as_str())
        .collect::<BTreeSet<_>>();
    let raw = optional_env("OSB_DLC_IDS")?.unwrap_or_default();
    let values = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let actual = values.iter().copied().collect::<BTreeSet<_>>();
    ensure!(
        actual.len() == values.len(),
        "OSB_DLC_IDS contains a duplicate DLC id"
    );
    ensure!(
        actual == expected,
        "OSB_DLC_IDS contradicts the enabled DLCs in osb.lock.json"
    );
    Ok(())
}

fn verify_composed_dlc_dependencies(config: &RuntimeConfig, lock: &InstallationLock) -> Result<()> {
    let enabled = |id: &str| lock.dlcs.iter().any(|dlc| dlc.id == id && dlc.enabled);
    for (needed, id, reason) in [
        (
            config.comments_enabled,
            "org.open-soverign-blog.comments",
            "comments are enabled",
        ),
        (
            config.collaboration_enabled,
            "org.open-soverign-blog.rbac",
            "collaboration is enabled",
        ),
        (
            config.admin_auth.mode == AdminAuthMode::External
                || matches!(config.auth_mode, AuthMode::Oauth | AuthMode::LocalAndOauth),
            "org.open-soverign-blog.external-auth",
            "external authentication is selected",
        ),
        (
            config.runner.is_some(),
            "org.open-soverign-blog.code-runner-client",
            "a code-runner transport is configured",
        ),
    ] {
        ensure!(
            !needed || enabled(id),
            "{reason}, but required DLC {id} is not enabled in osb.lock.json"
        );
    }
    Ok(())
}

fn optional_env(name: &str) -> Result<Option<String>> {
    match env::var_os(name) {
        None => Ok(None),
        Some(value) => {
            let value = value
                .into_string()
                .map_err(|_| anyhow::anyhow!("{name} must be valid UTF-8"))?;
            let value = value.trim();
            Ok((!value.is_empty()).then(|| value.to_owned()))
        }
    }
}

fn untracked_installation_opt_in_from_env() -> Result<bool> {
    match env::var_os("OSB_ALLOW_UNTRACKED_INSTALLATION") {
        None => strict_untracked_opt_in(None),
        Some(value) => {
            let value = value.into_string().map_err(|_| {
                anyhow::anyhow!("OSB_ALLOW_UNTRACKED_INSTALLATION must be valid UTF-8")
            })?;
            strict_untracked_opt_in(Some(&value))
        }
    }
}

fn strict_untracked_opt_in(value: Option<&str>) -> Result<bool> {
    match value {
        None | Some("") | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => anyhow::bail!(
            "OSB_ALLOW_UNTRACKED_INSTALLATION must be exactly true or false when non-empty"
        ),
    }
}

fn ensure_untracked_installation_allowed(allow_untracked: bool, delivery_only: bool) -> Result<()> {
    ensure!(
        allow_untracked,
        "OSB_INSTALL_LOCK_DIGEST is required; only a pre-contract source/legacy installation may temporarily set OSB_ALLOW_UNTRACKED_INSTALLATION=true"
    );
    ensure!(
        !delivery_only,
        "OSB_ALLOW_UNTRACKED_INSTALLATION cannot bypass the installation lock on a delivery-only deployment"
    );
    Ok(())
}

fn required_path(name: &str) -> Result<PathBuf> {
    optional_env(name)?
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("{name} is required for a tracked installation"))
}

fn read_regular_bounded(path: &Path, maximum: u64, label: &str) -> Result<String> {
    let bytes = read_regular_bytes_bounded(path, maximum, label)?;
    String::from_utf8(bytes).with_context(|| format!("{label} must be UTF-8: {}", path.display()))
}

fn read_regular_bytes_bounded(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label}: {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{label} must be a regular non-symlink file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= maximum,
        "{label} exceeds the {maximum}-byte limit: {}",
        path.display()
    );
    let mut file =
        File::open(path).with_context(|| format!("failed to open {label}: {}", path.display()))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {label}: {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= maximum,
        "{label} changed while reading and exceeded the {maximum}-byte limit"
    );
    Ok(bytes)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_format_is_canonical_and_lowercase() {
        assert!(is_sha256(&"ab".repeat(32)));
        assert!(!is_sha256(&"AB".repeat(32)));
        assert!(!is_sha256("short"));
    }

    #[test]
    fn untracked_installation_requires_an_exact_explicit_opt_in() {
        assert!(!strict_untracked_opt_in(None).unwrap());
        assert!(!strict_untracked_opt_in(Some("")).unwrap());
        assert!(!strict_untracked_opt_in(Some("false")).unwrap());
        assert!(strict_untracked_opt_in(Some("true")).unwrap());
        for invalid in [" ", "TRUE", "1", "yes", "on", " true ", "false "] {
            assert!(strict_untracked_opt_in(Some(invalid)).is_err());
        }

        let missing_opt_in = ensure_untracked_installation_allowed(false, false).unwrap_err();
        assert!(
            missing_opt_in
                .to_string()
                .contains("OSB_INSTALL_LOCK_DIGEST is required")
        );
        ensure_untracked_installation_allowed(true, false).unwrap();
        let delivery_bypass = ensure_untracked_installation_allowed(true, true).unwrap_err();
        assert!(delivery_bypass.to_string().contains("delivery-only"));
    }

    #[test]
    fn set_cardinality_detects_duplicate_dlc_environment_values() {
        let values = ["org.example.one", "org.example.one"];
        assert_ne!(
            values.iter().copied().collect::<BTreeSet<_>>().len(),
            values.len()
        );
    }

    #[test]
    fn tracked_startup_rejects_a_rehashed_lock_with_forged_bundled_manifest_digest() {
        let mut lock =
            InstallationLock::from_json(include_str!("../../../osb.lock.example.json")).unwrap();
        lock.dlcs[0].manifest_sha256 = "f".repeat(64);
        lock.refresh_digest().unwrap();
        lock.validate().unwrap();

        let error = verify_bundled_dlc_bytes(&lock).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("manifest bytes compiled into this server")
        );
    }

    #[test]
    fn home_curation_manifest_remains_compatible_with_deployed_0_1_0_locks() {
        let (_, _, manifest) = BUNDLED_OFFICIAL_MANIFESTS
            .iter()
            .find(|(id, _, _)| *id == "org.open-soverign-blog.home-curation")
            .expect("bundled home-curation manifest");
        assert_eq!(
            format!("{:x}", Sha256::digest(manifest.as_bytes())),
            "7e013273f9e65bb51fee3642d423585c99d46757f73566c2e3324ee0009186a3",
            "changing bundled 0.1.0 manifest bytes invalidates existing installation locks; add an explicit lock migration before changing them",
        );
    }
}
