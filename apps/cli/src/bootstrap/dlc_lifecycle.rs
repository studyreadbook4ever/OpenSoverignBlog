use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions, Permissions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand};
use osb_plugin_api::{
    DlcHistoryAction, DlcHistoryRecord, InstallationIntent, InstallationLock, InstalledDlc,
    InstalledDlcSourceKind, PLUGIN_API_VERSION, PluginManifest, RequestedDlc, RetainedDlcState,
};
use osb_storage_sqlite::DATABASE_SCHEMA_VERSION;
use semver::{Version, VersionReq};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use super::{
    CONFIG_SCHEMA, INSTALL_LOCK, INSTALL_MANIFEST, OfficialDlc, find_official_dlc,
    verify_bundled_official_manifest_bytes, verify_intent_lock_pair,
};

const INTENT_LIMIT: u64 = 512 * 1024;
const LOCK_LIMIT: u64 = 2 * 1024 * 1024;
const ENV_LIMIT: u64 = 1024 * 1024;
const MANAGED_ENV_KEYS: [&str; 3] = ["OSB_DLC_IDS", "OSB_FEATURES", "OSB_INSTALL_LOCK_DIGEST"];

#[derive(Debug, Args)]
pub(super) struct DlcArgs {
    /// Human-owned installation intent.
    #[arg(
        long,
        default_value = INSTALL_MANIFEST,
        env = "OSB_INSTALL_MANIFEST",
        global = true
    )]
    intent: PathBuf,
    /// Exact machine-generated installation lock.
    #[arg(
        long,
        default_value = INSTALL_LOCK,
        env = "OSB_INSTALL_LOCK",
        global = true
    )]
    lock: PathBuf,
    /// Deployment environment. Defaults to `.env` beside the intent.
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
    #[command(subcommand)]
    action: DlcAction,
}

#[derive(Debug, Subcommand)]
enum DlcAction {
    /// List installed DLCs, or the complete bundled official catalog.
    List {
        /// Include official DLCs that are available but not installed.
        #[arg(long)]
        available: bool,
        /// Emit stable JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Add and enable one bundled official DLC.
    #[command(visible_alias = "install")]
    Add {
        /// Stable alias/full id, optionally followed by @SEMVER_REQ.
        #[arg(value_name = "DLC[@SEMVER_REQ]")]
        dlc: String,
        /// SemVer request; conflicts with an @SEMVER_REQ suffix.
        #[arg(long, value_name = "SEMVER_REQ")]
        version: Option<String>,
    },
    /// Enable an installed DLC without deleting or recreating its state.
    Enable {
        #[arg(value_name = "DLC")]
        dlc: String,
    },
    /// Disable an installed DLC while retaining its state and content.
    Disable {
        #[arg(value_name = "DLC")]
        dlc: String,
    },
    /// Re-resolve one or every installed DLC against this engine's official catalog.
    Upgrade {
        /// Stable alias/full id. Omit to upgrade every installed official DLC.
        #[arg(value_name = "DLC")]
        dlc: Option<String>,
        /// Replacement SemVer request. Requires a single DLC target.
        #[arg(long, value_name = "SEMVER_REQ", requires = "dlc")]
        version: Option<String>,
    },
    /// Candidate-upgrade reconciliation of the engine tuple and every requested DLC.
    Reconcile {
        /// Engine version currently recorded in the source lock.
        #[arg(long)]
        from: String,
        /// Candidate engine version; must equal this CLI binary.
        #[arg(long)]
        to: String,
        /// Auditable release/artifact source label.
        #[arg(long)]
        source: String,
        /// Optional verified candidate artifact SHA-256.
        #[arg(long)]
        artifact_sha256: Option<String>,
    },
    /// Remove a DLC from composition while retaining its database state/content.
    Remove {
        #[arg(value_name = "DLC")]
        dlc: String,
    },
}

#[derive(Debug)]
struct Deployment {
    intent_path: PathBuf,
    lock_path: PathBuf,
    env_path: PathBuf,
    intent_original: Vec<u8>,
    lock_original: Vec<u8>,
    env_original: Vec<u8>,
    env_values: BTreeMap<String, String>,
    intent: InstallationIntent,
    lock: InstallationLock,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListEntry {
    alias: &'static str,
    id: &'static str,
    installed: bool,
    enabled: bool,
    requested_version: Option<String>,
    version: String,
    manifest_sha256: String,
}

#[derive(Debug)]
struct Replacement {
    path: PathBuf,
    label: &'static str,
    original: Vec<u8>,
    replacement: Vec<u8>,
}

#[derive(Debug)]
struct PreparedReplacement {
    path: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
    original: Vec<u8>,
}

pub(super) fn run(args: DlcArgs) -> Result<()> {
    match &args.action {
        DlcAction::List { available, json } => list(&args.intent, &args.lock, *available, *json),
        DlcAction::Add { dlc, version } => {
            let mut deployment = load_deployment(&args)?;
            ensure_cli_matches_lock(&deployment.lock)?;
            let (official, requirement) = parse_dlc_spec(dlc, version.as_deref())?;
            ensure!(
                !deployment
                    .intent
                    .dlcs
                    .iter()
                    .any(|requested| requested.id == official.id),
                "DLC {} is already installed; use enable or upgrade",
                official.id
            );
            let mut resolved = resolve_official(official, &requirement, true, current_engine())?;
            if let Some(index) = deployment
                .lock
                .retained_dlcs
                .iter()
                .position(|retained| retained.id == official.id)
            {
                let retained = deployment.lock.retained_dlcs.remove(index);
                resolved.state_version = retained.state_version;
                resolved.applied_migrations = retained.applied_migrations;
            }
            deployment.intent.dlcs.push(RequestedDlc {
                id: official.id.into(),
                version: requirement,
                enabled: true,
            });
            push_history(
                &mut deployment.lock,
                DlcHistoryAction::Installed,
                official.id,
                None,
                Some(resolved.version.clone()),
                current_engine(),
            );
            deployment.lock.dlcs.push(resolved);
            normalize_contracts(&mut deployment);
            persist(&mut deployment)?;
            println!(
                "official DLC installed and enabled: {} · lockDigest={}",
                official.id, deployment.lock.lock_digest
            );
            Ok(())
        }
        DlcAction::Enable { dlc } => set_enabled(load_deployment(&args)?, dlc, true),
        DlcAction::Disable { dlc } => set_enabled(load_deployment(&args)?, dlc, false),
        DlcAction::Upgrade { dlc, version } => {
            upgrade(load_deployment(&args)?, dlc.as_deref(), version.as_deref())
        }
        DlcAction::Reconcile {
            from,
            to,
            source,
            artifact_sha256,
        } => reconcile(
            load_deployment(&args)?,
            from,
            to,
            source.clone(),
            artifact_sha256.clone(),
        ),
        DlcAction::Remove { dlc } => remove(load_deployment(&args)?, dlc),
    }
}

fn list(intent_path: &Path, lock_path: &Path, available: bool, json: bool) -> Result<()> {
    let intent_source = read_regular_bounded(intent_path, INTENT_LIMIT, "installation intent")?;
    let lock_source = read_regular_bounded(lock_path, LOCK_LIMIT, "installation lock")?;
    let intent = InstallationIntent::from_toml(as_utf8(&intent_source, "installation intent")?)
        .map_err(anyhow::Error::msg)?;
    let lock = InstallationLock::from_json(as_utf8(&lock_source, "installation lock")?)
        .map_err(anyhow::Error::msg)?;
    verify_intent_lock_pair(&intent, &lock)?;
    ensure_bundled_official_records(&lock)?;

    let entries = super::OFFICIAL_DLCS
        .iter()
        .map(|official| -> Result<Option<ListEntry>> {
            let installed = lock.dlcs.iter().find(|dlc| dlc.id == official.id);
            if installed.is_none() && !available {
                return Ok(None);
            }
            let manifest = parse_official_manifest(*official)?;
            Ok(Some(ListEntry {
                alias: official.alias,
                id: official.id,
                installed: installed.is_some(),
                enabled: installed.is_some_and(|dlc| dlc.enabled),
                requested_version: installed.map(|dlc| dlc.requested_version.clone()),
                version: installed
                    .map(|dlc| dlc.version.clone())
                    .unwrap_or(manifest.version),
                manifest_sha256: installed
                    .map(|dlc| dlc.manifest_sha256.clone())
                    .unwrap_or_else(|| official_manifest_digest(*official)),
            }))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if entries.is_empty() {
        println!("no official DLC is installed");
    } else {
        println!("ALIAS\tSTATUS\tVERSION\tREQUEST\tID");
        for entry in entries {
            let status = if entry.enabled {
                "enabled"
            } else if entry.installed {
                "disabled"
            } else {
                "available"
            };
            println!(
                "{}\t{}\t{}\t{}\t{}",
                entry.alias,
                status,
                entry.version,
                entry.requested_version.as_deref().unwrap_or("-"),
                entry.id
            );
        }
    }
    Ok(())
}

fn load_deployment(args: &DlcArgs) -> Result<Deployment> {
    let env_path = args.env_file.clone().unwrap_or_else(|| {
        args.intent
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".env")
    });
    ensure_distinct_paths(&args.intent, &args.lock, &env_path)?;
    let intent_original = read_regular_bounded(&args.intent, INTENT_LIMIT, "installation intent")?;
    let lock_original = read_regular_bounded(&args.lock, LOCK_LIMIT, "installation lock")?;
    let env_original = read_regular_bounded(&env_path, ENV_LIMIT, "deployment environment")?;
    #[cfg(unix)]
    ensure!(
        fs::metadata(&env_path)?.permissions().mode() & 0o077 == 0,
        "refusing to update an unprotected .env; remove group/world permissions first"
    );
    let intent = InstallationIntent::from_toml(as_utf8(&intent_original, "installation intent")?)
        .map_err(anyhow::Error::msg)?;
    let lock = InstallationLock::from_json(as_utf8(&lock_original, "installation lock")?)
        .map_err(anyhow::Error::msg)?;
    verify_intent_lock_pair(&intent, &lock)?;
    ensure_bundled_official_records(&lock)?;
    let env_values = project_environment(&env_original)?;
    verify_environment_projection(&lock, &env_values)?;
    Ok(Deployment {
        intent_path: args.intent.clone(),
        lock_path: args.lock.clone(),
        env_path,
        intent_original,
        lock_original,
        env_original,
        env_values,
        intent,
        lock,
    })
}

fn set_enabled(mut deployment: Deployment, raw: &str, enabled: bool) -> Result<()> {
    ensure_cli_matches_lock(&deployment.lock)?;
    let official = official_target(raw)?;
    if !enabled {
        ensure_not_required_by_runtime(&deployment, official)?;
        ensure_no_enabled_dependents(&deployment.lock, official.id)?;
    }
    let requested = deployment
        .intent
        .dlcs
        .iter_mut()
        .find(|requested| requested.id == official.id)
        .with_context(|| format!("DLC {} is not installed", official.id))?;
    let installed = deployment
        .lock
        .dlcs
        .iter_mut()
        .find(|installed| installed.id == official.id)
        .expect("verified intent and lock have the same DLC ids");
    if installed.enabled == enabled {
        println!(
            "official DLC is already {}: {}",
            if enabled { "enabled" } else { "disabled" },
            official.id
        );
        return Ok(());
    }
    requested.enabled = enabled;
    installed.enabled = enabled;
    let version = installed.version.clone();
    push_history(
        &mut deployment.lock,
        if enabled {
            DlcHistoryAction::Enabled
        } else {
            DlcHistoryAction::Disabled
        },
        official.id,
        Some(version.clone()),
        Some(version),
        current_engine(),
    );
    normalize_contracts(&mut deployment);
    persist(&mut deployment)?;
    println!(
        "official DLC {}: {} · state/content retained · lockDigest={}",
        if enabled { "enabled" } else { "disabled" },
        official.id,
        deployment.lock.lock_digest
    );
    Ok(())
}

fn upgrade(mut deployment: Deployment, target: Option<&str>, version: Option<&str>) -> Result<()> {
    ensure_cli_matches_lock(&deployment.lock)?;
    let target_id = target.map(official_target).transpose()?.map(|dlc| dlc.id);
    ensure!(
        target_id.is_some() || version.is_none(),
        "--version requires a single DLC target"
    );
    if let Some(id) = target_id {
        ensure!(
            deployment.intent.dlcs.iter().any(|dlc| dlc.id == id),
            "DLC {id} is not installed"
        );
    }

    let mut changed = false;
    let mut transitions = Vec::new();
    let old_by_id = deployment
        .lock
        .dlcs
        .iter()
        .map(|dlc| (dlc.id.clone(), dlc.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut resolved = Vec::with_capacity(deployment.intent.dlcs.len());
    for request in &mut deployment.intent.dlcs {
        let old = old_by_id
            .get(&request.id)
            .expect("verified lock contains every intent request");
        if target_id.is_some_and(|id| id != request.id) {
            resolved.push(old.clone());
            continue;
        }
        if let Some(requirement) = version {
            RequestedDlc {
                id: request.id.clone(),
                version: requirement.into(),
                enabled: request.enabled,
            }
            .validate()
            .map_err(anyhow::Error::msg)?;
            request.version = requirement.into();
        }
        let official = find_official_dlc(&request.id)
            .expect("official records were checked while loading the deployment");
        let mut new = resolve_official(
            official,
            &request.version,
            request.enabled,
            current_engine(),
        )?;
        carry_state(old, &mut new);
        ensure_immutable_release(old, &new)?;
        let old_version = Version::parse(&old.version).context("locked DLC version is invalid")?;
        let new_version = Version::parse(&new.version).context("bundled DLC version is invalid")?;
        ensure!(
            new_version >= old_version,
            "DLC {} would downgrade from {} to {}; candidate catalogs may only advance versions",
            request.id,
            old.version,
            new.version
        );
        if new_version > old_version {
            transitions.push((request.id.clone(), old.version.clone(), new.version.clone()));
        }
        changed |= &new != old || request.version != old.requested_version;
        resolved.push(new);
    }
    ensure!(
        target_id.is_some() || !deployment.intent.dlcs.is_empty(),
        "no DLC is installed"
    );
    if !changed {
        println!(
            "official DLC lock is already current for engine {}",
            current_engine()
        );
        return Ok(());
    }
    deployment.lock.dlcs = resolved;
    for (id, from, to) in transitions {
        push_history(
            &mut deployment.lock,
            DlcHistoryAction::Upgraded,
            &id,
            Some(from),
            Some(to),
            current_engine(),
        );
    }
    normalize_contracts(&mut deployment);
    persist(&mut deployment)?;
    println!(
        "official DLC lock reconciled with engine {} · lockDigest={}",
        current_engine(),
        deployment.lock.lock_digest
    );
    Ok(())
}

fn reconcile(
    mut deployment: Deployment,
    from: &str,
    to: &str,
    source: String,
    artifact_sha256: Option<String>,
) -> Result<()> {
    ensure!(
        to == current_engine(),
        "candidate reconciliation must run from target CLI {}; received --to {to}",
        current_engine()
    );
    ensure!(
        deployment.lock.engine.version == from,
        "engine reconciliation source differs from the current lock: expected {}, received {from}",
        deployment.lock.engine.version
    );
    let from_version = Version::parse(from).context("--from must be SemVer")?;
    let to_version = Version::parse(to).context("--to must be SemVer")?;
    ensure!(
        to_version > from_version,
        "candidate engine version must be greater than the source version"
    );
    if let Some(digest) = artifact_sha256.as_deref() {
        ensure!(
            valid_sha256(digest),
            "--artifact-sha256 must be 64 lowercase hex characters"
        );
    }
    ensure!(
        !source.trim().is_empty() && source.len() <= 2048,
        "--source is invalid"
    );

    let old_by_id = deployment
        .lock
        .dlcs
        .iter()
        .map(|dlc| (dlc.id.clone(), dlc.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut resolved = Vec::with_capacity(deployment.intent.dlcs.len());
    let mut transitions = Vec::new();
    for request in &deployment.intent.dlcs {
        let official = find_official_dlc(&request.id).with_context(|| {
            format!(
                "requested DLC {} is not bundled by candidate engine {}; remote arbitrary code is never resolved",
                request.id,
                current_engine()
            )
        })?;
        let old = old_by_id
            .get(&request.id)
            .expect("verified lock contains every intent request");
        let mut new = resolve_official(official, &request.version, request.enabled, to)?;
        carry_state(old, &mut new);
        ensure_immutable_release(old, &new)?;
        let old_version = Version::parse(&old.version).context("locked DLC version is invalid")?;
        let new_version = Version::parse(&new.version).context("bundled DLC version is invalid")?;
        ensure!(
            new_version >= old_version,
            "candidate engine would downgrade DLC {} from {} to {}",
            request.id,
            old.version,
            new.version
        );
        if new_version > old_version {
            transitions.push((request.id.clone(), old.version.clone(), new.version.clone()));
        }
        resolved.push(new);
    }
    deployment.lock.engine.version = to.into();
    deployment.lock.engine.config_schema_version = CONFIG_SCHEMA.into();
    deployment.lock.engine.database_schema_version = DATABASE_SCHEMA_VERSION;
    deployment.lock.engine.plugin_api = PLUGIN_API_VERSION.into();
    deployment.lock.engine.source = source;
    deployment.lock.engine.artifact_sha256 = artifact_sha256;
    deployment.lock.dlcs = resolved;
    for (id, old, new) in transitions {
        push_history(
            &mut deployment.lock,
            DlcHistoryAction::Upgraded,
            &id,
            Some(old),
            Some(new),
            to,
        );
    }
    normalize_contracts(&mut deployment);
    persist(&mut deployment)?;
    println!(
        "candidate installation reconciled: {from} -> {to} · {} official DLC(s) · lockDigest={}",
        deployment.lock.dlcs.len(),
        deployment.lock.lock_digest
    );
    Ok(())
}

fn remove(mut deployment: Deployment, raw: &str) -> Result<()> {
    ensure_cli_matches_lock(&deployment.lock)?;
    let official = official_target(raw)?;
    ensure_not_required_by_runtime(&deployment, official)?;
    ensure_no_enabled_dependents(&deployment.lock, official.id)?;
    let index = deployment
        .lock
        .dlcs
        .iter()
        .position(|installed| installed.id == official.id)
        .with_context(|| format!("DLC {} is not installed", official.id))?;
    let removed = deployment.lock.dlcs.remove(index);
    deployment
        .intent
        .dlcs
        .retain(|requested| requested.id != official.id);
    deployment
        .lock
        .retained_dlcs
        .retain(|retained| retained.id != official.id);
    deployment.lock.retained_dlcs.push(RetainedDlcState {
        id: removed.id.clone(),
        removed_version: removed.version.clone(),
        state_version: removed.state_version,
        applied_migrations: removed.applied_migrations.clone(),
    });
    push_history(
        &mut deployment.lock,
        DlcHistoryAction::Removed,
        official.id,
        Some(removed.version),
        None,
        current_engine(),
    );
    normalize_contracts(&mut deployment);
    persist(&mut deployment)?;
    println!(
        "official DLC removed from composition: {} · database state/content retained · lockDigest={}",
        official.id, deployment.lock.lock_digest
    );
    Ok(())
}

fn parse_dlc_spec(raw: &str, explicit: Option<&str>) -> Result<(OfficialDlc, String)> {
    let (name, suffix) = raw
        .rsplit_once('@')
        .map_or((raw, None), |(name, version)| (name, Some(version)));
    ensure!(!name.is_empty(), "DLC id is required before @SEMVER_REQ");
    ensure!(
        explicit.is_none() || suffix.is_none(),
        "provide a SemVer request either with @SEMVER_REQ or --version, not both"
    );
    let official = official_target(name)?;
    let manifest = parse_official_manifest(official)?;
    let requirement = explicit
        .or(suffix)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("^{}", manifest.version));
    RequestedDlc {
        id: official.id.into(),
        version: requirement.clone(),
        enabled: true,
    }
    .validate()
    .map_err(anyhow::Error::msg)?;
    Ok((official, requirement))
}

fn official_target(raw: &str) -> Result<OfficialDlc> {
    ensure!(
        !raw.contains('/') && !raw.contains(':') && !raw.contains('\\'),
        "DLC targets must be a stable bundled alias or full id; paths and URLs are rejected"
    );
    find_official_dlc(raw).with_context(|| {
        format!(
            "unknown official DLC {raw:?}; only manifests bundled into this CLI may be resolved"
        )
    })
}

fn resolve_official(
    official: OfficialDlc,
    requirement: &str,
    enabled: bool,
    engine_version: &str,
) -> Result<InstalledDlc> {
    let manifest = parse_official_manifest(official)?;
    ensure!(
        manifest.id == official.id,
        "bundled DLC catalog id mismatch"
    );
    let requirement_parsed = VersionReq::parse(requirement)
        .with_context(|| format!("invalid SemVer request for {}", official.id))?;
    let version = Version::parse(&manifest.version).context("bundled DLC version is invalid")?;
    ensure!(
        requirement_parsed.matches(&version),
        "bundled DLC {} {} does not satisfy requested range {}",
        official.id,
        manifest.version,
        requirement
    );
    ensure!(
        manifest
            .supports_core(engine_version)
            .map_err(anyhow::Error::msg)?,
        "bundled DLC {} {} is incompatible with engine {} ({})",
        official.id,
        manifest.version,
        engine_version,
        manifest
            .core_compatibility
            .as_deref()
            .unwrap_or("unspecified")
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
    Ok(InstalledDlc {
        id: manifest.id,
        requested_version: requirement.into(),
        version: manifest.version,
        core_compatibility: manifest
            .core_compatibility
            .unwrap_or_else(|| format!("={engine_version}")),
        manifest_version: manifest.manifest_version,
        plugin_api: manifest.plugin_api,
        source_kind: InstalledDlcSourceKind::Bundled,
        source: official.source.into(),
        manifest_sha256: official_manifest_digest(official),
        artifact_sha256: None,
        enabled,
        approved_capabilities: capabilities,
        config_sha256,
        state_version: manifest.state.as_ref().map(|state| state.version),
        applied_migrations: Vec::new(),
    })
}

fn parse_official_manifest(official: OfficialDlc) -> Result<PluginManifest> {
    PluginManifest::from_toml(official.manifest)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("bundled official manifest {} is invalid", official.id))
}

fn official_manifest_digest(official: OfficialDlc) -> String {
    format!("{:x}", Sha256::digest(official.manifest.as_bytes()))
}

fn ensure_bundled_official_records(lock: &InstallationLock) -> Result<()> {
    verify_bundled_official_manifest_bytes(lock)?;
    for installed in &lock.dlcs {
        let official = find_official_dlc(&installed.id).with_context(|| {
            format!(
                "DLC {} is not in this CLI's bundled official catalog; lifecycle commands never load remote arbitrary code",
                installed.id
            )
        })?;
        ensure!(
            installed.source_kind == InstalledDlcSourceKind::Bundled
                && installed.source == official.source,
            "DLC {} is not locked to its bundled official manifest; file/HTTPS code is rejected",
            installed.id
        );
    }
    Ok(())
}

fn ensure_cli_matches_lock(lock: &InstallationLock) -> Result<()> {
    ensure!(
        lock.engine.version == current_engine()
            && lock.engine.config_schema_version == CONFIG_SCHEMA
            && lock.engine.database_schema_version == DATABASE_SCHEMA_VERSION
            && lock.engine.plugin_api == PLUGIN_API_VERSION,
        "ordinary DLC maintenance requires the matching engine CLI {}; use `installation dlc reconcile` from a candidate CLI during an engine upgrade",
        current_engine()
    );
    Ok(())
}

fn carry_state(old: &InstalledDlc, new: &mut InstalledDlc) {
    new.enabled = old.enabled;
    new.applied_migrations = old.applied_migrations.clone();
}

fn ensure_immutable_release(old: &InstalledDlc, new: &InstalledDlc) -> Result<()> {
    if old.version == new.version {
        ensure!(
            old.manifest_sha256 == new.manifest_sha256,
            "official DLC {} {} has different manifest bytes in the candidate catalog; published versions are immutable and require a version bump",
            old.id,
            old.version
        );
    }
    Ok(())
}

fn normalize_contracts(deployment: &mut Deployment) {
    deployment
        .intent
        .dlcs
        .sort_by(|left, right| left.id.cmp(&right.id));
    deployment
        .lock
        .dlcs
        .sort_by(|left, right| left.id.cmp(&right.id));
    deployment
        .lock
        .retained_dlcs
        .sort_by(|left, right| left.id.cmp(&right.id));
}

fn push_history(
    lock: &mut InstallationLock,
    action: DlcHistoryAction,
    id: &str,
    from_version: Option<String>,
    to_version: Option<String>,
    engine_version: &str,
) {
    lock.history.push(DlcHistoryRecord {
        sequence: u64::try_from(lock.history.len()).expect("DLC history length fits u64") + 1,
        action,
        dlc_id: id.into(),
        from_version,
        to_version,
        engine_version: engine_version.into(),
    });
}

fn persist(deployment: &mut Deployment) -> Result<()> {
    validate_enabled_dependencies(&deployment.lock)?;
    deployment
        .lock
        .refresh_digest()
        .map_err(anyhow::Error::msg)?;
    verify_intent_lock_pair(&deployment.intent, &deployment.lock)?;
    let intent = deployment
        .intent
        .to_toml_pretty()
        .map_err(anyhow::Error::msg)?
        .into_bytes();
    let lock = deployment
        .lock
        .to_pretty_json()
        .map_err(anyhow::Error::msg)?
        .into_bytes();
    let (ids, features) = enabled_composition(&deployment.lock)?;
    let env = replace_environment_values(
        &deployment.env_original,
        &BTreeMap::from([
            ("OSB_DLC_IDS", ids),
            ("OSB_FEATURES", features),
            (
                "OSB_INSTALL_LOCK_DIGEST",
                deployment.lock.lock_digest.clone(),
            ),
        ]),
    )?;
    let replacements = [
        Replacement {
            path: deployment.intent_path.clone(),
            label: "installation intent",
            original: deployment.intent_original.clone(),
            replacement: intent,
        },
        Replacement {
            path: deployment.lock_path.clone(),
            label: "installation lock",
            original: deployment.lock_original.clone(),
            replacement: lock,
        },
        Replacement {
            path: deployment.env_path.clone(),
            label: "deployment environment",
            original: deployment.env_original.clone(),
            replacement: env,
        },
    ];
    transactional_replace(&replacements, |_| Ok(()))?;
    Ok(())
}

fn validate_enabled_dependencies(lock: &InstallationLock) -> Result<()> {
    let installed = lock
        .dlcs
        .iter()
        .map(|dlc| (dlc.id.as_str(), dlc))
        .collect::<BTreeMap<_, _>>();
    for dlc in lock.dlcs.iter().filter(|dlc| dlc.enabled) {
        let official = find_official_dlc(&dlc.id).expect("official lock was verified");
        let manifest = parse_official_manifest(official)?;
        for dependency in manifest
            .dependencies
            .iter()
            .filter(|dependency| !dependency.optional)
        {
            let resolved = installed.get(dependency.id.as_str()).with_context(|| {
                format!(
                    "enabled DLC {} requires missing DLC {}",
                    dlc.id, dependency.id
                )
            })?;
            ensure!(
                resolved.enabled,
                "enabled DLC {} requires enabled DLC {}",
                dlc.id,
                dependency.id
            );
            let requirement = VersionReq::parse(&dependency.version)
                .context("official DLC dependency range is invalid")?;
            let version =
                Version::parse(&resolved.version).context("locked DLC version is invalid")?;
            ensure!(
                requirement.matches(&version),
                "enabled DLC {} requires {} {}, but {} is locked",
                dlc.id,
                dependency.id,
                dependency.version,
                resolved.version
            );
        }
    }
    Ok(())
}

fn ensure_no_enabled_dependents(lock: &InstallationLock, target: &str) -> Result<()> {
    for installed in lock
        .dlcs
        .iter()
        .filter(|dlc| dlc.enabled && dlc.id != target)
    {
        let official = find_official_dlc(&installed.id).expect("official lock was verified");
        let manifest = parse_official_manifest(official)?;
        if manifest
            .dependencies
            .iter()
            .any(|dependency| !dependency.optional && dependency.id == target)
        {
            bail!(
                "cannot disable/remove {target}; enabled DLC {} depends on it",
                installed.id
            );
        }
    }
    Ok(())
}

fn ensure_not_required_by_runtime(deployment: &Deployment, target: OfficialDlc) -> Result<()> {
    let value_is_true = |name: &str| {
        deployment
            .env_values
            .get(name)
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    };
    match target.runtime_feature {
        "comments" if value_is_true("OSB_COMMENTS") => bail!(
            "cannot disable/remove comments while OSB_COMMENTS=true; change the runtime configuration first"
        ),
        "rbac" if value_is_true("OSB_COLLABORATION") => bail!(
            "cannot disable/remove rbac while OSB_COLLABORATION=true; change the runtime configuration first"
        ),
        "external_auth"
            if deployment.intent.selection.admin_auth
                == osb_plugin_api::InstallationAdminAuth::External
                || deployment
                    .env_values
                    .get("OSB_AUTH_MODE")
                    .is_some_and(|mode| matches!(mode.as_str(), "oauth" | "local_and_oauth")) =>
        {
            bail!(
                "cannot disable/remove external-auth while administrator or reader OAuth is selected"
            )
        }
        _ => Ok(()),
    }
}

fn enabled_composition(lock: &InstallationLock) -> Result<(String, String)> {
    let mut ids = Vec::new();
    let mut features = Vec::new();
    for installed in lock.dlcs.iter().filter(|dlc| dlc.enabled) {
        let official = find_official_dlc(&installed.id)
            .with_context(|| format!("unknown bundled DLC {}", installed.id))?;
        ids.push(installed.id.as_str());
        features.push(official.runtime_feature);
    }
    Ok((
        ids.join(","),
        if features.is_empty() {
            "none".into()
        } else {
            features.join(",")
        },
    ))
}

fn verify_environment_projection(
    lock: &InstallationLock,
    values: &BTreeMap<String, String>,
) -> Result<()> {
    let (ids, features) = enabled_composition(lock)?;
    for (name, expected) in [
        ("OSB_DLC_IDS", ids.as_str()),
        ("OSB_FEATURES", features.as_str()),
        ("OSB_INSTALL_LOCK_DIGEST", lock.lock_digest.as_str()),
    ] {
        let actual = values
            .get(name)
            .with_context(|| format!("deployment environment is missing {name}"))?;
        ensure!(
            actual == expected,
            "deployment environment {name} differs from the verified installation lock"
        );
    }
    Ok(())
}

fn project_environment(source: &[u8]) -> Result<BTreeMap<String, String>> {
    let source = as_utf8(source, "deployment environment")?;
    let wanted = MANAGED_ENV_KEYS
        .into_iter()
        .chain(["OSB_AUTH_MODE", "OSB_COMMENTS", "OSB_COLLABORATION"])
        .collect::<BTreeSet<_>>();
    let mut projected = BTreeMap::new();
    for (index, line) in source.lines().enumerate() {
        ensure!(
            line.len() <= 16 * 1024,
            ".env line {} is too long",
            index + 1
        );
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, raw)) = line.split_once('=') else {
            continue;
        };
        if !wanted.contains(name) {
            continue;
        }
        ensure!(
            !projected.contains_key(name),
            ".env contains duplicate managed value {name}"
        );
        projected.insert(name.into(), unquote(raw)?.into());
    }
    Ok(projected)
}

fn unquote(value: &str) -> Result<&str> {
    if value.starts_with('\'') || value.starts_with('"') {
        let quote = value.as_bytes()[0];
        ensure!(
            value.len() >= 2 && value.as_bytes()[value.len() - 1] == quote,
            "managed environment value has an unterminated quote"
        );
        Ok(&value[1..value.len() - 1])
    } else {
        Ok(value)
    }
}

fn replace_environment_values(
    original: &[u8],
    updates: &BTreeMap<&str, String>,
) -> Result<Vec<u8>> {
    let source = as_utf8(original, "deployment environment")?;
    let mut output = String::with_capacity(source.len() + 256);
    let mut replaced = BTreeSet::new();
    for segment in source.split_inclusive('\n') {
        let (without_lf, ending) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        let (line, ending) = without_lf
            .strip_suffix('\r')
            .map_or((without_lf, ending.to_owned()), |line| {
                (line, format!("\r{ending}"))
            });
        let name = line.split_once('=').map(|(name, _)| name);
        if let Some((name, value)) = name.and_then(|name| updates.get_key_value(name)) {
            ensure!(
                replaced.insert(*name),
                ".env contains duplicate managed value {name}"
            );
            ensure!(
                value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric()
                        || matches!(
                            byte,
                            b'.' | b','
                                | b'-'
                                | b'_'
                                | b':'
                                | b'/'
                                | b'='
                                | b'<'
                                | b'>'
                                | b'^'
                                | b' '
                        )
                }),
                "refusing to render an unsafe managed environment value"
            );
            output.push_str(name);
            output.push('=');
            output.push_str(value);
            output.push_str(&ending);
        } else {
            output.push_str(segment);
        }
    }
    for name in updates.keys() {
        ensure!(
            replaced.contains(name),
            ".env is missing managed value {name}"
        );
    }
    Ok(output.into_bytes())
}

fn transactional_replace<F>(replacements: &[Replacement], mut after_replace: F) -> Result<()>
where
    F: FnMut(usize) -> Result<()>,
{
    ensure!(
        !replacements.is_empty(),
        "transaction requires at least one file"
    );
    let nonce = Uuid::now_v7().simple().to_string();
    let mut prepared = Vec::with_capacity(replacements.len());
    let prepare_result = (|| -> Result<()> {
        for replacement in replacements {
            let current = read_regular_bounded(
                &replacement.path,
                u64::try_from(replacement.original.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
                replacement.label,
            )?;
            ensure!(
                current == replacement.original,
                "{} changed while the DLC transaction was being prepared",
                replacement.label
            );
            let metadata = fs::metadata(&replacement.path)?;
            let parent = replacement.path.parent().unwrap_or_else(|| Path::new("."));
            let name = replacement
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .context("transaction target name is not UTF-8")?;
            let stage = parent.join(format!(".{name}.{nonce}.stage"));
            let backup = parent.join(format!(".{name}.{nonce}.backup"));
            write_staged(&stage, &replacement.replacement, metadata.permissions())?;
            if let Err(error) = write_staged(&backup, &replacement.original, metadata.permissions())
            {
                let _ = fs::remove_file(&stage);
                return Err(error);
            }
            prepared.push(PreparedReplacement {
                path: replacement.path.clone(),
                stage,
                backup,
                original: replacement.original.clone(),
            });
        }
        sync_parent_directories(prepared.iter().flat_map(|item| [&item.stage, &item.backup]))?;
        Ok(())
    })();
    if let Err(error) = prepare_result {
        cleanup_transaction_files(&prepared);
        return Err(error);
    }

    let mut committed = 0usize;
    let commit_result = (|| -> Result<()> {
        for (index, item) in prepared.iter().enumerate() {
            fs::rename(&item.stage, &item.path).with_context(|| {
                format!(
                    "failed to commit transaction target {}",
                    item.path.display()
                )
            })?;
            committed = index + 1;
            after_replace(committed)?;
        }
        sync_parent_directories(prepared.iter().map(|item| &item.path))?;
        Ok(())
    })();

    if let Err(error) = commit_result {
        let rollback = rollback_transaction(&prepared, committed);
        return match rollback {
            Ok(()) => {
                Err(error.context("DLC transaction rolled back without changing original bytes"))
            }
            Err(rollback_error) => Err(anyhow::anyhow!(
                "DLC transaction failed ({error:#}); rollback also failed ({rollback_error:#}); adjacent .backup files were retained for manual recovery"
            )),
        };
    }

    cleanup_transaction_files(&prepared);
    let _ = sync_parent_directories(prepared.iter().map(|item| &item.path));
    Ok(())
}

fn rollback_transaction(prepared: &[PreparedReplacement], committed: usize) -> Result<()> {
    for item in prepared.iter().take(committed) {
        fs::rename(&item.backup, &item.path).with_context(|| {
            format!(
                "failed to restore transaction target {}",
                item.path.display()
            )
        })?;
    }
    sync_parent_directories(prepared.iter().take(committed).map(|item| &item.path))?;
    for item in prepared.iter().take(committed) {
        ensure!(
            fs::read(&item.path)? == item.original,
            "rollback verification failed for {}",
            item.path.display()
        );
    }
    cleanup_transaction_files(prepared);
    Ok(())
}

fn cleanup_transaction_files(prepared: &[PreparedReplacement]) {
    for item in prepared {
        let _ = fs::remove_file(&item.stage);
        let _ = fs::remove_file(&item.backup);
    }
}

fn write_staged(path: &Path, bytes: &[u8], permissions: Permissions) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(permissions.mode() & 0o777);
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create transaction file {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn sync_parent_directories<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Result<()> {
    let parents = paths
        .into_iter()
        .map(|path| {
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        })
        .collect::<BTreeSet<_>>();
    for parent in parents {
        OpenOptions::new()
            .read(true)
            .open(&parent)
            .with_context(|| format!("failed to open transaction directory {}", parent.display()))?
            .sync_all()?;
    }
    Ok(())
}

fn read_regular_bounded(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label} {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{label} must be a regular non-symlink file: {}",
        path.display()
    );
    ensure!(metadata.len() <= limit, "{label} exceeds its size limit");
    let bytes =
        fs::read(path).with_context(|| format!("failed to read {label} {}", path.display()))?;
    ensure!(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= limit,
        "{label} grew beyond its size limit while being read"
    );
    Ok(bytes)
}

fn ensure_distinct_paths(intent: &Path, lock: &Path, env: &Path) -> Result<()> {
    let canonical = [intent, lock, env]
        .into_iter()
        .map(|path| {
            path.canonicalize()
                .with_context(|| format!("failed to resolve transaction target {}", path.display()))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        canonical.len() == 3,
        "intent, lock, and environment must be three distinct regular files"
    );
    Ok(())
}

fn as_utf8<'a>(bytes: &'a [u8], label: &str) -> Result<&'a str> {
    std::str::from_utf8(bytes).with_context(|| format!("{label} is not UTF-8"))
}

fn current_engine() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use osb_plugin_api::{
        INSTALL_INTENT_SCHEMA_VERSION, INSTALL_LOCK_SCHEMA_VERSION, InstallationAdminAuth,
        InstallationCache, InstallationSelection, InstallationStyle, InstallationStyleKind,
        LockedEngine,
    };
    use tempfile::tempdir;

    fn fixture(root: &Path) -> Deployment {
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
        let official = find_official_dlc("seo").unwrap();
        let installed = resolve_official(official, "^0.1.0", true, current_engine()).unwrap();
        let intent = InstallationIntent {
            schema_version: INSTALL_INTENT_SCHEMA_VERSION.into(),
            installation_id: "018f0000-0000-7000-8000-000000000001".into(),
            site_id: "018f0000-0000-7000-8000-000000000002".into(),
            created_with: current_engine().into(),
            selection: selection.clone(),
            dlcs: vec![RequestedDlc {
                id: official.id.into(),
                version: "^0.1.0".into(),
                enabled: true,
            }],
        };
        let mut lock = InstallationLock {
            schema_version: INSTALL_LOCK_SCHEMA_VERSION.into(),
            installation_id: intent.installation_id.clone(),
            engine: LockedEngine {
                version: current_engine().into(),
                config_schema_version: CONFIG_SCHEMA.into(),
                database_schema_version: DATABASE_SCHEMA_VERSION,
                plugin_api: PLUGIN_API_VERSION.into(),
                source: "test".into(),
                artifact_sha256: None,
            },
            selection,
            dlcs: vec![installed],
            retained_dlcs: Vec::new(),
            history: vec![DlcHistoryRecord {
                sequence: 1,
                action: DlcHistoryAction::Installed,
                dlc_id: official.id.into(),
                from_version: None,
                to_version: Some("0.1.0".into()),
                engine_version: current_engine().into(),
            }],
            lock_digest: String::new(),
        };
        lock.refresh_digest().unwrap();
        let intent_path = root.join(INSTALL_MANIFEST);
        let lock_path = root.join(INSTALL_LOCK);
        let env_path = root.join(".env");
        fs::write(&intent_path, intent.to_toml_pretty().unwrap()).unwrap();
        fs::write(&lock_path, lock.to_pretty_json().unwrap()).unwrap();
        fs::write(
            &env_path,
            format!(
                "SECRET='do not reformat this = value'\nOSB_DLC_IDS={}\nOSB_FEATURES=seo\nOSB_INSTALL_LOCK_DIGEST={}\nOSB_AUTH_MODE=disabled\nOSB_COMMENTS=false\nOSB_COLLABORATION=false\n",
                official.id, lock.lock_digest
            ),
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&env_path, fs::Permissions::from_mode(0o600)).unwrap();
        load_deployment(&DlcArgs {
            intent: intent_path,
            lock: lock_path,
            env_file: Some(env_path),
            action: DlcAction::List {
                available: false,
                json: false,
            },
        })
        .unwrap()
    }

    #[test]
    fn remove_and_readd_preserve_the_host_owned_migration_ledger() {
        let root = tempdir().unwrap();
        let mut deployment = fixture(root.path());
        deployment.lock.dlcs[0].state_version = Some(7);
        deployment.lock.dlcs[0].applied_migrations = vec!["seo.state.v7".into()];
        deployment.lock.refresh_digest().unwrap();
        deployment.lock_original = deployment.lock.to_pretty_json().unwrap().into_bytes();
        fs::write(&deployment.lock_path, &deployment.lock_original).unwrap();
        deployment.env_original = replace_environment_values(
            &deployment.env_original,
            &BTreeMap::from([(
                "OSB_INSTALL_LOCK_DIGEST",
                deployment.lock.lock_digest.clone(),
            )]),
        )
        .unwrap();
        fs::write(&deployment.env_path, &deployment.env_original).unwrap();
        deployment.env_values = project_environment(&deployment.env_original).unwrap();

        set_enabled(deployment, "seo", false).unwrap();
        let args = DlcArgs {
            intent: root.path().join(INSTALL_MANIFEST),
            lock: root.path().join(INSTALL_LOCK),
            env_file: Some(root.path().join(".env")),
            action: DlcAction::List {
                available: false,
                json: false,
            },
        };
        let deployment = load_deployment(&args).unwrap();
        assert_eq!(deployment.lock.history.len(), 2);
        assert_eq!(deployment.lock.history[1].sequence, 2);
        assert_eq!(
            deployment.lock.history[1].action,
            DlcHistoryAction::Disabled
        );
        assert_eq!(deployment.lock.dlcs[0].state_version, Some(7));
        assert_eq!(deployment.lock.dlcs[0].applied_migrations, ["seo.state.v7"]);
        assert!(
            as_utf8(&deployment.env_original, ".env")
                .unwrap()
                .contains("SECRET='do not reformat this = value'\n")
        );

        remove(deployment, "org.open-soverign-blog.seo").unwrap();
        let deployment = load_deployment(&args).unwrap();
        assert!(deployment.lock.dlcs.is_empty());
        assert!(deployment.intent.dlcs.is_empty());
        assert_eq!(deployment.lock.history.len(), 3);
        assert_eq!(deployment.lock.history[2].sequence, 3);
        assert_eq!(deployment.lock.history[2].action, DlcHistoryAction::Removed);
        assert_eq!(deployment.lock.retained_dlcs.len(), 1);
        assert_eq!(deployment.lock.retained_dlcs[0].state_version, Some(7));
        assert_eq!(
            deployment.lock.retained_dlcs[0].applied_migrations,
            ["seo.state.v7"]
        );
        assert_eq!(
            deployment.env_values.get("OSB_DLC_IDS").map(String::as_str),
            Some("")
        );
        assert_eq!(
            deployment
                .env_values
                .get("OSB_FEATURES")
                .map(String::as_str),
            Some("none")
        );

        run(DlcArgs {
            intent: root.path().join(INSTALL_MANIFEST),
            lock: root.path().join(INSTALL_LOCK),
            env_file: Some(root.path().join(".env")),
            action: DlcAction::Add {
                dlc: "seo".into(),
                version: None,
            },
        })
        .unwrap();
        let deployment = load_deployment(&args).unwrap();
        assert!(deployment.lock.retained_dlcs.is_empty());
        assert_eq!(deployment.lock.dlcs[0].state_version, Some(7));
        assert_eq!(deployment.lock.dlcs[0].applied_migrations, ["seo.state.v7"]);
        assert_eq!(deployment.lock.history.len(), 4);
        assert_eq!(deployment.lock.history[3].sequence, 4);
        assert_eq!(
            deployment.lock.history[3].action,
            DlcHistoryAction::Installed
        );
    }

    #[test]
    fn official_resolution_is_exact_and_rejects_unknown_or_unsatisfied_versions() {
        let official = official_target("seo").unwrap();
        let resolved = resolve_official(official, "=0.1.0", true, current_engine()).unwrap();
        assert_eq!(resolved.id, "org.open-soverign-blog.seo");
        assert_eq!(resolved.version, "0.1.0");
        assert_eq!(resolved.source_kind, InstalledDlcSourceKind::Bundled);
        assert_eq!(resolved.manifest_sha256.len(), 64);
        assert!(official_target("https://example.test/plugin.toml").is_err());
        assert!(resolve_official(official, ">=9.0.0", true, current_engine()).is_err());
    }

    #[test]
    fn add_alias_persists_sorted_exact_records_history_and_environment_digest() {
        let root = tempdir().unwrap();
        let deployment = fixture(root.path());
        let args = DlcArgs {
            intent: deployment.intent_path.clone(),
            lock: deployment.lock_path.clone(),
            env_file: Some(deployment.env_path.clone()),
            action: DlcAction::Add {
                dlc: "ai-authorship@^0.1.0".into(),
                version: None,
            },
        };
        run(args).unwrap();
        let deployment = load_deployment(&DlcArgs {
            intent: root.path().join(INSTALL_MANIFEST),
            lock: root.path().join(INSTALL_LOCK),
            env_file: Some(root.path().join(".env")),
            action: DlcAction::List {
                available: false,
                json: false,
            },
        })
        .unwrap();
        let ids = deployment
            .lock
            .dlcs
            .iter()
            .map(|dlc| dlc.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "org.open-soverign-blog.ai-authorship",
                "org.open-soverign-blog.seo"
            ]
        );
        assert_eq!(
            deployment
                .intent
                .dlcs
                .iter()
                .map(|dlc| dlc.id.as_str())
                .collect::<Vec<_>>(),
            ids
        );
        let installed = &deployment.lock.dlcs[0];
        assert_eq!(installed.version, "0.1.0");
        assert_eq!(installed.requested_version, "^0.1.0");
        assert_eq!(installed.manifest_sha256.len(), 64);
        assert_eq!(deployment.lock.history.len(), 2);
        assert_eq!(deployment.lock.history[1].sequence, 2);
        assert_eq!(
            deployment.lock.history[1].action,
            DlcHistoryAction::Installed
        );
        assert_eq!(
            deployment
                .env_values
                .get("OSB_INSTALL_LOCK_DIGEST")
                .map(String::as_str),
            Some(deployment.lock.lock_digest.as_str())
        );
    }

    #[test]
    fn candidate_reconcile_updates_the_engine_tuple_and_every_exact_dlc_record() {
        let root = tempdir().unwrap();
        let mut deployment = fixture(root.path());
        let request = ">=0.0.1, <0.2.0";
        deployment.intent.dlcs[0].version = request.into();
        deployment.lock.engine.version = "0.0.9".into();
        deployment.lock.engine.config_schema_version = "open-soverign-blog/1".into();
        deployment.lock.engine.database_schema_version = 1;
        deployment.lock.dlcs[0].requested_version = request.into();
        deployment.lock.dlcs[0].version = "0.0.5".into();
        deployment.lock.dlcs[0].core_compatibility = ">=0.0.1, <0.2.0".into();
        deployment.lock.dlcs[0].manifest_sha256 = "b".repeat(64);
        deployment.lock.dlcs[0].applied_migrations = vec!["seo.state.v1".into()];
        deployment.lock.history[0].to_version = Some("0.0.5".into());
        deployment.lock.history[0].engine_version = "0.0.9".into();
        deployment.lock.refresh_digest().unwrap();
        deployment.intent_original = deployment.intent.to_toml_pretty().unwrap().into_bytes();
        deployment.lock_original = deployment.lock.to_pretty_json().unwrap().into_bytes();
        deployment.env_original = replace_environment_values(
            &deployment.env_original,
            &BTreeMap::from([(
                "OSB_INSTALL_LOCK_DIGEST",
                deployment.lock.lock_digest.clone(),
            )]),
        )
        .unwrap();
        fs::write(&deployment.intent_path, &deployment.intent_original).unwrap();
        fs::write(&deployment.lock_path, &deployment.lock_original).unwrap();
        fs::write(&deployment.env_path, &deployment.env_original).unwrap();
        deployment.env_values = project_environment(&deployment.env_original).unwrap();

        reconcile(
            deployment,
            "0.0.9",
            current_engine(),
            "candidate-release".into(),
            Some("c".repeat(64)),
        )
        .unwrap();

        let deployment = load_deployment(&DlcArgs {
            intent: root.path().join(INSTALL_MANIFEST),
            lock: root.path().join(INSTALL_LOCK),
            env_file: Some(root.path().join(".env")),
            action: DlcAction::List {
                available: false,
                json: false,
            },
        })
        .unwrap();
        assert_eq!(deployment.lock.engine.version, current_engine());
        assert_eq!(deployment.lock.engine.config_schema_version, CONFIG_SCHEMA);
        assert_eq!(
            deployment.lock.engine.database_schema_version,
            DATABASE_SCHEMA_VERSION
        );
        assert_eq!(deployment.lock.engine.plugin_api, PLUGIN_API_VERSION);
        assert_eq!(deployment.lock.engine.source, "candidate-release");
        assert_eq!(deployment.lock.engine.artifact_sha256, Some("c".repeat(64)));
        assert_eq!(deployment.lock.dlcs[0].version, "0.1.0");
        assert_eq!(
            deployment.lock.dlcs[0].manifest_sha256,
            official_manifest_digest(find_official_dlc("seo").unwrap())
        );
        assert_eq!(deployment.lock.dlcs[0].applied_migrations, ["seo.state.v1"]);
        assert_eq!(deployment.lock.history.len(), 2);
        assert_eq!(deployment.lock.history[1].sequence, 2);
        assert_eq!(
            deployment.lock.history[1].action,
            DlcHistoryAction::Upgraded
        );
        assert_eq!(
            deployment.lock.history[1].from_version.as_deref(),
            Some("0.0.5")
        );
        assert_eq!(
            deployment.lock.history[1].to_version.as_deref(),
            Some("0.1.0")
        );
        assert_eq!(
            deployment
                .env_values
                .get("OSB_INSTALL_LOCK_DIGEST")
                .map(String::as_str),
            Some(deployment.lock.lock_digest.as_str())
        );
    }

    #[test]
    fn failed_three_file_transaction_restores_original_bytes() {
        let root = tempdir().unwrap();
        let deployment = fixture(root.path());
        let originals = [
            deployment.intent_original.clone(),
            deployment.lock_original.clone(),
            deployment.env_original.clone(),
        ];
        let paths = [
            deployment.intent_path.clone(),
            deployment.lock_path.clone(),
            deployment.env_path.clone(),
        ];
        let replacements = [
            Replacement {
                path: paths[0].clone(),
                label: "intent",
                original: originals[0].clone(),
                replacement: b"new intent\n".to_vec(),
            },
            Replacement {
                path: paths[1].clone(),
                label: "lock",
                original: originals[1].clone(),
                replacement: b"new lock\n".to_vec(),
            },
            Replacement {
                path: paths[2].clone(),
                label: "environment",
                original: originals[2].clone(),
                replacement: b"new env\n".to_vec(),
            },
        ];
        let error = transactional_replace(&replacements, |committed| {
            ensure!(committed != 1, "injected commit failure");
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("rolled back"));
        for (path, original) in paths.iter().zip(originals) {
            assert_eq!(fs::read(path).unwrap(), original);
        }
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".backup")
        }));
    }
}
