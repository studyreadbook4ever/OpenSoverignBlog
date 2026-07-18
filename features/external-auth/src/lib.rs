use std::collections::BTreeMap;

use osb_kernel::Principal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedExternalIdentity {
    pub issuer: String,
    pub subject: String,
    #[serde(default)]
    pub claims: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityMappingPolicy {
    pub allowed_issuers: Vec<String>,
    #[serde(default)]
    pub role_claim: Option<String>,
    #[serde(default)]
    pub role_allowlist: Vec<String>,
}

impl IdentityMappingPolicy {
    /// Maps claims only after a concrete adapter has cryptographically verified
    /// issuer, signature, audience, expiry, nonce, and protocol replay rules.
    pub fn map_verified(
        &self,
        identity: &VerifiedExternalIdentity,
    ) -> Result<Principal, ExternalAuthError> {
        if !self
            .allowed_issuers
            .iter()
            .any(|value| value == &identity.issuer)
        {
            return Err(ExternalAuthError::UntrustedIssuer);
        }
        if identity.subject.trim().is_empty() || identity.subject.len() > 500 {
            return Err(ExternalAuthError::InvalidSubject);
        }
        let roles = self
            .role_claim
            .as_ref()
            .and_then(|claim| identity.claims.get(claim))
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|role| self.role_allowlist.iter().any(|allowed| allowed == role))
            .map(str::to_owned)
            .collect();
        Ok(Principal {
            id: format!("{}#{}", identity.issuer, identity.subject),
            roles,
            capabilities: vec![],
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExternalAuthError {
    #[error("external identity issuer is not allowed")]
    UntrustedIssuer,
    #[error("external identity subject is invalid")]
    InvalidSubject,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_roles_and_issuers_do_not_enter_the_principal() {
        let policy = IdentityMappingPolicy {
            allowed_issuers: vec!["https://identity.example".into()],
            role_claim: Some("roles".into()),
            role_allowlist: vec!["editor".into()],
        };
        let identity = VerifiedExternalIdentity {
            issuer: "https://identity.example".into(),
            subject: "user-1".into(),
            claims: BTreeMap::from([("roles".into(), "editor,root".into())]),
        };
        assert_eq!(
            policy.map_verified(&identity).unwrap().roles,
            vec!["editor"]
        );
    }
}
