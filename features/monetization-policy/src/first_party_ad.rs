use serde::{Deserialize, Deserializer, Serialize};
use url::Url;

use crate::GateError;

pub const FIRST_PARTY_AD_SCHEMA_VERSION: &str = "1.0";
const MEDIA_PATH_PREFIX: &str = "/media/";
const MAX_CLICK_URL_LENGTH: usize = 2_048;
const MAX_SPONSOR_NAME_LENGTH: usize = 200;
const MAX_BODY_TEXT_LENGTH: usize = 2_000;
const MAX_ALT_TEXT_LENGTH: usize = 300;

/// The deliberately small first-party monetization vertical slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirstPartyAdKind {
    HouseAd,
    DirectSponsor,
}

/// Slots are typed instead of being arbitrary selectors or template fragments.
/// More slots can be added as separately reviewed contract versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedAdSlot {
    ArticleFooter,
}

impl NamedAdSlot {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArticleFooter => "article_footer",
        }
    }
}

/// The exact, visible English disclosure rendered adjacent to the creative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisclosureLabel {
    #[serde(rename = "Advertisement")]
    Advertisement,
    #[serde(rename = "Sponsored")]
    Sponsored,
}

impl DisclosureLabel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Advertisement => "Advertisement",
            Self::Sponsored => "Sponsored",
        }
    }
}

/// A content-addressed asset already stored by the first-party media service.
/// A host, scheme, query, fragment, or arbitrary path cannot be represented by
/// a valid value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstPartyImage {
    pub media_path: String,
    pub alt_text: String,
}

impl FirstPartyImage {
    pub fn digest(&self) -> Result<&str, GateError> {
        validate_media_path(&self.media_path)?;
        Ok(&self.media_path[MEDIA_PATH_PREFIX.len()..])
    }

    fn validate(&self) -> Result<(), GateError> {
        validate_media_path(&self.media_path)?;
        validate_plain_text(
            &self.alt_text,
            MAX_ALT_TEXT_LENGTH,
            "image alt text is empty or too long",
        )
    }
}

/// A machine-checkable declaration of the capabilities the static delivery
/// path does not use. All fields are required on the wire and all must remain
/// false. Adding an active capability requires a different, consent-aware
/// adapter and cannot silently widen this contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StaticDeliveryPolicy {
    pub third_party_fetches: bool,
    pub scripts: bool,
    pub tracking_pixels: bool,
    pub raw_html: bool,
    pub personalization: bool,
    pub browser_storage: bool,
    pub identifiers: bool,
    pub measurement: bool,
}

impl StaticDeliveryPolicy {
    fn validate(self) -> Result<(), GateError> {
        let restrictions = [
            ("third_party_fetches", self.third_party_fetches),
            ("scripts", self.scripts),
            ("tracking_pixels", self.tracking_pixels),
            ("raw_html", self.raw_html),
            ("personalization", self.personalization),
            ("browser_storage", self.browser_storage),
            ("identifiers", self.identifiers),
            ("measurement", self.measurement),
        ];
        if let Some((capability, _)) = restrictions.into_iter().find(|(_, active)| *active) {
            return Err(GateError::RestrictedStaticDelivery(capability));
        }
        Ok(())
    }
}

/// A house ad or direct sponsorship that can be rendered without an optional
/// resource request, script, storage operation, identifier, or personalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstPartyAd {
    pub schema_version: String,
    pub ad_id: String,
    pub kind: FirstPartyAdKind,
    pub slot: NamedAdSlot,
    pub disclosure: DisclosureLabel,
    pub sponsor_name: String,
    pub body_text: String,
    #[serde(
        default,
        deserialize_with = "deserialize_present_click_url",
        skip_serializing_if = "Option::is_none"
    )]
    pub click_url: Option<Url>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_image",
        skip_serializing_if = "Option::is_none"
    )]
    pub image: Option<FirstPartyImage>,
    pub delivery: StaticDeliveryPolicy,
}

impl FirstPartyAd {
    /// Fails closed unless the complete static-delivery contract is satisfied.
    ///
    /// This is a technical eligibility decision: it does not certify a legal
    /// basis or prevent the destination of a user-initiated link from applying
    /// its own privacy rules.
    pub fn authorize_without_consent(&self) -> Result<AuthorizedFirstPartyAd<'_>, GateError> {
        self.validate()?;
        Ok(AuthorizedFirstPartyAd { ad: self })
    }

    fn validate(&self) -> Result<(), GateError> {
        if self.schema_version != FIRST_PARTY_AD_SCHEMA_VERSION {
            return Err(GateError::InvalidStaticAd("unsupported schema version"));
        }
        validate_stable_id(&self.ad_id)?;
        let expected_disclosure = match self.kind {
            FirstPartyAdKind::HouseAd => DisclosureLabel::Advertisement,
            FirstPartyAdKind::DirectSponsor => DisclosureLabel::Sponsored,
        };
        if self.disclosure != expected_disclosure {
            return Err(GateError::InvalidStaticAd(
                "disclosure does not match the ad kind",
            ));
        }
        validate_plain_text(
            &self.sponsor_name,
            MAX_SPONSOR_NAME_LENGTH,
            "sponsor name is empty or too long",
        )?;
        validate_plain_text(
            &self.body_text,
            MAX_BODY_TEXT_LENGTH,
            "body text is empty or too long",
        )?;
        if let Some(click_url) = &self.click_url {
            validate_click_url(click_url)?;
        }
        if let Some(image) = &self.image {
            image.validate()?;
        }
        self.delivery.validate()
    }
}

/// An opaque proof that the strict static contract has passed validation.
/// Rendering is available only through this value.
#[must_use]
pub struct AuthorizedFirstPartyAd<'a> {
    ad: &'a FirstPartyAd,
}

impl AuthorizedFirstPartyAd<'_> {
    /// Produces a structured representation suitable for a trusted host UI.
    /// It contains no HTML, script, pixel, storage instruction, or arbitrary
    /// passive resource URL.
    pub fn render_plan(&self) -> StaticAdRenderPlan {
        StaticAdRenderPlan {
            ad_id: self.ad.ad_id.clone(),
            slot: self.ad.slot,
            disclosure: self.ad.disclosure,
            sponsor_name: self.ad.sponsor_name.clone(),
            body_text: self.ad.body_text.clone(),
            click_url: self.ad.click_url.clone(),
            image: self.ad.image.clone(),
        }
    }

    /// Produces deterministic, escaped markup with no inline script or style.
    /// The only passive URL it can emit is the validated same-origin media path.
    pub fn render_safe_html(&self) -> String {
        render_plan_as_html(&self.render_plan())
    }
}

/// Safe data for a host renderer. This type is output-only on the wire so an
/// untrusted caller must submit and validate `FirstPartyAd` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StaticAdRenderPlan {
    pub ad_id: String,
    pub slot: NamedAdSlot,
    pub disclosure: DisclosureLabel,
    pub sponsor_name: String,
    pub body_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_url: Option<Url>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<FirstPartyImage>,
}

fn validate_stable_id(value: &str) -> Result<(), GateError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b':' | b'-')
        });
    if !valid {
        return Err(GateError::InvalidStaticAd("ad id is invalid"));
    }
    Ok(())
}

fn validate_plain_text(
    value: &str,
    max_length: usize,
    length_error: &'static str,
) -> Result<(), GateError> {
    if value.trim().is_empty() || value.chars().count() > max_length {
        return Err(GateError::InvalidStaticAd(length_error));
    }
    if value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(GateError::InvalidStaticAd(
            "plain text contains a restricted control character",
        ));
    }
    Ok(())
}

fn validate_click_url(url: &Url) -> Result<(), GateError> {
    if url.as_str().len() > MAX_CLICK_URL_LENGTH
        || url.scheme() != "https"
        || url.cannot_be_a_base()
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(GateError::InvalidStaticAd(
            "click URL must be credential-free HTTPS without a fragment",
        ));
    }
    Ok(())
}

fn validate_media_path(value: &str) -> Result<(), GateError> {
    let Some(digest) = value.strip_prefix(MEDIA_PATH_PREFIX) else {
        return Err(GateError::InvalidStaticAd(
            "image must use a first-party media path",
        ));
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(GateError::InvalidStaticAd(
            "image media path must contain a lowercase SHA-256 digest",
        ));
    }
    Ok(())
}

// Optional means absent, not an explicitly supplied null. This keeps the Rust
// decoder aligned with the fail-closed JSON Schema.
fn deserialize_present_click_url<'de, D>(deserializer: D) -> Result<Option<Url>, D::Error>
where
    D: Deserializer<'de>,
{
    Url::deserialize(deserializer).map(Some)
}

fn deserialize_present_image<'de, D>(deserializer: D) -> Result<Option<FirstPartyImage>, D::Error>
where
    D: Deserializer<'de>,
{
    FirstPartyImage::deserialize(deserializer).map(Some)
}

fn render_plan_as_html(plan: &StaticAdRenderPlan) -> String {
    let mut html = String::with_capacity(512);
    html.push_str("<aside class=\"osb-static-ad\" data-osb-ad-id=\"");
    push_escaped_html(&mut html, &plan.ad_id);
    html.push_str("\" data-osb-slot=\"");
    html.push_str(plan.slot.as_str());
    html.push_str("\" aria-label=\"");
    html.push_str(plan.disclosure.as_str());
    html.push_str("\"><strong class=\"osb-static-ad__disclosure\">");
    html.push_str(plan.disclosure.as_str());
    html.push_str("</strong><p class=\"osb-static-ad__sponsor\">");
    push_escaped_html(&mut html, &plan.sponsor_name);
    html.push_str("</p>");
    if let Some(image) = &plan.image {
        html.push_str("<img class=\"osb-static-ad__image\" src=\"");
        push_escaped_html(&mut html, &image.media_path);
        html.push_str("\" alt=\"");
        push_escaped_html(&mut html, &image.alt_text);
        html.push_str("\" loading=\"lazy\" decoding=\"async\">");
    }
    html.push_str("<p class=\"osb-static-ad__body\">");
    push_escaped_html(&mut html, &plan.body_text);
    html.push_str("</p>");
    if let Some(click_url) = &plan.click_url {
        html.push_str("<a class=\"osb-static-ad__link\" href=\"");
        push_escaped_html(&mut html, click_url.as_str());
        html.push_str(
            "\" rel=\"nofollow sponsored noopener noreferrer\" referrerpolicy=\"no-referrer\">Visit sponsor</a>",
        );
    }
    html.push_str("</aside>");
    html
}

fn push_escaped_html(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_ad() -> FirstPartyAd {
        FirstPartyAd {
            schema_version: FIRST_PARTY_AD_SCHEMA_VERSION.into(),
            ad_id: "campaign:summer-1".into(),
            kind: FirstPartyAdKind::DirectSponsor,
            slot: NamedAdSlot::ArticleFooter,
            disclosure: DisclosureLabel::Sponsored,
            sponsor_name: "Example & Sons".into(),
            body_text: "A plain-text sponsorship message.".into(),
            click_url: Some(Url::parse("https://sponsor.example/offer?a=1&b=2").unwrap()),
            image: Some(FirstPartyImage {
                media_path: format!("/media/{}", "a".repeat(64)),
                alt_text: "Sponsor mark".into(),
            }),
            delivery: StaticDeliveryPolicy::default(),
        }
    }

    #[test]
    fn valid_static_ad_has_a_structured_plan_and_deterministic_markup() {
        let ad = valid_ad();
        let authorized = ad.authorize_without_consent().unwrap();
        let plan = authorized.render_plan();
        assert_eq!(plan.slot, NamedAdSlot::ArticleFooter);
        assert_eq!(
            plan.image.unwrap().media_path,
            format!("/media/{}", "a".repeat(64))
        );

        let html = authorized.render_safe_html();
        assert!(html.starts_with(
            "<aside class=\"osb-static-ad\" data-osb-ad-id=\"campaign:summer-1\" data-osb-slot=\"article_footer\""
        ));
        assert!(html.contains(">Sponsored</strong>"));
        assert!(html.contains("Example &amp; Sons"));
        assert!(html.contains("href=\"https://sponsor.example/offer?a=1&amp;b=2\""));
        assert!(html.contains("rel=\"nofollow sponsored noopener noreferrer\""));
        assert_eq!(html, authorized.render_safe_html());
    }

    #[test]
    fn model_text_is_escaped_in_every_html_context() {
        let mut ad = valid_ad();
        ad.sponsor_name = "<img src=x onerror=alert(1)>".into();
        ad.body_text = "<script>alert('xss')</script> & \"quoted\"".into();
        ad.image.as_mut().unwrap().alt_text = "\" onload=\"alert(2)".into();

        let html = ad.authorize_without_consent().unwrap().render_safe_html();
        assert!(!html.contains("<script"));
        assert!(!html.contains("<img src=x"));
        assert!(!html.contains(" onload=\"alert"));
        assert!(html.contains("&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"));
        assert!(html.contains("&quot; onload=&quot;alert(2)"));
    }

    #[test]
    fn external_or_malformed_image_sources_fail_closed() {
        for source in [
            "https://tracker.example/pixel.gif",
            "//tracker.example/pixel.gif",
            "/media/abc",
            &format!("/media/{}", "A".repeat(64)),
            &format!("/media/{}?track=1", "a".repeat(64)),
        ] {
            let mut ad = valid_ad();
            ad.image.as_mut().unwrap().media_path = source.into();
            assert!(matches!(
                ad.authorize_without_consent(),
                Err(GateError::InvalidStaticAd(_))
            ));
        }
    }

    #[test]
    fn restricted_delivery_capabilities_never_receive_static_authorization() {
        let policies = [
            (
                "third_party_fetches",
                StaticDeliveryPolicy {
                    third_party_fetches: true,
                    ..StaticDeliveryPolicy::default()
                },
            ),
            (
                "scripts",
                StaticDeliveryPolicy {
                    scripts: true,
                    ..StaticDeliveryPolicy::default()
                },
            ),
            (
                "tracking_pixels",
                StaticDeliveryPolicy {
                    tracking_pixels: true,
                    ..StaticDeliveryPolicy::default()
                },
            ),
            (
                "raw_html",
                StaticDeliveryPolicy {
                    raw_html: true,
                    ..StaticDeliveryPolicy::default()
                },
            ),
            (
                "personalization",
                StaticDeliveryPolicy {
                    personalization: true,
                    ..StaticDeliveryPolicy::default()
                },
            ),
            (
                "browser_storage",
                StaticDeliveryPolicy {
                    browser_storage: true,
                    ..StaticDeliveryPolicy::default()
                },
            ),
            (
                "identifiers",
                StaticDeliveryPolicy {
                    identifiers: true,
                    ..StaticDeliveryPolicy::default()
                },
            ),
            (
                "measurement",
                StaticDeliveryPolicy {
                    measurement: true,
                    ..StaticDeliveryPolicy::default()
                },
            ),
        ];

        for (name, policy) in policies {
            let mut ad = valid_ad();
            ad.delivery = policy;
            assert_eq!(
                ad.authorize_without_consent().err(),
                Some(GateError::RestrictedStaticDelivery(name))
            );
        }
    }

    #[test]
    fn unknown_tracking_or_html_fields_are_rejected_on_deserialization() {
        let mut value = serde_json::to_value(valid_ad()).unwrap();
        value["tracking_pixel_url"] = serde_json::json!("https://tracker.example/p.gif");
        assert!(serde_json::from_value::<FirstPartyAd>(value).is_err());

        let mut value = serde_json::to_value(valid_ad()).unwrap();
        value["raw_html"] = serde_json::json!("<script>alert(1)</script>");
        assert!(serde_json::from_value::<FirstPartyAd>(value).is_err());

        for key in ["click_url", "image"] {
            let mut value = serde_json::to_value(valid_ad()).unwrap();
            value[key] = serde_json::Value::Null;
            assert!(serde_json::from_value::<FirstPartyAd>(value).is_err());
        }
    }

    #[test]
    fn only_safe_user_initiated_https_destinations_are_allowed() {
        for destination in [
            "http://sponsor.example/",
            "javascript:alert(1)",
            "https://user:password@sponsor.example/",
            "https://sponsor.example/#campaign",
        ] {
            let mut ad = valid_ad();
            ad.click_url = Some(Url::parse(destination).unwrap());
            assert!(matches!(
                ad.authorize_without_consent(),
                Err(GateError::InvalidStaticAd(_))
            ));
        }
    }

    #[test]
    fn disclosure_is_exact_and_kind_specific() {
        let mut sponsor = valid_ad();
        sponsor.disclosure = DisclosureLabel::Advertisement;
        assert!(sponsor.authorize_without_consent().is_err());

        let mut house = valid_ad();
        house.kind = FirstPartyAdKind::HouseAd;
        house.disclosure = DisclosureLabel::Advertisement;
        assert!(house.authorize_without_consent().is_ok());
    }

    #[test]
    fn ids_and_plain_text_bounds_are_enforced() {
        for id in [
            "",
            "Uppercase",
            "-leading",
            "contains space",
            &"a".repeat(129),
        ] {
            let mut ad = valid_ad();
            ad.ad_id = id.into();
            assert!(ad.authorize_without_consent().is_err());
        }

        let mut ad = valid_ad();
        ad.sponsor_name = " ".into();
        assert!(ad.authorize_without_consent().is_err());

        let mut ad = valid_ad();
        ad.body_text = "a".repeat(MAX_BODY_TEXT_LENGTH + 1);
        assert!(ad.authorize_without_consent().is_err());

        let mut ad = valid_ad();
        ad.body_text = "message\u{0}".into();
        assert!(ad.authorize_without_consent().is_err());
    }
}
