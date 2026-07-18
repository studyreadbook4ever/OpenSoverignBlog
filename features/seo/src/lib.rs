use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeoPolicy {
    pub public_url: Url,
    #[serde(default = "default_blog_path")]
    pub article_base_path: String,
    #[serde(default)]
    pub no_index: bool,
}

impl SeoPolicy {
    pub fn validate(&self) -> Result<(), SeoError> {
        if !matches!(self.public_url.scheme(), "http" | "https")
            || self.public_url.host_str().is_none()
            || !self.public_url.username().is_empty()
            || self.public_url.password().is_some()
            || self.public_url.query().is_some()
            || self.public_url.fragment().is_some()
        {
            return Err(SeoError::InvalidPublicUrl);
        }
        let public_path = self.public_url.path().trim_matches('/');
        if !public_path.is_empty()
            && public_path.split('/').any(|segment| {
                segment.is_empty()
                    || matches!(segment, "." | "..")
                    || !segment
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.~%".contains(&byte))
            })
        {
            return Err(SeoError::InvalidPublicUrl);
        }
        if self.article_base_path.split('/').any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || !segment.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
        }) {
            return Err(SeoError::InvalidBasePath);
        }
        Ok(())
    }

    pub fn article_route_pattern(&self) -> Result<String, SeoError> {
        self.validate()?;
        Ok(format!(
            "/{}/{{slug}}",
            self.article_base_path.trim_matches('/')
        ))
    }

    pub fn canonical_article_url(&self, slug: &str) -> Result<Url, SeoError> {
        self.validate()?;
        if slug.trim().is_empty()
            || slug.contains('/')
            || slug.contains('\\')
            || slug.chars().any(char::is_control)
        {
            return Err(SeoError::InvalidSlug);
        }
        let mut url = self.public_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| SeoError::CannotBeBaseUrl)?;
            segments.pop_if_empty();
            for segment in self.article_base_path.trim_matches('/').split('/') {
                if !segment.is_empty() {
                    segments.push(segment);
                }
            }
            segments.push(slug);
        }
        Ok(url)
    }

    /// Builds a public site resource URL without dropping an operator's
    /// reverse-proxy base path. `Url::join("sitemap.xml")` would interpret a
    /// path without a trailing slash as a file and accidentally strip it.
    pub fn public_resource_url(&self, segment: &str) -> Result<Url, SeoError> {
        self.validate()?;
        if segment.is_empty()
            || segment.contains('/')
            || segment.contains('\\')
            || segment.chars().any(char::is_control)
        {
            return Err(SeoError::InvalidResourcePath);
        }
        self.public_route_url(segment)
    }

    /// Builds an absolute URL for a fixed public route while preserving an
    /// operator-configured reverse-proxy base path.
    ///
    /// A single leading slash is accepted so route literals can be passed
    /// directly. Empty, dot, query, fragment, and backslash-bearing segments
    /// are rejected instead of being normalized into a different resource.
    pub fn public_route_url(&self, path: &str) -> Result<Url, SeoError> {
        self.validate()?;
        let path = path.strip_prefix('/').unwrap_or(path);
        if path.is_empty()
            || path.contains('\\')
            || path.contains(['?', '#'])
            || path.chars().any(char::is_control)
            || path
                .split('/')
                .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        {
            return Err(SeoError::InvalidResourcePath);
        }
        let mut url = self.public_url.clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| SeoError::CannotBeBaseUrl)?;
        segments.pop_if_empty();
        for segment in path.split('/') {
            segments.push(segment);
        }
        drop(segments);
        Ok(url)
    }
}

fn default_blog_path() -> String {
    "blog".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SeoError {
    #[error("article slug is unsafe")]
    InvalidSlug,
    #[error("configured public URL cannot own path segments")]
    CannotBeBaseUrl,
    #[error("public URL must use HTTP or HTTPS")]
    InvalidPublicUrl,
    #[error("article base path contains an unsafe segment")]
    InvalidBasePath,
    #[error("public resource path is unsafe")]
    InvalidResourcePath,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_slugs_are_encoded_without_becoming_an_open_redirect() {
        let policy = SeoPolicy {
            public_url: Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "writing".into(),
            no_index: false,
        };
        assert_eq!(
            policy.canonical_article_url("안녕하세요").unwrap().as_str(),
            "https://blog.example/base/writing/%EC%95%88%EB%85%95%ED%95%98%EC%84%B8%EC%9A%94"
        );
        assert!(policy.canonical_article_url("//evil.example").is_err());
        assert_eq!(
            policy.public_resource_url("sitemap.xml").unwrap().as_str(),
            "https://blog.example/base/sitemap.xml"
        );
    }

    #[test]
    fn public_routes_keep_the_reverse_proxy_base_path() {
        let policy = SeoPolicy {
            public_url: Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "writing".into(),
            no_index: false,
        };
        for (path, expected) in [
            ("/agents.txt", "https://blog.example/base/agents.txt"),
            ("llms.txt", "https://blog.example/base/llms.txt"),
            (
                "/.well-known/open-soverign-blog.json",
                "https://blog.example/base/.well-known/open-soverign-blog.json",
            ),
            (
                "/api/v1/capabilities",
                "https://blog.example/base/api/v1/capabilities",
            ),
            (
                "/openapi/openapi.yaml",
                "https://blog.example/base/openapi/openapi.yaml",
            ),
        ] {
            assert_eq!(policy.public_route_url(path).unwrap().as_str(), expected);
        }
    }

    #[test]
    fn public_routes_reject_normalization_and_authority_confusion() {
        let policy = SeoPolicy {
            public_url: Url::parse("https://blog.example/base/").unwrap(),
            article_base_path: "writing".into(),
            no_index: false,
        };
        for path in [
            "",
            "/",
            "//evil.example/path",
            "api//feed",
            "api/../admin",
            "api/./feed",
            "api/feed?draft=true",
            "api/feed#fragment",
            "api\\feed",
        ] {
            assert_eq!(
                policy.public_route_url(path),
                Err(SeoError::InvalidResourcePath),
                "unsafe path was accepted: {path}"
            );
        }
    }

    #[test]
    fn public_url_rejects_credentials_query_and_fragment() {
        for value in [
            "https://user:secret@blog.example/",
            "https://blog.example/?preview=1",
            "https://blog.example/#fragment",
        ] {
            let policy = SeoPolicy {
                public_url: Url::parse(value).unwrap(),
                article_base_path: "blog".into(),
                no_index: false,
            };
            assert_eq!(policy.validate(), Err(SeoError::InvalidPublicUrl));
        }
    }
}
