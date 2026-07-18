use osb_kernel::{
    AccessDecision, AccessPolicy, ContentAction, Principal, ResourceScope, SingleOwnerPolicy,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleGrant {
    pub role: String,
    pub action: ContentAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<Uuid>,
}

pub struct RbacPolicy {
    fallback: SingleOwnerPolicy,
    grants: Vec<RoleGrant>,
}

impl RbacPolicy {
    pub fn new(owner_id: impl Into<String>, grants: Vec<RoleGrant>) -> Self {
        Self {
            fallback: SingleOwnerPolicy {
                owner_id: owner_id.into(),
            },
            grants,
        }
    }
}

impl AccessPolicy for RbacPolicy {
    fn authorize(
        &self,
        principal: Option<&Principal>,
        action: ContentAction,
        resource: &ResourceScope,
    ) -> AccessDecision {
        let fallback = self.fallback.authorize(principal, action, resource);
        if fallback.allowed {
            return fallback;
        }
        let Some(principal) = principal else {
            return fallback;
        };
        let granted = self.grants.iter().any(|grant| {
            grant.action == action
                && principal.roles.iter().any(|role| role == &grant.role)
                && grant
                    .site_id
                    .is_none_or(|site_id| site_id == resource.site_id)
                && grant
                    .document_id
                    .is_none_or(|document_id| Some(document_id) == resource.document_id)
        });
        AccessDecision {
            allowed: granted,
            reason: if granted {
                "role grant matched the exact resource scope"
            } else {
                "no role grant matched; deny by default"
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_grant_cannot_cross_site_boundaries() {
        let allowed_site = Uuid::now_v7();
        let other_site = Uuid::now_v7();
        let policy = RbacPolicy::new(
            "owner",
            vec![RoleGrant {
                role: "editor".into(),
                action: ContentAction::Revise,
                site_id: Some(allowed_site),
                document_id: None,
            }],
        );
        let editor = Principal {
            id: "editor-1".into(),
            roles: vec!["editor".into()],
            capabilities: vec![],
        };
        assert!(
            policy
                .authorize(
                    Some(&editor),
                    ContentAction::Revise,
                    &ResourceScope {
                        site_id: allowed_site,
                        document_id: None,
                    }
                )
                .allowed
        );
        assert!(
            !policy
                .authorize(
                    Some(&editor),
                    ContentAction::Revise,
                    &ResourceScope {
                        site_id: other_site,
                        document_id: None,
                    }
                )
                .allowed
        );
    }
}
