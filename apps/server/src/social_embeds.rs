use osb_kernel::EmbedReference;

/// Validates semantic contracts owned by the bundled social-embed DLC.
/// Unknown providers remain valid inert core references.
pub(crate) fn validate_official_embeds(embeds: &[EmbedReference]) -> Result<(), String> {
    for embed in embeds {
        match embed.provider.as_str() {
            "youtube" => validate_youtube(embed)?,
            "x" => validate_x(embed)?,
            _ => {}
        }
    }
    Ok(())
}

fn validate_youtube(embed: &EmbedReference) -> Result<(), String> {
    let resource = embed.resource_id.as_str();
    if resource.len() != 11
        || !resource
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("YouTube embed resourceId must be an 11-character video ID".into());
    }
    let expected = format!("https://www.youtube.com/watch?v={resource}");
    if embed.canonical_url.as_str() != expected {
        return Err("YouTube embed canonicalUrl must use the normalized watch URL".into());
    }
    Ok(())
}

fn validate_x(embed: &EmbedReference) -> Result<(), String> {
    if embed.resource_id.is_empty()
        || embed.resource_id.len() > 30
        || !embed.resource_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("X embed resourceId must be a numeric status ID".into());
    }
    let url = &embed.canonical_url;
    if url.scheme() != "https"
        || url.host_str() != Some("x.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("X embed canonicalUrl must be a normalized x.com status URL".into());
    }
    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    if segments.len() != 3
        || segments[1] != "status"
        || segments[2] != embed.resource_id
        || segments[0].is_empty()
        || segments[0].len() > 15
        || !segments[0]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(
            "X embed canonicalUrl must contain a valid handle and matching status ID".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn embed(provider: &str, resource_id: &str, canonical_url: &str) -> EmbedReference {
        EmbedReference {
            id: format!("{provider}-{resource_id}"),
            provider: provider.into(),
            resource_id: resource_id.into(),
            canonical_url: Url::parse(canonical_url).unwrap(),
            title: "reference".into(),
            consent_purpose_ids: vec![],
        }
    }

    #[test]
    fn known_providers_require_normalized_semantics() {
        validate_official_embeds(&[
            embed(
                "youtube",
                "dQw4w9WgXcQ",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            ),
            embed("x", "123456789", "https://x.com/openai/status/123456789"),
        ])
        .unwrap();

        assert!(
            validate_official_embeds(&[embed(
                "youtube",
                "dQw4w9WgXcQ",
                "https://youtube.com/embed/dQw4w9WgXcQ",
            )])
            .is_err()
        );
        assert!(
            validate_official_embeds(&[embed(
                "x",
                "123456789",
                "https://x.com/openai/status/other",
            )])
            .is_err()
        );
    }

    #[test]
    fn unknown_providers_stay_inert_and_portable() {
        validate_official_embeds(&[embed(
            "example",
            "any-resource",
            "https://example.invalid/reference",
        )])
        .unwrap();
    }
}
