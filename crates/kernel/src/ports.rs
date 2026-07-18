use uuid::Uuid;

use crate::{
    Ai2AiEnvelope, AiProposalAuditRecord, DocumentSnapshot, NewDocument, ProposedRevision,
    RevisionSnapshot,
};

pub trait ContentRepository: Send + Sync {
    fn create_document(&self, input: NewDocument) -> Result<DocumentSnapshot, RepositoryError>;

    fn get_document(&self, id: Uuid) -> Result<DocumentSnapshot, RepositoryError>;

    fn get_published_by_slug(
        &self,
        site_id: Uuid,
        slug: &str,
    ) -> Result<DocumentSnapshot, RepositoryError>;

    fn list_published(
        &self,
        site_id: Uuid,
        limit: usize,
    ) -> Result<Vec<DocumentSnapshot>, RepositoryError>;

    /// Lists current snapshots for an authenticated administration surface.
    /// Unlike `list_published`, this includes drafts and never belongs on an
    /// unauthenticated route.
    fn list_documents(
        &self,
        site_id: Uuid,
        limit: usize,
    ) -> Result<Vec<DocumentSnapshot>, RepositoryError>;

    /// Returns immutable history newest-first for review and rollback tools.
    fn list_revisions(
        &self,
        document_id: Uuid,
        limit: usize,
    ) -> Result<Vec<RevisionSnapshot>, RepositoryError>;

    fn append_revision(&self, input: ProposedRevision)
    -> Result<RevisionSnapshot, RepositoryError>;

    /// Validates and accepts an AI2AI proposal while durably recording its
    /// complete policy, context receipts, and provenance in the same commit as
    /// the resulting revision.
    fn append_ai_proposal(
        &self,
        envelope: Ai2AiEnvelope,
    ) -> Result<RevisionSnapshot, RepositoryError>;

    /// Lists immutable AI2AI acceptance receipts newest-first.
    fn list_ai_proposals(
        &self,
        document_id: Uuid,
        limit: usize,
    ) -> Result<Vec<AiProposalAuditRecord>, RepositoryError>;

    fn publish(
        &self,
        document_id: Uuid,
        revision_id: Uuid,
    ) -> Result<DocumentSnapshot, RepositoryError>;
}

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("content was not found")]
    NotFound,
    #[error("the slug already exists")]
    DuplicateSlug,
    #[error("the base revision is stale")]
    RevisionConflict,
    #[error("the request was already accepted")]
    DuplicateIdempotencyKey,
    #[error("invalid content: {0}")]
    Validation(String),
    #[error("storage error: {0}")]
    Storage(String),
}
