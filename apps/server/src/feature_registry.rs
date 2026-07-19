use std::collections::BTreeSet;

use serde::Serialize;

const KNOWN_FEATURES: [&str; 10] = [
    "ads",
    "ai_authorship",
    "code_runner",
    "comments",
    "external_auth",
    "home_curation",
    "rbac",
    "release_check",
    "seo",
    "social_embeds",
];

/// Maps stable installation/DLC identifiers to the runtime composition names
/// retained by the capabilities API. Unknown third-party DLCs remain locked by
/// the installation contract, but cannot become executable merely by naming
/// themselves in a manifest.
pub fn runtime_feature_for_dlc(id: &str) -> Option<&'static str> {
    match id {
        "org.open-soverign-blog.monetization-policy" => Some("ads"),
        "org.open-soverign-blog.ai-authorship" => Some("ai_authorship"),
        "org.open-soverign-blog.code-runner-client" => Some("code_runner"),
        "org.open-soverign-blog.comments" => Some("comments"),
        "org.open-soverign-blog.external-auth" => Some("external_auth"),
        "org.open-soverign-blog.home-curation" => Some("home_curation"),
        "org.open-soverign-blog.rbac" => Some("rbac"),
        "org.open-soverign-blog.release-check" => Some("release_check"),
        "org.open-soverign-blog.seo" => Some("seo"),
        "org.open-soverign-blog.social-embeds" => Some("social_embeds"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleStatus {
    Active,
    Available,
    Degraded,
    Disabled,
    Misconfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleDescriptor {
    pub id: &'static str,
    pub status: ModuleStatus,
    pub requested: bool,
    pub operational: bool,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct FeatureRegistry {
    modules: Vec<ModuleDescriptor>,
}

impl FeatureRegistry {
    pub fn from_requested(raw: &str) -> Result<Self, String> {
        let requested: BTreeSet<_> = raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect();
        let unknown: Vec<_> = requested
            .iter()
            .filter(|value| !KNOWN_FEATURES.contains(value))
            .copied()
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "unknown OSB_FEATURES value(s): {}; known values: {}",
                unknown.join(", "),
                KNOWN_FEATURES.join(", ")
            ));
        }

        let mut modules = vec![
            module(
                "seo",
                requested.contains("seo"),
                true,
                "canonical URLs, redirect history, robots.txt, and sitemap are mounted",
            ),
            module(
                "home_curation",
                requested.contains("home_curation"),
                true,
                "the pinned-first home feed and administrator curation routes are mounted",
            ),
            module(
                "ai_authorship",
                requested.contains("ai_authorship"),
                true,
                "portable public revision authorship provenance is enabled",
            ),
            module(
                "social_embeds",
                requested.contains("social_embeds"),
                true,
                "strict YouTube and X provider parsing is enabled without arbitrary embed HTML",
            ),
            module(
                "release_check",
                requested.contains("release_check"),
                true,
                "bounded informational release-channel checks are enabled",
            ),
        ];
        for (id, reason) in [
            (
                "external_auth",
                "domain library and manifest are available; a verified provider/session adapter is not composed into this server",
            ),
            (
                "rbac",
                "domain library and manifest are available; no identity/session adapter is composed into this server",
            ),
            (
                "comments",
                "domain library and manifest are available; persistence, moderation routes, and abuse controls are not composed",
            ),
            (
                "code_runner",
                "contract and security profile are available; no isolated runner broker is configured",
            ),
            (
                "ads",
                "policy contracts are available; no consent policy and provider adapter are configured",
            ),
        ] {
            let is_requested = requested.contains(id);
            modules.push(ModuleDescriptor {
                id,
                status: if is_requested {
                    ModuleStatus::Misconfigured
                } else {
                    ModuleStatus::Available
                },
                requested: is_requested,
                operational: false,
                reason: reason.into(),
            });
        }
        modules.push(ModuleDescriptor {
            id: "ontology",
            status: ModuleStatus::Available,
            requested: false,
            operational: true,
            reason: "optional per-revision sidecar; Markdown remains required and no global activation is needed".into(),
        });
        Ok(Self { modules })
    }

    pub fn active_ids(&self) -> Vec<String> {
        self.modules
            .iter()
            .filter(|module| module.status == ModuleStatus::Active)
            .map(|module| module.id.to_owned())
            .collect()
    }

    pub fn modules(&self) -> &[ModuleDescriptor] {
        &self.modules
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.modules
            .iter()
            .any(|module| module.id == id && module.status == ModuleStatus::Active)
    }

    pub fn is_requested(&self, id: &str) -> bool {
        self.modules
            .iter()
            .any(|module| module.id == id && module.requested)
    }

    pub fn set_runtime_status(
        &mut self,
        id: &str,
        status: ModuleStatus,
        operational: bool,
        reason: impl Into<String>,
    ) -> Result<(), String> {
        let module = self
            .modules
            .iter_mut()
            .find(|module| module.id == id)
            .ok_or_else(|| format!("unknown runtime module: {id}"))?;
        if status == ModuleStatus::Active && !module.requested {
            return Err(format!("module {id} cannot become active unless requested"));
        }
        module.status = status;
        module.operational = operational;
        module.reason = reason.into();
        Ok(())
    }

    /// Marks a module that is built directly into this server composition.
    /// This is distinct from an optional adapter requested through
    /// `OSB_FEATURES`: community authorization and comments have no external
    /// readiness dependency once their SQLite migrations are available.
    pub fn activate_composed(&mut self, id: &str, reason: impl Into<String>) -> Result<(), String> {
        let module = self
            .modules
            .iter_mut()
            .find(|module| module.id == id)
            .ok_or_else(|| format!("unknown composed module: {id}"))?;
        module.status = ModuleStatus::Active;
        module.operational = true;
        module.reason = reason.into();
        Ok(())
    }
}

fn module(
    id: &'static str,
    requested: bool,
    operational: bool,
    reason: &'static str,
) -> ModuleDescriptor {
    ModuleDescriptor {
        id,
        status: if requested {
            ModuleStatus::Active
        } else {
            ModuleStatus::Disabled
        },
        requested,
        operational,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_requested_but_uncomposed_module_is_never_advertised_as_active() {
        let registry = FeatureRegistry::from_requested("seo,ads,code_runner").unwrap();
        assert_eq!(registry.active_ids(), vec!["seo"]);
        assert_eq!(
            registry
                .modules()
                .iter()
                .find(|module| module.id == "ads")
                .unwrap()
                .status,
            ModuleStatus::Misconfigured
        );
    }

    #[test]
    fn unknown_feature_names_fail_configuration() {
        assert!(FeatureRegistry::from_requested("seo,typo").is_err());
    }

    #[test]
    fn built_in_community_modules_are_reported_as_operational() {
        let mut registry = FeatureRegistry::from_requested("seo").unwrap();
        registry
            .activate_composed("comments", "persistent comment routes are mounted")
            .unwrap();
        assert!(registry.is_active("comments"));
        let comments = registry
            .modules()
            .iter()
            .find(|module| module.id == "comments")
            .unwrap();
        assert!(comments.operational);
        assert!(!comments.requested);
    }
}
