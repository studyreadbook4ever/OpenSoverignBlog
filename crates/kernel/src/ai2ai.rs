use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ProposedRevision;

pub const AI2AI_SPEC_VERSION: &str = "1.0";
pub const AI_PROPOSAL_AUDIT_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiActorKind {
    Human,
    Agent,
    Tool,
    Service,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiActor {
    pub kind: AiActorKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataBoundary {
    LocalOnly,
    ApprovedProviders,
    RedactBeforeSend,
    ExternalAllowed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiPolicySnapshot {
    pub data_boundary: DataBoundary,
    #[serde(default)]
    pub allowed_provider_ids: Vec<String>,
    #[serde(default)]
    pub allowed_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ai2AiEnvelope {
    pub spec_version: String,
    pub message_id: Uuid,
    pub idempotency_key: String,
    pub occurred_at: DateTime<Utc>,
    pub actor: AiActor,
    pub intent: String,
    pub proposal: ProposedRevision,
    pub policy: AiPolicySnapshot,
    #[serde(default)]
    pub context_receipts: Vec<ContextReceipt>,
    #[serde(default)]
    pub provenance: Vec<AiProvenanceEntry>,
}

/// Immutable receipt for an AI2AI proposal accepted as a content revision.
///
/// The complete envelope is retained so policy, context inclusion/exclusion,
/// and provenance decisions remain reviewable alongside the accepted
/// revision. `received_at` records local acceptance time; it intentionally
/// differs from the caller-controlled `envelope.occurred_at` timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiProposalAuditRecord {
    pub schema_version: String,
    pub document_id: Uuid,
    pub accepted_revision_id: Uuid,
    pub received_at: DateTime<Utc>,
    pub envelope: Ai2AiEnvelope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextReceipt {
    pub reference: String,
    pub content_hash: String,
    pub scope: String,
    pub included: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiProvenanceEntry {
    pub kind: String,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

impl Ai2AiEnvelope {
    pub fn validate(&self) -> Result<(), Ai2AiValidationError> {
        if self.spec_version != AI2AI_SPEC_VERSION {
            return Err(Ai2AiValidationError::UnsupportedVersion);
        }
        if self.message_id.is_nil()
            || self.proposal.document_id.is_nil()
            || self.proposal.base_revision_id.is_nil()
        {
            return Err(Ai2AiValidationError::InvalidIdentifier);
        }
        if self.idempotency_key.trim().is_empty()
            || self.idempotency_key.len() > 200
            || self.idempotency_key.contains('\0')
        {
            return Err(Ai2AiValidationError::InvalidIdempotencyKey);
        }
        if self.actor.id.trim().is_empty()
            || self.actor.id.len() > 300
            || self.actor.id.contains('\0')
            || self.intent.trim().is_empty()
            || self.intent.len() > 2000
            || self.intent.contains('\0')
            || self
                .actor
                .provider
                .as_ref()
                .is_some_and(|value| value.len() > 300 || value.contains('\0'))
            || self
                .actor
                .model
                .as_ref()
                .is_some_and(|value| value.len() > 300 || value.contains('\0'))
        {
            return Err(Ai2AiValidationError::MissingIdentityOrIntent);
        }
        if self.occurred_at > Utc::now() + Duration::minutes(5) {
            return Err(Ai2AiValidationError::InvalidTimestamp);
        }
        self.policy
            .validate_for_provider(self.actor.provider.as_deref())?;
        validate_context_receipts(&self.context_receipts)?;
        validate_provenance(&self.provenance)?;
        self.proposal
            .validate()
            .map_err(|_| Ai2AiValidationError::InvalidProposal)
    }
}

impl AiPolicySnapshot {
    pub fn validate_for_provider(
        &self,
        actor_provider: Option<&str>,
    ) -> Result<(), Ai2AiValidationError> {
        validate_policy(self, actor_provider)
    }
}

fn validate_policy(
    policy: &AiPolicySnapshot,
    actor_provider: Option<&str>,
) -> Result<(), Ai2AiValidationError> {
    if policy.allowed_provider_ids.len() > 128
        || policy.allowed_capabilities.len() > 128
        || !unique_bounded_values(&policy.allowed_provider_ids, 300)
        || !unique_bounded_values(&policy.allowed_capabilities, 300)
        || policy
            .max_cost
            .is_some_and(|value| !value.is_finite() || value < 0.0)
    {
        return Err(Ai2AiValidationError::InvalidPolicy);
    }
    match policy.data_boundary {
        DataBoundary::LocalOnly if !policy.allowed_provider_ids.is_empty() => {
            Err(Ai2AiValidationError::InvalidPolicy)
        }
        DataBoundary::ApprovedProviders => {
            let provider = actor_provider.ok_or(Ai2AiValidationError::InvalidPolicy)?;
            if policy
                .allowed_provider_ids
                .iter()
                .any(|value| value == provider)
            {
                Ok(())
            } else {
                Err(Ai2AiValidationError::InvalidPolicy)
            }
        }
        _ => Ok(()),
    }
}

fn validate_context_receipts(values: &[ContextReceipt]) -> Result<(), Ai2AiValidationError> {
    if values.len() > 10_000 {
        return Err(Ai2AiValidationError::InvalidContextReceipt);
    }
    let mut references = BTreeSet::new();
    for value in values {
        if value.reference.trim().is_empty()
            || value.reference.len() > 2048
            || value.scope.trim().is_empty()
            || value.scope.len() > 100
            || !valid_content_hash(&value.content_hash)
            || !references.insert((&value.reference, &value.scope))
            || value.exclusion_reason.as_ref().is_some_and(|reason| {
                reason.trim().is_empty() || reason.len() > 1000 || reason.contains('\0')
            })
            || (!value.included && value.exclusion_reason.is_none())
            || (value.included && value.exclusion_reason.is_some())
        {
            return Err(Ai2AiValidationError::InvalidContextReceipt);
        }
    }
    Ok(())
}

fn validate_provenance(values: &[AiProvenanceEntry]) -> Result<(), Ai2AiValidationError> {
    if values.len() > 10_000 {
        return Err(Ai2AiValidationError::InvalidProvenance);
    }
    for value in values {
        if value.kind.trim().is_empty()
            || value.kind.len() > 100
            || value.reference.trim().is_empty()
            || value.reference.len() > 2048
            || value
                .content_hash
                .as_ref()
                .is_some_and(|digest| !valid_content_hash(digest))
        {
            return Err(Ai2AiValidationError::InvalidProvenance);
        }
    }
    Ok(())
}

fn unique_bounded_values(values: &[String], maximum: usize) -> bool {
    let mut seen = BTreeSet::new();
    values.iter().all(|value| {
        !value.trim().is_empty()
            && value.len() <= maximum
            && !value.contains('\0')
            && seen.insert(value)
    })
}

fn valid_content_hash(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Ai2AiValidationError {
    #[error("unsupported AI2AI version")]
    UnsupportedVersion,
    #[error("invalid idempotency key")]
    InvalidIdempotencyKey,
    #[error("actor identity and intent are required")]
    MissingIdentityOrIntent,
    #[error("proposed revision is invalid")]
    InvalidProposal,
    #[error("message, document, and base revision identifiers must be non-nil UUIDs")]
    InvalidIdentifier,
    #[error("AI2AI timestamp is unreasonably far in the future")]
    InvalidTimestamp,
    #[error("AI policy snapshot is inconsistent or out of bounds")]
    InvalidPolicy,
    #[error("AI context receipt is invalid, duplicated, or inconsistent")]
    InvalidContextReceipt,
    #[error("AI provenance entry is invalid")]
    InvalidProvenance,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RevisionActor, RevisionActorKind};

    fn envelope() -> Ai2AiEnvelope {
        Ai2AiEnvelope {
            spec_version: AI2AI_SPEC_VERSION.into(),
            message_id: Uuid::now_v7(),
            idempotency_key: "proposal-1".into(),
            occurred_at: Utc::now(),
            actor: AiActor {
                kind: AiActorKind::Agent,
                id: "writer-agent".into(),
                provider: Some("local-model".into()),
                model: Some("model-v1".into()),
            },
            intent: "Propose a clearer introduction".into(),
            proposal: ProposedRevision {
                document_id: Uuid::now_v7(),
                base_revision_id: Uuid::now_v7(),
                title: "Title".into(),
                slug: "title".into(),
                source_markdown: "# Title".into(),
                embeds: vec![],
                intent: None,
                ontology: None,
                ai_summary: None,
                authorship: Default::default(),
                actor: RevisionActor {
                    kind: RevisionActorKind::Agent,
                    id: "writer-agent".into(),
                    display_name: None,
                },
                idempotency_key: None,
            },
            policy: AiPolicySnapshot {
                data_boundary: DataBoundary::ApprovedProviders,
                allowed_provider_ids: vec!["local-model".into()],
                allowed_capabilities: vec!["content.propose".into()],
                max_cost: Some(0.1),
                max_tokens: Some(2_000),
            },
            context_receipts: vec![],
            provenance: vec![],
        }
    }

    #[test]
    fn approved_provider_policy_must_cover_the_claimed_actor() {
        let mut value = envelope();
        value.validate().unwrap();
        value.policy.allowed_provider_ids = vec!["another-provider".into()];
        assert_eq!(value.validate(), Err(Ai2AiValidationError::InvalidPolicy));
    }

    #[test]
    fn exclusion_receipts_require_an_explicit_reason() {
        let mut value = envelope();
        value.context_receipts.push(ContextReceipt {
            reference: "draft:private-note".into(),
            content_hash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            scope: "private".into(),
            included: false,
            exclusion_reason: None,
        });
        assert_eq!(
            value.validate(),
            Err(Ai2AiValidationError::InvalidContextReceipt)
        );
    }

    #[test]
    fn audit_record_uses_explicit_camel_case_wire_fields() {
        let envelope = envelope();
        let record = AiProposalAuditRecord {
            schema_version: AI_PROPOSAL_AUDIT_SCHEMA_VERSION.into(),
            document_id: envelope.proposal.document_id,
            accepted_revision_id: Uuid::now_v7(),
            received_at: Utc::now(),
            envelope,
        };
        let json = serde_json::to_value(record).unwrap();
        assert_eq!(json["schemaVersion"], AI_PROPOSAL_AUDIT_SCHEMA_VERSION);
        assert!(json.get("documentId").is_some());
        assert!(json.get("acceptedRevisionId").is_some());
        assert!(json.get("receivedAt").is_some());
        assert!(json.get("envelope").is_some());
        assert!(json.get("accepted_revision_id").is_none());
    }
}
