//! Deterministic clean-room publish renderer.
//!
//! Markdown and optional author-intent HTML are untrusted. Raw author HTML is
//! never interpreted by the Markdown path; intent HTML passes through the same
//! strict sanitizer used by preview and publication.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use ammonia::Builder;
use osb_kernel::{EmbedReference, RevisionSnapshot};
use pulldown_cmark::{CowStr, Event, Options, Parser, html};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

pub const RENDERER_VERSION: &str = "osb-renderer/0.1.0";
pub const SANITIZER_POLICY_VERSION: &str = "strict-html/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    Intent,
    Markdown,
    MarkdownSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishArtifact {
    pub view: ViewMode,
    pub html: String,
    pub source_hash: String,
    pub artifact_hash: String,
    pub renderer_version: String,
    pub sanitizer_policy_version: String,
    pub required_style_assets: Vec<String>,
    pub required_script_assets: Vec<String>,
}

pub fn render_revision(revision: &RevisionSnapshot, view: ViewMode) -> PublishArtifact {
    let (html, source_hash) = match view {
        ViewMode::Intent => revision.intent.as_ref().map_or_else(
            || {
                (
                    render_markdown_with_embeds(&revision.source_markdown, &revision.embeds),
                    revision.content_hash.clone(),
                )
            },
            |intent| {
                (
                    inject_intent_embeds(
                        &sanitize_untrusted_html(&intent.source_html),
                        &revision.embeds,
                    ),
                    hash_bytes(intent.source_html.as_bytes()),
                )
            },
        ),
        ViewMode::Markdown => (
            render_markdown_with_embeds(&revision.source_markdown, &revision.embeds),
            hash_bytes(revision.source_markdown.as_bytes()),
        ),
        ViewMode::MarkdownSource => {
            let escaped = escape_html(&revision.source_markdown);
            (
                format!("<pre class=\"osb-markdown-source\"><code>{escaped}</code></pre>"),
                hash_bytes(revision.source_markdown.as_bytes()),
            )
        }
    };
    let artifact_hash = hash_bytes(html.as_bytes());
    PublishArtifact {
        view,
        html,
        source_hash,
        artifact_hash,
        renderer_version: RENDERER_VERSION.into(),
        sanitizer_policy_version: SANITIZER_POLICY_VERSION.into(),
        required_style_assets: vec![
            "/assets/osb-content.css".into(),
            "/vendor/katex/katex.min.css".into(),
        ],
        required_script_assets: vec![
            "/vendor/katex/katex.min.js".into(),
            "/assets/osb-content.js".into(),
        ],
    }
}

pub fn render_markdown(source: &str) -> String {
    render_markdown_with_embeds(source, &[])
}

pub fn summarize_markdown(source: &str, max_characters: usize) -> String {
    let mut summary = String::new();
    let mut suppress_raw_active_content = false;
    for event in Parser::new_ext(source, Options::empty()) {
        match event {
            Event::Text(value) | Event::Code(value) if !suppress_raw_active_content => {
                if !summary.is_empty() && !summary.ends_with(char::is_whitespace) {
                    summary.push(' ');
                }
                summary.push_str(&value);
            }
            Event::SoftBreak | Event::HardBreak => summary.push(' '),
            Event::Html(value) | Event::InlineHtml(value) => {
                let markup = value.to_ascii_lowercase();
                if markup.contains("<script") || markup.contains("<style") {
                    suppress_raw_active_content = true;
                }
                if markup.contains("</script") || markup.contains("</style") {
                    suppress_raw_active_content = false;
                }
            }
            _ => {}
        }
        if summary.chars().count() >= max_characters {
            break;
        }
    }
    let normalized = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut output: String = normalized.chars().take(max_characters).collect();
    if normalized.chars().count() > max_characters {
        output.push('…');
    }
    output
}

pub fn render_markdown_with_embeds(source: &str, embeds: &[EmbedReference]) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    options.insert(Options::ENABLE_MATH);

    let parser = Parser::new_ext(source, options).map(|event| match event {
        // Raw HTML is rendered as text in the portable Markdown path. Authors
        // who explicitly enable an intent layer still receive sanitization.
        Event::Html(value) | Event::InlineHtml(value) => Event::Text(value),
        Event::Text(value) => parse_embed_directive(&value)
            .and_then(|id| embeds.iter().find(|embed| embed.id == id))
            .map_or(Event::Text(value), trusted_embed_event),
        Event::InlineMath(value) => trusted_math_event(value, false),
        Event::DisplayMath(value) => trusted_math_event(value, true),
        other => other,
    });
    let mut output = String::with_capacity(source.len() + source.len() / 2);
    html::push_html(&mut output, parser);
    sanitize_generated_html(&output)
}

fn parse_embed_directive(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    trimmed.strip_prefix("::osb-embed ").filter(|value| {
        !value.is_empty()
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
    })
}

fn trusted_embed_event<'a>(embed: &EmbedReference) -> Event<'a> {
    Event::Html(CowStr::Boxed(embed_facade(embed).into_boxed_str()))
}

fn inject_intent_embeds(source: &str, embeds: &[EmbedReference]) -> String {
    let mut output = source.to_owned();
    for embed in embeds {
        output = output.replace(&format!("{{{{embed:{}}}}}", embed.id), &embed_facade(embed));
    }
    sanitize_generated_html(&output)
}

fn embed_facade(embed: &EmbedReference) -> String {
    let provider = escape_html(&embed.provider);
    let title = escape_html(&embed.title);
    let id = escape_html(&embed.id);
    let href = if matches!(embed.canonical_url.scheme(), "http" | "https") {
        escape_html(embed.canonical_url.as_str())
    } else {
        "#".into()
    };
    format!(
        "<figure id=\"embed-{id}\" class=\"osb-embed osb-embed-{provider}\"><figcaption>{title}</figcaption><a href=\"{href}\">Open {provider} content</a></figure>"
    )
}

pub fn sanitize_untrusted_html(source: &str) -> String {
    sanitizer().clean(source).to_string()
}

fn sanitize_generated_html(source: &str) -> String {
    // Generated HTML is still sanitized. The distinction is provenance, not a
    // bypass around the policy boundary.
    sanitizer().clean(source).to_string()
}

fn trusted_math_event<'a>(source: CowStr<'a>, display: bool) -> Event<'a> {
    let class = if display {
        "osb-math osb-math-display"
    } else {
        "osb-math osb-math-inline"
    };
    let tag = if display { "div" } else { "span" };
    let escaped = escape_html(&source);
    Event::Html(CowStr::Boxed(
        format!("<{tag} class=\"{class}\"><code>{escaped}</code></{tag}>").into_boxed_str(),
    ))
}

fn sanitizer<'a>() -> Builder<'a> {
    let tags: HashSet<&str> = [
        "a",
        "article",
        "aside",
        "blockquote",
        "br",
        "code",
        "del",
        "details",
        "div",
        "em",
        "figcaption",
        "figure",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "img",
        "input",
        "kbd",
        "li",
        "mark",
        "ol",
        "p",
        "pre",
        "s",
        "section",
        "small",
        "span",
        "strong",
        "sub",
        "summary",
        "sup",
        "table",
        "tbody",
        "td",
        "th",
        "thead",
        "tr",
        "ul",
    ]
    .into_iter()
    .collect();
    let generic_attributes: HashSet<&str> = ["class", "id", "lang", "title"].into_iter().collect();
    let mut tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::new();
    tag_attributes.insert("a", ["href", "hreflang", "target"].into_iter().collect());
    tag_attributes.insert(
        "img",
        ["src", "alt", "width", "height", "loading", "decoding"]
            .into_iter()
            .collect(),
    );
    tag_attributes.insert(
        "input",
        ["type", "checked", "disabled"].into_iter().collect(),
    );
    tag_attributes.insert("ol", ["start", "reversed"].into_iter().collect());
    tag_attributes.insert("td", ["colspan", "rowspan"].into_iter().collect());
    tag_attributes.insert("th", ["colspan", "rowspan", "scope"].into_iter().collect());

    let schemes: HashSet<&str> = ["http", "https", "mailto"].into_iter().collect();
    let mut builder = Builder::new();
    builder
        .tags(tags)
        .generic_attributes(generic_attributes)
        .tag_attributes(tag_attributes)
        .url_schemes(schemes)
        // Passive content may not create a third-party request. Remote media
        // must enter through a typed embed/provider adapter and the consent
        // resource gate. Relative image URLs remain available for first-party
        // assets and exports.
        .attribute_filter(|element, attribute, value| {
            if element == "img" && attribute == "src" && !is_first_party_relative_url(value) {
                None
            } else {
                Some(Cow::Borrowed(value))
            }
        })
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true);
    builder
}

fn is_first_party_relative_url(value: &str) -> bool {
    if value.is_empty()
        || value != value.trim()
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return false;
    }
    let base = Url::parse("https://open-soverign-blog.invalid/")
        .expect("the fixed sanitizer base URL is valid");
    base.join(value).is_ok_and(|resolved| {
        resolved.scheme() == base.scheme()
            && resolved.host_str() == base.host_str()
            && resolved.port_or_known_default() == base.port_or_known_default()
            && resolved.username().is_empty()
            && resolved.password().is_none()
    })
}

fn hash_bytes(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}

fn escape_html(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    for character in source.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_html_in_markdown_is_not_executed() {
        let output = render_markdown("hello <img src=x onerror='alert(1)'><script>x</script>");
        assert!(!output.contains("<script"));
        assert!(!output.contains("<img"));
        assert!(output.contains("&lt;img"));
        assert!(output.contains("&lt;script&gt;"));
    }

    #[test]
    fn intent_html_uses_a_strict_allowlist() {
        let output = sanitize_untrusted_html(
            "<p onclick=\"x()\">safe</p><iframe src=\"https://evil.invalid\"></iframe><a href=\"javascript:x()\">x</a>",
        );
        assert_eq!(output, "<p>safe</p><a rel=\"noopener noreferrer\">x</a>");
    }

    #[test]
    fn math_is_preserved_as_a_typed_render_target() {
        let output = render_markdown("Inline $x^2$ and $$y = mx + b$$");
        assert!(output.contains("osb-math-inline"));
        assert!(output.contains("osb-math-display"));
        assert!(output.contains("x^2"));
    }

    #[test]
    fn source_view_is_always_escaped() {
        let escaped = escape_html("<script>'x'</script>");
        assert_eq!(escaped, "&lt;script&gt;&#39;x&#39;&lt;/script&gt;");
    }

    #[test]
    fn typed_embeds_render_as_first_party_facades_without_iframes() {
        let embed = EmbedReference {
            id: "demo".into(),
            provider: "video".into(),
            resource_id: "abc123".into(),
            canonical_url: "https://video.example/watch/abc123".parse().unwrap(),
            title: "A video".into(),
            consent_purpose_ids: vec!["external_content".into()],
        };
        let output = render_markdown_with_embeds("::osb-embed demo", &[embed]);
        assert!(
            output.contains("class=\"osb-embed osb-embed-video\""),
            "{output}"
        );
        assert!(output.contains("https://video.example/watch/abc123"));
        assert!(!output.contains("iframe"));
        assert!(!output.contains("script"));
    }

    #[test]
    fn passive_html_cannot_make_a_pre_consent_third_party_request() {
        let output = sanitize_untrusted_html(
            r#"<img src="https://tracker.example/pixel" alt="remote"><img src="/media/local.png" alt="local"><img src="//tracker.example/pixel" alt="scheme-relative"><img src="\\tracker.example/pixel" alt="backslash"><img src="/\tracker.example/pixel" alt="mixed">"#,
        );
        assert!(!output.contains("tracker.example"), "{output}");
        assert!(output.contains("src=\"/media/local.png\""), "{output}");
    }

    #[test]
    fn seo_summary_contains_text_but_never_raw_html() {
        let summary = summarize_markdown("# Title\n\nHello **world** <script>x</script>", 80);
        assert_eq!(summary, "Title Hello world");
    }
}
