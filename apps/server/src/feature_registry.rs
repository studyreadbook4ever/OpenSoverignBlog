use std::collections::BTreeSet;

use serde::Serialize;

const KNOWN_FEATURES: [&str; 6] = [
    "ads",
    "code_runner",
    "comments",
    "external_auth",
    "rbac",
    "seo",
];

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

        let mut modules = vec![module(
            "seo",
            requested.contains("seo"),
            true,
            "canonical URLs, redirect history, robots.txt, and sitemap are mounted",
        )];
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
