use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

pub const CONTENT_SCHEMA_VERSION: &str = "1.0";
pub const AI_SUMMARY_SOURCE_HASH_VERSION: &str = "osb-ai-summary-source/1";
pub const AI_SUMMARY_MAX_CHARACTERS: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentStatus {
    Draft,
    Published,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionActorKind {
    Human,
    Agent,
    Importer,
    System,
}

/// Portable, public-facing authorship provenance for one immutable revision.
///
/// This deliberately does not contain an internal user, agent, session, or
/// service identifier. Optional AI/import plugins may create this metadata,
/// but readers and exports can keep displaying it after the plugin is removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicAuthorship {
    pub kind: PublicAuthorshipKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
    #[serde(default)]
    pub human_reviewed: bool,
}

impl Default for PublicAuthorship {
    fn default() -> Self {
        Self {
            kind: PublicAuthorshipKind::Human,
            generator: None,
            human_reviewed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicAuthorshipKind {
    Human,
    AiGenerated,
    AiAssisted,
    Imported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionActor {
    pub kind: RevisionActorKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Optional author-intent representation.
///
/// `source_html` is always untrusted, including when it came from an LLM or an
/// administrator. Only the renderer may turn it into a publish artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentLayer {
    pub format: String,
    pub source_html: String,
    #[serde(default)]
    pub renderer_hints: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OntologySidecar {
    pub schema: String,
    #[serde(default)]
    pub statements: Vec<OntologyStatement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OntologyStatement {
    pub subject: String,
    pub predicate: String,
    pub object: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default)]
    pub confirmed_by_author: bool,
}

/// A typed, non-executing external embed reference.
///
/// The core stores identity and disclosure data only. A renderer emits a
/// first-party facade; a provider adapter may hydrate it only after consent and
/// capability checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedReference {
    pub id: String,
    pub provider: String,
    pub resource_id: String,
    pub canonical_url: Url,
    pub title: String,
    #[serde(default)]
    pub consent_purpose_ids: Vec<String>,
}

/// A reviewed, portable AI-generated summary for one immutable revision.
///
/// Provider credentials are intentionally not part of this model. Callers may
/// use a one-shot credential to generate the text, but only this public
/// provenance is allowed to cross the content storage boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AiSummary {
    pub text: String,
    pub source_hash: String,
    pub provenance: AiSummaryProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AiSummaryProvenance {
    pub provider: String,
    pub model: String,
    pub prompt_version: String,
    pub generated_at: DateTime<Utc>,
    pub human_reviewed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionSnapshot {
    pub schema_version: String,
    pub id: Uuid,
    pub document_id: Uuid,
    pub revision_number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_revision_id: Option<Uuid>,
    pub title: String,
    pub slug: String,
    pub source_markdown: String,
    #[serde(default)]
    pub embeds: Vec<EmbedReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<IntentLayer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ontology: Option<OntologySidecar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_summary: Option<AiSummary>,
    #[serde(default)]
    pub authorship: PublicAuthorship,
    pub actor: RevisionActor,
    pub content_hash: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSnapshot {
    pub schema_version: String,
    pub id: Uuid,
    pub site_id: Uuid,
    pub status: DocumentStatus,
    pub current_revision_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_revision_id: Option<Uuid>,
    pub revision: RevisionSnapshot,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewDocument {
    pub site_id: Uuid,
    pub title: String,
    pub slug: String,
    pub source_markdown: String,
    #[serde(default)]
    pub embeds: Vec<EmbedReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<IntentLayer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ontology: Option<OntologySidecar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_summary: Option<AiSummary>,
    #[serde(default)]
    pub authorship: PublicAuthorship,
    pub actor: RevisionActor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedRevision {
    pub document_id: Uuid,
    pub base_revision_id: Uuid,
    pub title: String,
    pub slug: String,
    pub source_markdown: String,
    #[serde(default)]
    pub embeds: Vec<EmbedReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<IntentLayer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ontology: Option<OntologySidecar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_summary: Option<AiSummary>,
    #[serde(default)]
    pub authorship: PublicAuthorship,
    pub actor: RevisionActor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl NewDocument {
    pub fn validate(&self) -> Result<(), ContentValidationError> {
        validate_title(&self.title)?;
        validate_slug(&self.slug)?;
        validate_markdown(&self.source_markdown)?;
        validate_embeds(&self.embeds)?;
        validate_authorship(&self.authorship)?;
        validate_optional_layers(self.intent.as_ref(), self.ontology.as_ref())?;
        if let Some(summary) = &self.ai_summary {
            validate_ready_ai_summary(&self.title, &self.source_markdown, summary)?;
        }
        Ok(())
    }
}

impl ProposedRevision {
    pub fn validate(&self) -> Result<(), ContentValidationError> {
        validate_title(&self.title)?;
        validate_slug(&self.slug)?;
        validate_markdown(&self.source_markdown)?;
        validate_embeds(&self.embeds)?;
        validate_authorship(&self.authorship)?;
        validate_optional_layers(self.intent.as_ref(), self.ontology.as_ref())?;
        if let Some(summary) = &self.ai_summary {
            validate_ready_ai_summary(&self.title, &self.source_markdown, summary)?;
        }
        if let Some(key) = &self.idempotency_key
            && (key.trim().is_empty() || key.len() > 200)
        {
            return Err(ContentValidationError::InvalidIdempotencyKey);
        }
        Ok(())
    }
}

pub fn content_hash(
    title: &str,
    slug: &str,
    markdown: &str,
    embeds: &[EmbedReference],
    intent: Option<&IntentLayer>,
    ontology: Option<&OntologySidecar>,
) -> String {
    content_hash_with_ai_summary(title, slug, markdown, embeds, intent, ontology, None)
}

/// Computes the revision hash while preserving the exact legacy hash whenever
/// the optional AI summary is absent.
pub fn content_hash_with_ai_summary(
    title: &str,
    slug: &str,
    markdown: &str,
    embeds: &[EmbedReference],
    intent: Option<&IntentLayer>,
    ontology: Option<&OntologySidecar>,
    ai_summary: Option<&AiSummary>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CONTENT_SCHEMA_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(title.as_bytes());
    hasher.update([0]);
    hasher.update(slug.as_bytes());
    hasher.update([0]);
    hasher.update(markdown.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(embeds).expect("embed serialization is infallible"));
    hasher.update([0]);
    if let Some(value) = intent {
        hasher.update(serde_json::to_vec(value).expect("intent serialization is infallible"));
    }
    hasher.update([0]);
    if let Some(value) = ontology {
        hasher.update(serde_json::to_vec(value).expect("ontology serialization is infallible"));
    }
    if let Some(value) = ai_summary {
        hasher.update([0]);
        hasher.update(b"ai-summary-v1");
        hasher.update([0]);
        hasher.update(serde_json::to_vec(value).expect("AI summary serialization is infallible"));
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// Hashes the exact author-controlled source used to generate an AI summary.
///
/// The domain separator and NUL delimiters make this stable and unambiguous
/// without coupling it to a document slug or any provider-specific metadata.
pub fn ai_summary_source_hash(title: &str, markdown: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(AI_SUMMARY_SOURCE_HASH_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(title.as_bytes());
    hasher.update([0]);
    hasher.update(markdown.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

impl AiSummary {
    pub fn validate_for_source(
        &self,
        title: &str,
        markdown: &str,
    ) -> Result<(), ContentValidationError> {
        validate_ready_ai_summary(title, markdown, self)
    }
}

impl RevisionSnapshot {
    /// Returns summary metadata only when it is safe to publish for this exact
    /// immutable source. Public JSON and rendered HTML share this fail-closed
    /// boundary so clients cannot observe a stale or unreviewed summary.
    pub fn publishable_ai_summary(&self) -> Option<&AiSummary> {
        self.ai_summary.as_ref().filter(|summary| {
            summary
                .validate_for_source(&self.title, &self.source_markdown)
                .is_ok()
        })
    }
}

/// Validates that a persisted summary is ready to be published with this
/// exact title and Markdown source.
pub fn validate_ready_ai_summary(
    title: &str,
    markdown: &str,
    summary: &AiSummary,
) -> Result<(), ContentValidationError> {
    let text_length = summary.text.trim().chars().count();
    if !(1..=AI_SUMMARY_MAX_CHARACTERS).contains(&text_length)
        || contains_disallowed_control(&summary.text)
    {
        return Err(ContentValidationError::InvalidAiSummaryText);
    }
    let provenance = &summary.provenance;
    if !valid_summary_label(&provenance.provider, 100)
        || !valid_summary_label(&provenance.model, 300)
        || !valid_summary_label(&provenance.prompt_version, 100)
    {
        return Err(ContentValidationError::InvalidAiSummaryProvenance);
    }
    if !provenance.human_reviewed {
        return Err(ContentValidationError::AiSummaryRequiresHumanReview);
    }
    if summary.source_hash != ai_summary_source_hash(title, markdown) {
        return Err(ContentValidationError::AiSummarySourceMismatch);
    }
    Ok(())
}

fn valid_summary_label(value: &str, max_length: usize) -> bool {
    let length = value.trim().chars().count();
    (1..=max_length).contains(&length) && !value.chars().any(char::is_control)
}

fn contains_disallowed_control(value: &str) -> bool {
    value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

fn validate_title(value: &str) -> Result<(), ContentValidationError> {
    let length = value.trim().chars().count();
    if !(1..=300).contains(&length) || value.contains('\0') {
        return Err(ContentValidationError::InvalidTitle);
    }
    Ok(())
}

fn validate_slug(value: &str) -> Result<(), ContentValidationError> {
    let slug = value.trim();
    if slug.is_empty()
        || slug.len() > 240
        || slug.starts_with('.')
        || slug.ends_with('.')
        || slug.contains('/')
        || slug.contains('\\')
        || slug.contains('\0')
        || slug.chars().any(char::is_control)
    {
        return Err(ContentValidationError::InvalidSlug);
    }
    Ok(())
}

fn validate_markdown(value: &str) -> Result<(), ContentValidationError> {
    if value.len() > 10 * 1024 * 1024 || value.contains('\0') {
        return Err(ContentValidationError::InvalidMarkdown);
    }
    Ok(())
}

fn validate_optional_layers(
    intent: Option<&IntentLayer>,
    ontology: Option<&OntologySidecar>,
) -> Result<(), ContentValidationError> {
    if let Some(intent) = intent {
        let invalid_hints = intent.renderer_hints.len() > 64
            || intent.renderer_hints.iter().any(|(key, value)| {
                key.trim().is_empty()
                    || key.len() > 100
                    || value.len() > 1000
                    || key.contains('\0')
                    || value.contains('\0')
            });
        let invalid_provenance = intent.provenance.as_ref().is_some_and(|provenance| {
            provenance.origin.trim().is_empty()
                || provenance.origin.len() > 100
                || provenance
                    .source_uri
                    .as_ref()
                    .is_some_and(|value| value.len() > 2048 || Url::parse(value).is_err())
                || provenance
                    .actor_id
                    .as_ref()
                    .is_some_and(|value| value.len() > 200 || value.contains('\0'))
                || provenance
                    .generated_by
                    .as_ref()
                    .is_some_and(|value| value.len() > 500 || value.contains('\0'))
        });
        if intent.format.trim().is_empty()
            || intent.format.len() > 100
            || intent.source_html.len() > 10 * 1024 * 1024
            || intent.source_html.contains('\0')
            || invalid_hints
            || invalid_provenance
        {
            return Err(ContentValidationError::InvalidIntentLayer);
        }
    }
    if let Some(ontology) = ontology {
        let invalid_statement = ontology.statements.iter().any(|statement| {
            statement.subject.trim().is_empty()
                || statement.subject.len() > 2048
                || statement.subject.contains('\0')
                || statement.predicate.trim().is_empty()
                || statement.predicate.len() > 2048
                || statement.predicate.contains('\0')
                || statement
                    .evidence
                    .as_ref()
                    .is_some_and(|value| value.len() > 4096 || value.contains('\0'))
        });
        if ontology.schema.trim().is_empty()
            || ontology.schema.len() > 2048
            || Url::parse(&ontology.schema).is_err()
            || ontology.statements.len() > 100_000
            || invalid_statement
            || serde_json::to_vec(ontology).is_ok_and(|value| value.len() > 10 * 1024 * 1024)
        {
            return Err(ContentValidationError::InvalidOntologyLayer);
        }
    }
    Ok(())
}

fn validate_authorship(value: &PublicAuthorship) -> Result<(), ContentValidationError> {
    if value.generator.as_ref().is_some_and(|generator| {
        generator.trim().is_empty()
            || generator.len() > 300
            || generator.chars().any(char::is_control)
    }) {
        return Err(ContentValidationError::InvalidAuthorship);
    }
    if value.kind == PublicAuthorshipKind::Human && value.generator.is_some() {
        return Err(ContentValidationError::InvalidAuthorship);
    }
    if value.kind != PublicAuthorshipKind::Human && value.generator.is_none() {
        return Err(ContentValidationError::InvalidAuthorship);
    }
    Ok(())
}

fn validate_embeds(values: &[EmbedReference]) -> Result<(), ContentValidationError> {
    if values.len() > 10_000 {
        return Err(ContentValidationError::InvalidEmbed);
    }
    let mut ids = std::collections::BTreeSet::new();
    for value in values {
        let safe_identifier = |candidate: &str| {
            !candidate.is_empty()
                && candidate.len() <= 200
                && candidate.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
        };
        let invalid_purposes = value.consent_purpose_ids.len() > 128
            || value
                .consent_purpose_ids
                .iter()
                .any(|purpose| !safe_identifier(purpose))
            || value
                .consent_purpose_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != value.consent_purpose_ids.len();
        if !safe_identifier(&value.id)
            || !safe_identifier(&value.provider)
            || value.resource_id.trim().is_empty()
            || value.resource_id.len() > 2000
            || value.title.trim().is_empty()
            || value.title.len() > 500
            || !matches!(value.canonical_url.scheme(), "http" | "https")
            || value.canonical_url.host_str().is_none()
            || !value.canonical_url.username().is_empty()
            || value.canonical_url.password().is_some()
            || invalid_purposes
            || !ids.insert(&value.id)
        {
            return Err(ContentValidationError::InvalidEmbed);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContentValidationError {
    #[error("title must be 1 to 300 characters and contain no null bytes")]
    InvalidTitle,
    #[error("slug is not a safe single path segment")]
    InvalidSlug,
    #[error("Markdown exceeds the size limit or contains a null byte")]
    InvalidMarkdown,
    #[error("intent layer is invalid")]
    InvalidIntentLayer,
    #[error("ontology sidecar is invalid")]
    InvalidOntologyLayer,
    #[error("idempotency key is invalid")]
    InvalidIdempotencyKey,
    #[error("embed reference is invalid or duplicated")]
    InvalidEmbed,
    #[error("public authorship metadata is invalid")]
    InvalidAuthorship,
    #[error("AI summary text must be 1 to 2000 characters and contain no active controls")]
    InvalidAiSummaryText,
    #[error("AI summary provenance is invalid")]
    InvalidAiSummaryProvenance,
    #[error("AI summary must be reviewed by a human before it is stored or published")]
    AiSummaryRequiresHumanReview,
    #[error("AI summary was generated from different title or Markdown source")]
    AiSummarySourceMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reviewed_summary(title: &str, markdown: &str) -> AiSummary {
        AiSummary {
            text: "A short reviewed summary.".into(),
            source_hash: ai_summary_source_hash(title, markdown),
            provenance: AiSummaryProvenance {
                provider: "openai".into(),
                model: "example-model".into(),
                prompt_version: "summary-v1".into(),
                generated_at: "2026-07-22T00:00:00Z".parse().unwrap(),
                human_reviewed: true,
            },
        }
    }

    #[test]
    fn ontology_does_not_change_the_required_markdown() {
        let proposed = ProposedRevision {
            document_id: Uuid::now_v7(),
            base_revision_id: Uuid::now_v7(),
            title: "Portable first".into(),
            slug: "portable-first".into(),
            source_markdown: "# Portable first\n".into(),
            embeds: vec![],
            intent: None,
            ontology: Some(OntologySidecar {
                schema: "https://example.invalid/ontology/v1".into(),
                statements: vec![],
            }),
            ai_summary: None,
            authorship: Default::default(),
            actor: RevisionActor {
                kind: RevisionActorKind::Human,
                id: "owner".into(),
                display_name: None,
            },
            idempotency_key: None,
        };
        proposed.validate().unwrap();
        assert_eq!(proposed.source_markdown, "# Portable first\n");
    }

    #[test]
    fn rejects_path_traversal_slugs() {
        for slug in ["../secret", "a/b", "a\\b", ".hidden"] {
            assert_eq!(
                validate_slug(slug),
                Err(ContentValidationError::InvalidSlug)
            );
        }
    }

    #[test]
    fn content_hash_covers_the_intent_layer() {
        let a = content_hash("T", "t", "text", &[], None, None);
        let b = content_hash(
            "T",
            "t",
            "text",
            &[],
            Some(&IntentLayer {
                format: "enhanced-html-v1".into(),
                source_html: "<p>text</p>".into(),
                renderer_hints: BTreeMap::new(),
                provenance: None,
            }),
            None,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn ai_summary_source_hash_has_stable_exact_source_semantics() {
        let digest = ai_summary_source_hash("Title", "# Body\n");
        assert_eq!(
            digest,
            "sha256:fe6d9b2fcd4e34895f4b1a1404bcbe125281e2308151e74e1342b03809585585"
        );
        assert_ne!(digest, ai_summary_source_hash("Title!", "# Body\n"));
        assert_ne!(digest, ai_summary_source_hash("Title", "# Body"));
    }

    #[test]
    fn legacy_content_hash_is_unchanged_without_a_summary() {
        let legacy = content_hash("T", "t", "text", &[], None, None);
        assert_eq!(
            legacy,
            "sha256:026e7f54c916501d42896afabf6b9767c937861fd800d335d0f86e8522d9c041"
        );
        assert_eq!(
            legacy,
            content_hash_with_ai_summary("T", "t", "text", &[], None, None, None)
        );

        let summary = reviewed_summary("T", "text");
        let summarized =
            content_hash_with_ai_summary("T", "t", "text", &[], None, None, Some(&summary));
        assert_ne!(legacy, summarized);

        let mut different_provenance = summary;
        different_provenance.provenance.model = "another-model".into();
        assert_ne!(
            summarized,
            content_hash_with_ai_summary(
                "T",
                "t",
                "text",
                &[],
                None,
                None,
                Some(&different_provenance)
            )
        );
    }

    #[test]
    fn ready_ai_summary_requires_matching_source_and_human_review() {
        let mut summary = reviewed_summary("Title", "Body");
        validate_ready_ai_summary("Title", "Body", &summary).unwrap();

        summary.provenance.human_reviewed = false;
        assert_eq!(
            validate_ready_ai_summary("Title", "Body", &summary),
            Err(ContentValidationError::AiSummaryRequiresHumanReview)
        );

        summary.provenance.human_reviewed = true;
        assert_eq!(
            validate_ready_ai_summary("Changed title", "Body", &summary),
            Err(ContentValidationError::AiSummarySourceMismatch)
        );
        assert_eq!(
            validate_ready_ai_summary("Title", "Changed body", &summary),
            Err(ContentValidationError::AiSummarySourceMismatch)
        );
    }

    #[test]
    fn ready_ai_summary_rejects_unsafe_or_unbounded_public_metadata() {
        let mut summary = reviewed_summary("Title", "Body");
        summary.text = "\u{7}".into();
        assert_eq!(
            summary.validate_for_source("Title", "Body"),
            Err(ContentValidationError::InvalidAiSummaryText)
        );

        summary.text = "x".repeat(AI_SUMMARY_MAX_CHARACTERS + 1);
        assert_eq!(
            summary.validate_for_source("Title", "Body"),
            Err(ContentValidationError::InvalidAiSummaryText)
        );

        summary.text = "Reviewed".into();
        summary.provenance.provider = "provider\nwith-control".into();
        assert_eq!(
            summary.validate_for_source("Title", "Body"),
            Err(ContentValidationError::InvalidAiSummaryProvenance)
        );
    }

    #[test]
    fn ai_summary_wire_model_rejects_credential_fields() {
        let value = serde_json::json!({
            "text": "Reviewed",
            "sourceHash": ai_summary_source_hash("Title", "Body"),
            "apiKey": "must-never-cross-the-content-boundary",
            "provenance": {
                "provider": "openai",
                "model": "example-model",
                "promptVersion": "summary-v1",
                "generatedAt": "2026-07-22T00:00:00Z",
                "humanReviewed": true
            }
        });
        assert!(serde_json::from_value::<AiSummary>(value).is_err());

        let serialized = serde_json::to_value(reviewed_summary("Title", "Body")).unwrap();
        assert!(serialized.get("apiKey").is_none());
        assert!(serialized.get("credential").is_none());
    }

    #[test]
    fn optional_layers_and_embed_urls_are_structurally_bounded() {
        let mut proposed = ProposedRevision {
            document_id: Uuid::now_v7(),
            base_revision_id: Uuid::now_v7(),
            title: "Bounded".into(),
            slug: "bounded".into(),
            source_markdown: "text".into(),
            embeds: vec![EmbedReference {
                id: "demo".into(),
                provider: "video".into(),
                resource_id: "1".into(),
                canonical_url: Url::parse("https://user:secret@example.invalid/watch").unwrap(),
                title: "demo".into(),
                consent_purpose_ids: vec![],
            }],
            intent: None,
            ontology: None,
            ai_summary: None,
            authorship: Default::default(),
            actor: RevisionActor {
                kind: RevisionActorKind::Human,
                id: "owner".into(),
                display_name: None,
            },
            idempotency_key: None,
        };
        assert_eq!(
            proposed.validate(),
            Err(ContentValidationError::InvalidEmbed)
        );
        proposed.embeds.clear();
        proposed.ontology = Some(OntologySidecar {
            schema: "not a URI".into(),
            statements: vec![],
        });
        assert_eq!(
            proposed.validate(),
            Err(ContentValidationError::InvalidOntologyLayer)
        );
    }

    #[test]
    fn public_authorship_requires_safe_portable_generator_metadata() {
        assert_eq!(
            validate_authorship(&PublicAuthorship {
                kind: PublicAuthorshipKind::AiGenerated,
                generator: None,
                human_reviewed: false,
            }),
            Err(ContentValidationError::InvalidAuthorship)
        );
        assert_eq!(
            validate_authorship(&PublicAuthorship {
                kind: PublicAuthorshipKind::Human,
                generator: Some("internal-agent-id".into()),
                human_reviewed: true,
            }),
            Err(ContentValidationError::InvalidAuthorship)
        );
        assert_eq!(
            validate_authorship(&PublicAuthorship {
                kind: PublicAuthorshipKind::Imported,
                generator: Some("source\nwith-control".into()),
                human_reviewed: true,
            }),
            Err(ContentValidationError::InvalidAuthorship)
        );
        validate_authorship(&PublicAuthorship {
            kind: PublicAuthorshipKind::AiAssisted,
            generator: Some("local/model-v1".into()),
            human_reviewed: true,
        })
        .unwrap();
    }
}
