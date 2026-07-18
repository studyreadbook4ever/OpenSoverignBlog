use chrono::{DateTime, Utc};
use osb_renderer::render_markdown;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentStatus {
    Pending,
    Approved,
    Rejected,
    Spam,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: Uuid,
    pub site_id: Uuid,
    pub document_id: Uuid,
    pub author_reference: String,
    pub source_markdown: String,
    pub status: CommentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentSubmission {
    pub site_id: Uuid,
    pub document_id: Uuid,
    pub author_reference: String,
    pub source_markdown: String,
}

impl CommentSubmission {
    pub fn validate(&self) -> Result<(), CommentError> {
        if self.author_reference.trim().is_empty() || self.author_reference.len() > 300 {
            return Err(CommentError::InvalidAuthor);
        }
        let length = self.source_markdown.trim().chars().count();
        if !(1..=20_000).contains(&length) || self.source_markdown.contains('\0') {
            return Err(CommentError::InvalidBody);
        }
        Ok(())
    }

    pub fn into_pending(self) -> Result<Comment, CommentError> {
        self.validate()?;
        let now = Utc::now();
        Ok(Comment {
            id: Uuid::now_v7(),
            site_id: self.site_id,
            document_id: self.document_id,
            author_reference: self.author_reference,
            source_markdown: self.source_markdown,
            status: CommentStatus::Pending,
            created_at: now,
            updated_at: now,
        })
    }
}

impl Comment {
    pub fn moderate(&mut self, next: CommentStatus) -> Result<(), CommentError> {
        let allowed = matches!(
            (self.status, next),
            (CommentStatus::Pending, CommentStatus::Approved)
                | (CommentStatus::Pending, CommentStatus::Rejected)
                | (CommentStatus::Pending, CommentStatus::Spam)
                | (CommentStatus::Approved, CommentStatus::Rejected)
                | (CommentStatus::Approved, CommentStatus::Spam)
                | (_, CommentStatus::Deleted)
        );
        if !allowed {
            return Err(CommentError::InvalidTransition);
        }
        self.status = next;
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn render_if_approved(&self) -> Option<String> {
        (self.status == CommentStatus::Approved).then(|| render_markdown(&self.source_markdown))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CommentError {
    #[error("comment author reference is invalid")]
    InvalidAuthor,
    #[error("comment body is invalid")]
    InvalidBody,
    #[error("comment moderation transition is invalid")]
    InvalidTransition,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_are_hidden_until_moderated_and_share_the_xss_boundary() {
        let mut comment = CommentSubmission {
            site_id: Uuid::now_v7(),
            document_id: Uuid::now_v7(),
            author_reference: "external:user-1".into(),
            source_markdown: "hello <img src=x onerror=alert(1)>".into(),
        }
        .into_pending()
        .unwrap();
        assert!(comment.render_if_approved().is_none());
        comment.moderate(CommentStatus::Approved).unwrap();
        let html = comment.render_if_approved().unwrap();
        assert!(!html.contains("<img"));
        assert!(html.contains("&lt;img"));
    }
}
