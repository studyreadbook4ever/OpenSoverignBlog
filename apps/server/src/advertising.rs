use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use osb_feature_monetization_policy::{
    KAKAO_ADFIT_CONSENT_PURPOSE_IDS, KAKAO_ADFIT_POLICY_VERSION, KAKAO_ADFIT_SCRIPT_URL,
    KakaoAdFitPlacement, KakaoAdFitUnits, KakaoAdFitViewport,
};
use serde::{Deserialize, Serialize};

use crate::{AppState, admin_auth};

const CONSENT_COOKIE: &str = "osb_adfit_consent_v1";
const CONSENT_MAX_AGE_SECONDS: u64 = 365 * 24 * 60 * 60;
const CONSENT_BODY_LIMIT: usize = 1024;

pub(super) const ADVERTISING_SECURITY_CSP: &str = "default-src 'none'; script-src 'self' https://t1.kakaocdn.net; style-src 'self'; style-src-elem 'self' 'unsafe-inline'; style-src-attr 'unsafe-inline'; img-src 'self' data: https://t1.kakaocdn.net https://analytics.ad.daum.net https://kaat.daum.net; font-src 'self'; connect-src 'self' https://serv.ds.kakao.com https://display.ad.daum.net https://aem-kakao-collector.onkakao.net; frame-src https://www.youtube-nocookie.com https://t1.kakaocdn.net https://t1.daumcdn.net; base-uri 'self'; form-action 'self'; frame-ancestors 'self'; object-src 'none'";

pub(super) fn is_public_reader_path(path: &str) -> bool {
    if path.starts_with("//") {
        return false;
    }
    let path = path.trim_end_matches('/');
    if matches!(path, "" | "/" | "/index.html" | "/references") {
        return true;
    }
    if !path.starts_with('/') {
        return false;
    }
    let segments = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    if segments.is_empty() || segments.iter().any(|segment| segment.is_empty()) {
        return false;
    }
    if let Some(handle) = segments[0].strip_prefix('@') {
        return !handle.is_empty() && (1..=3).contains(&segments.len());
    }
    if segments[0] == "blog" {
        return segments.len() == 2;
    }
    const RESERVED: &[&str] = &[
        ".well-known",
        "ai2ai.md",
        "agent.txt",
        "ads.txt",
        "agents.txt",
        "api",
        "assets",
        "blog",
        "custom.css",
        "docs",
        "favicon.svg",
        "healthz",
        "index.css",
        "index.html",
        "livez",
        "llms.txt",
        "login",
        "media",
        "onboarding",
        "openapi",
        "providers",
        "readyz",
        "references",
        "robots.txt",
        "schemas",
        "sitemap.xml",
        "studio",
        "unlicense",
        "vendor",
    ];
    let normalized_root = segments[0].to_ascii_lowercase();
    (1..=2).contains(&segments.len()) && !RESERVED.contains(&normalized_root.as_str())
}

pub(super) fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/v1/advertising/consent", get(get_consent))
        .route("/api/v1/advertising/consent", post(set_consent))
        .layer(DefaultBodyLimit::max(CONSENT_BODY_LIMIT))
        .layer(middleware::from_fn(private_no_store))
        .with_state(state)
}

async fn private_no_store(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ConsentStatus {
    Unknown,
    Granted,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ConsentDecision {
    Granted,
    Denied,
}

impl From<ConsentDecision> for ConsentStatus {
    fn from(value: ConsentDecision) -> Self {
        match value {
            ConsentDecision::Granted => Self::Granted,
            ConsentDecision::Denied => Self::Denied,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConsentOutput {
    decision: ConsentStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConsentInput {
    decision: ConsentDecision,
}

async fn get_consent(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if state.kakao_adfit.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(ConsentOutput {
        decision: consent_from_headers(&headers),
    })
    .into_response()
}

async fn set_consent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ConsentInput>,
) -> Response {
    if state.kakao_adfit.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !headers.contains_key(header::ORIGIN)
        || !admin_auth::request_origin_is_valid(&state, &headers)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "invalid_origin",
                "message": "Advertising consent can be changed only by this site's browser origin."
            })),
        )
            .into_response();
    }

    let decision = ConsentStatus::from(input.decision);
    let secure = if state.secure_session_cookie {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{CONSENT_COOKIE}={}; Path=/; Max-Age={CONSENT_MAX_AGE_SECONDS}; HttpOnly; SameSite=Lax{secure}",
        match decision {
            ConsentStatus::Granted => "granted",
            ConsentStatus::Denied => "denied",
            ConsentStatus::Unknown => unreachable!("POST accepts only terminal decisions"),
        }
    );
    let mut response = Json(ConsentOutput { decision }).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("static consent cookie is a valid header"),
    );
    response
}

fn consent_from_headers(headers: &HeaderMap) -> ConsentStatus {
    let Some(raw) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    else {
        return ConsentStatus::Unknown;
    };
    let mut found = None;
    for pair in raw.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        if name != CONSENT_COOKIE {
            continue;
        }
        if found.is_some() {
            return ConsentStatus::Unknown;
        }
        found = Some(match value {
            "granted" => ConsentStatus::Granted,
            "denied" => ConsentStatus::Denied,
            _ => ConsentStatus::Unknown,
        });
    }
    found.unwrap_or(ConsentStatus::Unknown)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AdvertisingCapabilities {
    provider: &'static str,
    script_url: &'static str,
    policy_version: &'static str,
    consent: AdvertisingConsentDescriptor,
    placements: AdvertisingPlacements,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdvertisingConsentDescriptor {
    required: bool,
    status_href: &'static str,
    action_href: &'static str,
    purpose_ids: Vec<&'static str>,
    privacy_href: &'static str,
    policy_href: &'static str,
}

#[derive(Debug, Serialize)]
struct AdvertisingPlacements {
    top: ResponsivePlacement,
    bottom: ResponsivePlacement,
}

#[derive(Debug, Serialize)]
struct ResponsivePlacement {
    pc: AdUnitDescriptor,
    mobile: AdUnitDescriptor,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdUnitDescriptor {
    unit_id: String,
    width: u16,
    height: u16,
}

pub(super) fn capabilities(units: &KakaoAdFitUnits) -> AdvertisingCapabilities {
    AdvertisingCapabilities {
        provider: "kakao-adfit",
        script_url: KAKAO_ADFIT_SCRIPT_URL,
        policy_version: KAKAO_ADFIT_POLICY_VERSION,
        consent: AdvertisingConsentDescriptor {
            required: true,
            status_href: "/api/v1/advertising/consent",
            action_href: "/api/v1/advertising/consent",
            purpose_ids: KAKAO_ADFIT_CONSENT_PURPOSE_IDS.to_vec(),
            privacy_href: "https://business.kakao.com/policy/privacy/",
            policy_href: "https://adfit.kakao.com/web/html/use_kakao.html",
        },
        placements: AdvertisingPlacements {
            top: responsive_placement(units, KakaoAdFitPlacement::Top),
            bottom: responsive_placement(units, KakaoAdFitPlacement::Bottom),
        },
    }
}

fn responsive_placement(
    units: &KakaoAdFitUnits,
    placement: KakaoAdFitPlacement,
) -> ResponsivePlacement {
    ResponsivePlacement {
        pc: unit_descriptor(units, placement, KakaoAdFitViewport::Pc),
        mobile: unit_descriptor(units, placement, KakaoAdFitViewport::Mobile),
    }
}

fn unit_descriptor(
    units: &KakaoAdFitUnits,
    placement: KakaoAdFitPlacement,
    viewport: KakaoAdFitViewport,
) -> AdUnitDescriptor {
    let (width, height) = viewport.dimensions();
    AdUnitDescriptor {
        unit_id: units.unit(placement, viewport).expose().to_owned(),
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_parser_fails_closed_on_unknown_or_duplicate_values() {
        let mut headers = HeaderMap::new();
        assert_eq!(consent_from_headers(&headers), ConsentStatus::Unknown);

        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=x; osb_adfit_consent_v1=granted"),
        );
        assert_eq!(consent_from_headers(&headers), ConsentStatus::Granted);

        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("osb_adfit_consent_v1=granted; osb_adfit_consent_v1=denied"),
        );
        assert_eq!(consent_from_headers(&headers), ConsentStatus::Unknown);
    }

    #[test]
    fn advertising_security_scope_excludes_control_and_machine_routes() {
        for path in [
            "/login",
            "/onboarding",
            "/studio",
            "/studio/write",
            "/api/v1/capabilities",
            "/openapi/openapi.yaml",
            "/.well-known/open-soverign-blog.json",
            "/.WELL-KNOWN/open-soverign-blog.json",
            "/ads.txt",
            "/ADS.TXT",
            "/AI2AI.md",
            "/favicon.svg",
            "/INDEX.CSS",
            "/references/archive",
            "/blog",
            "/BLOG/legacy",
            "/@",
            "/@/post",
            "/ontology//post",
            "//",
            "//attacker.example/",
        ] {
            assert!(!is_public_reader_path(path), "{path}");
        }
        for path in [
            "/",
            "/yangja",
            "/ontology/post",
            "/@writer",
            "/@writer/series/post",
            "/references",
        ] {
            assert!(is_public_reader_path(path), "{path}");
        }
    }
}
