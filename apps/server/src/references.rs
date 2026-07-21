use axum::{
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use osb_renderer::{RENDERER_VERSION, render_markdown};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{ApiError, AppState, config::ReferencesSettings, public_cached_response};

#[derive(Clone)]
pub(crate) struct ReferencesPage {
    label: String,
    source_markdown: String,
    artifact_html: String,
    source_hash: String,
}

impl ReferencesPage {
    pub(crate) fn new(settings: ReferencesSettings) -> Self {
        let source_hash = format!(
            "sha256:{:x}",
            Sha256::digest(settings.source_markdown.as_bytes())
        );
        let artifact_html = render_markdown(&settings.source_markdown);
        Self {
            label: settings.label,
            source_markdown: settings.source_markdown,
            artifact_html,
            source_hash,
        }
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn cache_identity(&self) -> (&str, &str, &'static str) {
        (&self.label, &self.source_hash, RENDERER_VERSION)
    }

    pub(crate) fn source_markdown(&self) -> &str {
        &self.source_markdown
    }

    pub(crate) fn artifact_html(&self) -> &str {
        &self.artifact_html
    }

    pub(crate) fn source_hash(&self) -> &str {
        &self.source_hash
    }

    fn view(&self) -> ReferencesPageView<'_> {
        ReferencesPageView {
            label: &self.label,
            source_markdown: &self.source_markdown,
            artifact_html: &self.artifact_html,
            source_hash: &self.source_hash,
            renderer_version: RENDERER_VERSION,
        }
    }
}

pub(crate) async fn get(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some(page) = state.references.as_ref() else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let body = serde_json::to_vec(&page.view()).map_err(|error| {
        ApiError::Internal(format!("references page is not serializable: {error}"))
    })?;
    public_cached_response(method, &headers, body, "application/json; charset=utf-8")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferencesPageView<'a> {
    label: &'a str,
    source_markdown: &'a str,
    artifact_html: &'a str,
    source_hash: &'a str,
    renderer_version: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_markdown_is_rendered_through_the_safe_portable_path() {
        let page = ReferencesPage::new(ReferencesSettings {
            label: "자료실".into(),
            source_markdown: "## 출처\n\n<script>alert(1)</script>\n\n**안전한 본문**".into(),
        });
        let view = page.view();
        assert_eq!(view.label, "자료실");
        assert!(view.artifact_html.contains("<strong>안전한 본문</strong>"));
        assert!(!view.artifact_html.contains("<script>"));
        assert!(view.source_hash.starts_with("sha256:"));
        assert_eq!(view.renderer_version, RENDERER_VERSION);
    }
}
