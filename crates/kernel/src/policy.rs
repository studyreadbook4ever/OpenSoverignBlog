use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Principal {
    pub id: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentAction {
    ReadPublished,
    ReadDraft,
    Create,
    Revise,
    Publish,
    Archive,
    Purge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceScope {
    pub site_id: Uuid,
    pub document_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessDecision {
    pub allowed: bool,
    pub reason: &'static str,
}

pub trait AccessPolicy: Send + Sync {
    fn authorize(
        &self,
        principal: Option<&Principal>,
        action: ContentAction,
        resource: &ResourceScope,
    ) -> AccessDecision;
}

/// Safe fallback when the RBAC feature is absent.
///
/// Published reads are public. Every mutation requires the composition root to
/// establish the configured owner principal.
pub struct SingleOwnerPolicy {
    pub owner_id: String,
}

impl AccessPolicy for SingleOwnerPolicy {
    fn authorize(
        &self,
        principal: Option<&Principal>,
        action: ContentAction,
        _resource: &ResourceScope,
    ) -> AccessDecision {
        if action == ContentAction::ReadPublished {
            return AccessDecision {
                allowed: true,
                reason: "published content is public",
            };
        }
        let allowed = principal.is_some_and(|value| value.id == self.owner_id);
        AccessDecision {
            allowed,
            reason: if allowed {
                "single owner policy"
            } else {
                "mutation denied by default"
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_policy_denies_anonymous_mutation() {
        let policy = SingleOwnerPolicy {
            owner_id: "owner".into(),
        };
        let scope = ResourceScope {
            site_id: Uuid::now_v7(),
            document_id: None,
        };
        assert!(
            !policy
                .authorize(None, ContentAction::Create, &scope)
                .allowed
        );
        assert!(
            policy
                .authorize(None, ContentAction::ReadPublished, &scope)
                .allowed
        );
    }
}
