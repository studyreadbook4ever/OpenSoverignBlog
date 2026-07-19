//! Modular instance-administrator authentication.
//!
//! Human credentials terminate here. Both the one-time access-key exchange and
//! an external OIDC login issue the same opaque HttpOnly browser session used by
//! Studio; neither credential is forwarded to content handlers.

use std::{
    sync::Arc,
    time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordVerifier},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    reqwest,
};
use osb_feature_external_auth::VerifiedExternalIdentity;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, Semaphore};

use crate::{
    AppState,
    admission::KeyedRateLimiter,
    community,
    config::{AdminAuthMode, AdminAuthSettings, ExternalAdminSettings},
};

const ACCESS_KEY_BODY_LIMIT: usize = 4 * 1024;
const ACCESS_KEY_ATTEMPTS_PER_CREDENTIAL_PER_MINUTE: usize = 6;
const ACCESS_KEY_BUCKET_LIMIT: usize = 512;
const ACCESS_KEY_VERIFICATION_CONCURRENCY: usize = 2;
const OIDC_START_CONCURRENCY: usize = 4;
const OIDC_STATE_LIFETIME: StdDuration = StdDuration::from_secs(10 * 60);
const OIDC_STATE_COOKIE: &str = "osb_oidc_state";

#[derive(Clone)]
pub(super) enum AdminAuthRuntime {
    AccessKey(Arc<AccessKeyRuntime>),
    External(Arc<ExternalRuntime>),
    Disabled,
}

impl AdminAuthRuntime {
    pub(super) fn from_settings(settings: &AdminAuthSettings) -> Result<Self> {
        match settings.mode {
            AdminAuthMode::AccessKey => {
                let phc = settings
                    .access_key_phc
                    .as_ref()
                    .context("access-key PHC is missing")?;
                PasswordHash::new(phc)
                    .map_err(|error| anyhow::anyhow!("access-key PHC is invalid: {error}"))?;
                Ok(Self::AccessKey(Arc::new(AccessKeyRuntime {
                    phc: Arc::from(phc.as_str()),
                    session_days: settings.session_days,
                    attempts: Arc::new(Mutex::new(KeyedRateLimiter::new(
                        ACCESS_KEY_BUCKET_LIMIT,
                        ACCESS_KEY_ATTEMPTS_PER_CREDENTIAL_PER_MINUTE,
                        StdDuration::from_secs(60),
                    ))),
                    verification_slots: Arc::new(Semaphore::new(
                        ACCESS_KEY_VERIFICATION_CONCURRENCY,
                    )),
                })))
            }
            AdminAuthMode::External => Ok(Self::External(Arc::new(ExternalRuntime {
                settings: settings
                    .external
                    .clone()
                    .context("external administrator settings are missing")?,
                session_days: settings.session_days,
                start_slots: Arc::new(Semaphore::new(OIDC_START_CONCURRENCY)),
            }))),
            AdminAuthMode::Disabled => Ok(Self::Disabled),
        }
    }

    pub(super) fn mode(&self) -> AdminAuthMode {
        match self {
            Self::AccessKey(_) => AdminAuthMode::AccessKey,
            Self::External(_) => AdminAuthMode::External,
            Self::Disabled => AdminAuthMode::Disabled,
        }
    }

    pub(super) fn external_label(&self) -> Option<&str> {
        match self {
            Self::External(runtime) => Some(&runtime.settings.label),
            _ => None,
        }
    }

    pub(super) fn external_adapter(&self) -> Option<&str> {
        match self {
            Self::External(runtime) => Some(&runtime.settings.adapter),
            _ => None,
        }
    }

    pub(super) fn binding_fingerprint(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"open-soverign-blog/admin-binding/v1\0");
        match self {
            Self::AccessKey(runtime) => {
                digest.update(b"access_key\0");
                digest.update(runtime.phc.as_bytes());
            }
            Self::External(runtime) => {
                digest.update(b"external\0");
                for part in [
                    runtime.settings.adapter.as_str(),
                    runtime.settings.issuer_url.as_str(),
                    runtime.settings.client_id.as_str(),
                    runtime.settings.owner_subject.as_str(),
                    runtime
                        .settings
                        .client_secret
                        .as_deref()
                        .unwrap_or_default(),
                ] {
                    digest.update(part.as_bytes());
                    digest.update(b"\0");
                }
            }
            Self::Disabled => digest.update(b"disabled\0"),
        }
        digest.finalize().into()
    }
}

pub(super) struct AccessKeyRuntime {
    phc: Arc<str>,
    session_days: i64,
    attempts: Arc<Mutex<KeyedRateLimiter>>,
    verification_slots: Arc<Semaphore>,
}

pub(super) struct ExternalRuntime {
    settings: ExternalAdminSettings,
    session_days: i64,
    start_slots: Arc<Semaphore>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OidcBrowserState {
    state: String,
    nonce: String,
    pkce_verifier: String,
    issued_at_unix: u64,
}

pub(super) fn routes(state: AppState) -> Router<AppState> {
    let routes = match &state.admin_auth {
        AdminAuthRuntime::AccessKey(_) => {
            Router::new().route("/api/v1/auth/access-key/session", post(exchange_access_key))
        }
        AdminAuthRuntime::External(_) => Router::new()
            .route("/api/v1/auth/external/start", get(start_external))
            .route("/api/v1/auth/external/callback", get(finish_external)),
        AdminAuthRuntime::Disabled => Router::new(),
    };
    routes
        .layer(DefaultBodyLimit::max(ACCESS_KEY_BODY_LIMIT))
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccessKeyInput {
    access_key: String,
}

async fn exchange_access_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<AccessKeyInput>,
) -> Response {
    let result = exchange_access_key_inner(&state, &headers, &input.access_key).await;
    input.access_key.clear();
    match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn exchange_access_key_inner(
    state: &AppState,
    headers: &HeaderMap,
    access_key: &str,
) -> Result<Response, AdminAuthError> {
    verify_origin(state, headers)?;
    let AdminAuthRuntime::AccessKey(runtime) = &state.admin_auth else {
        return Err(AdminAuthError::NotFound);
    };
    if !(32..=512).contains(&access_key.len()) || access_key.chars().any(char::is_control) {
        return Err(AdminAuthError::InvalidCredential);
    }
    let credential_key: [u8; 32] = Sha256::digest(access_key.as_bytes()).into();
    let permit = Arc::clone(&runtime.verification_slots)
        .try_acquire_owned()
        .map_err(|_| AdminAuthError::RateLimited)?;
    rate_limit_access_key(runtime, credential_key).await?;
    if let Some(cache) = &state.cache {
        let bucket = format!("administrator-access-key:{}", hex_digest(&credential_key));
        let admitted = cache
            .admit_fixed_window(
                &bucket,
                ACCESS_KEY_ATTEMPTS_PER_CREDENTIAL_PER_MINUTE as u64,
                60,
            )
            .await
            .map_err(|error| {
                tracing::warn!(%error, "distributed administrator login limiter is unavailable");
                AdminAuthError::Unavailable
            })?;
        if !admitted {
            return Err(AdminAuthError::RateLimited);
        }
    }
    let phc = Arc::clone(&runtime.phc);
    let mut candidate = access_key.as_bytes().to_vec();
    let verified = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let verified = PasswordHash::new(&phc).ok().is_some_and(|parsed| {
            Argon2::default()
                .verify_password(&candidate, &parsed)
                .is_ok()
        });
        candidate.fill(0);
        verified
    })
    .await
    .map_err(|_| AdminAuthError::Unavailable)?;
    if !verified {
        return Err(AdminAuthError::InvalidCredential);
    }
    runtime.attempts.lock().await.forget(&credential_key);
    administrator_session(state, runtime.session_days).await
}

async fn rate_limit_access_key(
    runtime: &AccessKeyRuntime,
    credential_key: [u8; 32],
) -> Result<(), AdminAuthError> {
    runtime
        .attempts
        .lock()
        .await
        .admit(credential_key)
        .then_some(())
        .ok_or(AdminAuthError::RateLimited)
}

async fn administrator_session(
    state: &AppState,
    session_days: i64,
) -> Result<Response, AdminAuthError> {
    let repository = Arc::clone(&state.repository);
    let site_id = state.site_id;
    let user = tokio::task::spawn_blocking(move || {
        let site = repository.ensure_legacy_site(site_id)?;
        repository.get_user_by_id(site.owner_user_id)
    })
    .await
    .map_err(|_| AdminAuthError::Unavailable)?
    .map_err(|error| AdminAuthError::Internal(error.to_string()))?;
    let auth_method = match state.admin_auth.mode() {
        AdminAuthMode::AccessKey => osb_storage_sqlite::SessionAuthMethod::AccessKey,
        AdminAuthMode::External => osb_storage_sqlite::SessionAuthMethod::External,
        AdminAuthMode::Disabled => return Err(AdminAuthError::NotFound),
    };
    community::administrator_authenticated_response(
        state,
        user,
        StatusCode::OK,
        auth_method,
        session_days,
    )
    .await
    .map_err(|error| AdminAuthError::Internal(format!("session creation failed: {error:?}")))
}

async fn start_external(State(state): State<AppState>) -> Result<Response, AdminAuthError> {
    let AdminAuthRuntime::External(runtime) = &state.admin_auth else {
        return Err(AdminAuthError::NotFound);
    };
    let _permit = Arc::clone(&runtime.start_slots)
        .try_acquire_owned()
        .map_err(|_| AdminAuthError::RateLimited)?;
    let (client, http_client) = oidc_client(&state, &runtime.settings).await?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("openid".into()))
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge)
        .url();
    drop(http_client);
    let browser_state = OidcBrowserState {
        state: csrf.secret().clone(),
        nonce: nonce.secret().clone(),
        pkce_verifier: verifier.secret().clone(),
        issued_at_unix: unix_time()?,
    };
    let binding_fingerprint = state.admin_auth.binding_fingerprint();
    let signed = encode_oidc_browser_state(
        state.cache_signing_key.as_ref(),
        &binding_fingerprint,
        &browser_state,
    )?;
    let mut response = Redirect::to(url.as_str()).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&oidc_state_cookie(&state, &signed))
            .map_err(|_| AdminAuthError::Unavailable)?,
    );
    Ok(response)
}

#[derive(Deserialize)]
struct OidcCallback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn finish_external(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OidcCallback>,
) -> Result<Response, AdminAuthError> {
    let AdminAuthRuntime::External(runtime) = &state.admin_auth else {
        return Err(AdminAuthError::NotFound);
    };
    if query.error.is_some() {
        return Err(AdminAuthError::InvalidCredential);
    }
    let code = query.code.filter(|value| (1..=4096).contains(&value.len()));
    let callback_state = query
        .state
        .filter(|value| (1..=1024).contains(&value.len()));
    let (code, callback_state) = code
        .zip(callback_state)
        .ok_or(AdminAuthError::InvalidCredential)?;
    let browser_state = decode_oidc_browser_state(&state, &headers)?;
    if !bool::from(
        browser_state
            .state
            .as_bytes()
            .ct_eq(callback_state.as_bytes()),
    ) {
        return Err(AdminAuthError::InvalidCredential);
    }
    let (client, http_client) = oidc_client(&state, &runtime.settings).await?;
    let token_response = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| AdminAuthError::InvalidCredential)?
        .set_pkce_verifier(PkceCodeVerifier::new(browser_state.pkce_verifier))
        .request_async(&http_client)
        .await
        .map_err(|_| AdminAuthError::InvalidCredential)?;
    let id_token = token_response
        .id_token()
        .ok_or(AdminAuthError::InvalidCredential)?;
    let verifier = client.id_token_verifier();
    let claims = id_token
        .claims(&verifier, &Nonce::new(browser_state.nonce))
        .map_err(|_| AdminAuthError::InvalidCredential)?;
    if claims.subject().as_str() != runtime.settings.owner_subject {
        return Err(AdminAuthError::InvalidCredential);
    }
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            token_response.access_token(),
            id_token
                .signing_alg()
                .map_err(|_| AdminAuthError::InvalidCredential)?,
            id_token
                .signing_key(&verifier)
                .map_err(|_| AdminAuthError::InvalidCredential)?,
        )
        .map_err(|_| AdminAuthError::InvalidCredential)?;
        if actual != *expected {
            return Err(AdminAuthError::InvalidCredential);
        }
    }
    let identity = VerifiedExternalIdentity {
        issuer: claims.issuer().as_str().to_owned(),
        subject: claims.subject().as_str().to_owned(),
        claims: Default::default(),
    };
    let subject_hash: [u8; 32] = Sha256::digest(identity.subject.as_bytes()).into();
    let repository = Arc::clone(&state.repository);
    let adapter = runtime.settings.adapter.clone();
    let issuer = identity.issuer;
    let binding_fingerprint = state.admin_auth.binding_fingerprint();
    let setup_complete = tokio::task::spawn_blocking(move || {
        repository.bind_external_identity(
            &adapter,
            &issuer,
            &subject_hash,
            &binding_fingerprint,
        )?;
        repository
            .get_admin_control_plane()
            .map(|control| control.setup_complete)
    })
    .await
    .map_err(|_| AdminAuthError::Unavailable)?
    .map_err(|error| AdminAuthError::Internal(error.to_string()))?;
    tracing::info!(
        adapter = %runtime.settings.adapter,
        subject_fingerprint = %hex_prefix(&subject_hash),
        "verified external administrator identity"
    );
    let response = administrator_session(&state, runtime.session_days).await?;
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .cloned()
        .ok_or(AdminAuthError::Unavailable)?;
    let mut redirect = Redirect::to(&application_path(
        &state,
        if setup_complete {
            "studio"
        } else {
            "onboarding"
        },
    ))
    .into_response();
    redirect.headers_mut().insert(header::SET_COOKIE, cookie);
    redirect.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_oidc_state_cookie(&state))
            .map_err(|_| AdminAuthError::Unavailable)?,
    );
    redirect.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(redirect)
}

async fn oidc_client(
    state: &AppState,
    settings: &ExternalAdminSettings,
) -> Result<
    (
        CoreClient<
            openidconnect::EndpointSet,
            openidconnect::EndpointNotSet,
            openidconnect::EndpointNotSet,
            openidconnect::EndpointNotSet,
            openidconnect::EndpointMaybeSet,
            openidconnect::EndpointMaybeSet,
        >,
        reqwest::Client,
    ),
    AdminAuthError,
> {
    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(StdDuration::from_secs(10))
        .build()
        .map_err(|_| AdminAuthError::Unavailable)?;
    let metadata = CoreProviderMetadata::discover_async(
        IssuerUrl::new(settings.issuer_url.to_string()).map_err(|_| AdminAuthError::Unavailable)?,
        &http_client,
    )
    .await
    .map_err(|error| {
        tracing::warn!(%error, "external identity provider discovery failed");
        AdminAuthError::Unavailable
    })?;
    let redirect_url = absolute_app_url(state, "api/v1/auth/external/callback")?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(settings.client_id.clone()),
        settings.client_secret.clone().map(ClientSecret::new),
    )
    .set_redirect_uri(RedirectUrl::new(redirect_url).map_err(|_| AdminAuthError::Unavailable)?);
    Ok((client, http_client))
}

fn absolute_app_url(state: &AppState, relative: &str) -> Result<String, AdminAuthError> {
    let mut base = state.seo_policy.public_url.clone();
    let path = format!("{}/", base.path().trim_end_matches('/'));
    base.set_path(&path);
    base.set_query(None);
    base.set_fragment(None);
    base.join(relative)
        .map(|url| url.to_string())
        .map_err(|_| AdminAuthError::Unavailable)
}

fn application_path(state: &AppState, page: &str) -> String {
    let base = state.seo_policy.public_url.path().trim_end_matches('/');
    format!("{base}/{page}")
}

fn unix_time() -> Result<u64, AdminAuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| AdminAuthError::Unavailable)
}

fn encode_oidc_browser_state(
    signing_key: &[u8; 32],
    binding_fingerprint: &[u8; 32],
    browser_state: &OidcBrowserState,
) -> Result<String, AdminAuthError> {
    let payload = serde_json::to_vec(browser_state)
        .map_err(|error| AdminAuthError::Internal(error.to_string()))?;
    if payload.len() > 2_048 {
        return Err(AdminAuthError::Unavailable);
    }
    let encoded = URL_SAFE_NO_PAD.encode(payload);
    let signature = oidc_state_signature(signing_key, binding_fingerprint, encoded.as_bytes());
    Ok(format!("{encoded}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

fn decode_oidc_browser_state(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<OidcBrowserState, AdminAuthError> {
    let signed = request_cookie(headers, OIDC_STATE_COOKIE)
        .filter(|value| value.len() <= 4_096)
        .ok_or(AdminAuthError::InvalidCredential)?;
    let binding_fingerprint = state.admin_auth.binding_fingerprint();
    decode_oidc_browser_state_at(
        state.cache_signing_key.as_ref(),
        &binding_fingerprint,
        &signed,
        unix_time()?,
    )
}

fn decode_oidc_browser_state_at(
    signing_key: &[u8; 32],
    binding_fingerprint: &[u8; 32],
    signed: &str,
    now: u64,
) -> Result<OidcBrowserState, AdminAuthError> {
    let (encoded, encoded_signature) = signed
        .split_once('.')
        .filter(|(_, signature)| !signature.contains('.'))
        .ok_or(AdminAuthError::InvalidCredential)?;
    let provided: [u8; 32] = URL_SAFE_NO_PAD
        .decode(encoded_signature)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(AdminAuthError::InvalidCredential)?;
    let expected = oidc_state_signature(signing_key, binding_fingerprint, encoded.as_bytes());
    if !bool::from(provided.ct_eq(&expected)) {
        return Err(AdminAuthError::InvalidCredential);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| AdminAuthError::InvalidCredential)?;
    if payload.len() > 2_048 {
        return Err(AdminAuthError::InvalidCredential);
    }
    let browser_state: OidcBrowserState =
        serde_json::from_slice(&payload).map_err(|_| AdminAuthError::InvalidCredential)?;
    let bounded = (1..=1_024).contains(&browser_state.state.len())
        && (1..=512).contains(&browser_state.nonce.len())
        && (43..=128).contains(&browser_state.pkce_verifier.len())
        && [
            browser_state.state.as_str(),
            browser_state.nonce.as_str(),
            browser_state.pkce_verifier.as_str(),
        ]
        .iter()
        .all(|value| !value.chars().any(char::is_control));
    let fresh = browser_state.issued_at_unix <= now.saturating_add(30)
        && now.saturating_sub(browser_state.issued_at_unix) <= OIDC_STATE_LIFETIME.as_secs();
    if !bounded || !fresh {
        return Err(AdminAuthError::InvalidCredential);
    }
    Ok(browser_state)
}

fn request_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(candidate, value)| (candidate == name).then(|| value.to_owned()))
}

fn oidc_cookie_path(state: &AppState) -> String {
    let base = state.seo_policy.public_url.path().trim_end_matches('/');
    format!("{base}/api/v1/auth/external/callback")
}

fn oidc_state_cookie(state: &AppState, value: &str) -> String {
    format!(
        "{OIDC_STATE_COOKIE}={value}; HttpOnly; SameSite=Lax; Path={}; Max-Age={}{}",
        oidc_cookie_path(state),
        OIDC_STATE_LIFETIME.as_secs(),
        if state.secure_session_cookie {
            "; Secure"
        } else {
            ""
        }
    )
}

fn clear_oidc_state_cookie(state: &AppState) -> String {
    format!(
        "{OIDC_STATE_COOKIE}=; HttpOnly; SameSite=Lax; Path={}; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT{}",
        oidc_cookie_path(state),
        if state.secure_session_cookie {
            "; Secure"
        } else {
            ""
        }
    )
}

fn oidc_state_signature(
    key: &[u8; 32],
    binding_fingerprint: &[u8; 32],
    payload: &[u8],
) -> [u8; 32] {
    let mut message = Vec::with_capacity(80 + payload.len());
    message.extend_from_slice(b"open-soverign-blog/oidc-browser-state/v1\0");
    message.extend_from_slice(binding_fingerprint);
    message.extend_from_slice(payload);
    hmac_sha256(key, &message)
}

fn hmac_sha256(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut inner_pad = [0x36_u8; BLOCK_SIZE];
    let mut outer_pad = [0x5c_u8; BLOCK_SIZE];
    for (index, byte) in key.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn verify_origin(state: &AppState, headers: &HeaderMap) -> Result<(), AdminAuthError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let origin = origin
        .to_str()
        .map_err(|_| AdminAuthError::InvalidCredential)?;
    let expected = state.seo_policy.public_url.origin().ascii_serialization();
    if origin == expected {
        Ok(())
    } else {
        Err(AdminAuthError::InvalidCredential)
    }
}

pub(super) fn request_origin_is_valid(state: &AppState, headers: &HeaderMap) -> bool {
    verify_origin(state, headers).is_ok()
}

fn hex_prefix(hash: &[u8; 32]) -> String {
    hash[..6].iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_digest(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug)]
enum AdminAuthError {
    InvalidCredential,
    RateLimited,
    Unavailable,
    NotFound,
    Internal(String),
}

impl IntoResponse for AdminAuthError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::InvalidCredential => (
                StatusCode::UNAUTHORIZED,
                "invalid_admin_auth",
                "administrator authentication failed",
            ),
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "admin_auth_rate_limited",
                "try again later",
            ),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "admin_auth_unavailable",
                "administrator authentication is temporarily unavailable",
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "the requested resource was not found",
            ),
            Self::Internal(error) => {
                tracing::error!(%error, "administrator authentication failed internally");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "the request could not be completed",
                )
            }
        };
        let mut response = (
            status,
            Json(AuthErrorBody {
                error: code,
                message,
            }),
        )
            .into_response();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
        response
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthErrorBody {
    error: &'static str,
    message: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::password_hash::{PasswordHasher, SaltString};
    use rand_core::OsRng;

    fn browser_state(issued_at_unix: u64) -> OidcBrowserState {
        OidcBrowserState {
            state: "csrf-state".into(),
            nonce: "oidc-nonce".into(),
            pkce_verifier: "v".repeat(43),
            issued_at_unix,
        }
    }

    #[test]
    fn oidc_browser_state_rejects_tampering_and_auth_module_rotation() {
        let signing_key = [0x5a; 32];
        let binding = [0x11; 32];
        let now = 1_900_000_000;
        let signed =
            encode_oidc_browser_state(&signing_key, &binding, &browser_state(now)).unwrap();
        let decoded = decode_oidc_browser_state_at(&signing_key, &binding, &signed, now).unwrap();
        assert_eq!(decoded.state, "csrf-state");

        let mut tampered = signed.clone().into_bytes();
        tampered[0] = if tampered[0] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(matches!(
            decode_oidc_browser_state_at(&signing_key, &binding, &tampered, now),
            Err(AdminAuthError::InvalidCredential)
        ));
        assert!(matches!(
            decode_oidc_browser_state_at(&signing_key, &[0x22; 32], &signed, now),
            Err(AdminAuthError::InvalidCredential)
        ));
    }

    #[test]
    fn oidc_browser_state_expires_and_rejects_far_future_tokens() {
        let signing_key = [0x5a; 32];
        let binding = [0x11; 32];
        let now = 1_900_000_000;
        let expired = encode_oidc_browser_state(
            &signing_key,
            &binding,
            &browser_state(now - OIDC_STATE_LIFETIME.as_secs() - 1),
        )
        .unwrap();
        assert!(matches!(
            decode_oidc_browser_state_at(&signing_key, &binding, &expired, now),
            Err(AdminAuthError::InvalidCredential)
        ));

        let future =
            encode_oidc_browser_state(&signing_key, &binding, &browser_state(now + 31)).unwrap();
        assert!(matches!(
            decode_oidc_browser_state_at(&signing_key, &binding, &future, now),
            Err(AdminAuthError::InvalidCredential)
        ));
    }

    #[tokio::test]
    async fn access_key_rate_limit_is_isolated_by_candidate_digest() {
        let salt = SaltString::generate(&mut OsRng);
        let phc = Argon2::default()
            .hash_password(b"valid-test-access-key", &salt)
            .unwrap()
            .to_string();
        let settings = AdminAuthSettings {
            mode: AdminAuthMode::AccessKey,
            access_key_phc: Some(phc),
            external: None,
            session_days: 30,
        };
        let AdminAuthRuntime::AccessKey(runtime) =
            AdminAuthRuntime::from_settings(&settings).unwrap()
        else {
            panic!("access-key settings must create an access-key runtime");
        };
        let noisy_candidate = [0x11; 32];
        for _ in 0..ACCESS_KEY_ATTEMPTS_PER_CREDENTIAL_PER_MINUTE {
            rate_limit_access_key(&runtime, noisy_candidate)
                .await
                .unwrap();
        }
        assert!(matches!(
            rate_limit_access_key(&runtime, noisy_candidate).await,
            Err(AdminAuthError::RateLimited)
        ));
        rate_limit_access_key(&runtime, [0x22; 32])
            .await
            .expect("one candidate must not lock out a different credential");
    }
}
