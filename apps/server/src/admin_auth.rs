//! Modular instance-administrator authentication.
//!
//! Human credentials terminate here. Both the one-time access-key exchange and
//! an external OIDC login issue the same opaque HttpOnly browser session used by
//! Studio; neither credential is forwarded to content handlers.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration as StdDuration, Instant},
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
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    reqwest,
};
use osb_feature_external_auth::VerifiedExternalIdentity;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::{
    AppState, community,
    config::{AdminAuthMode, AdminAuthSettings, ExternalAdminSettings},
};

const ACCESS_KEY_BODY_LIMIT: usize = 4 * 1024;
const ACCESS_KEY_ATTEMPTS_PER_MINUTE: usize = 12;
const OIDC_PENDING_LIFETIME: StdDuration = StdDuration::from_secs(10 * 60);
const OIDC_PENDING_LIMIT: usize = 64;

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
                    attempts: Arc::new(Mutex::new(VecDeque::new())),
                })))
            }
            AdminAuthMode::External => Ok(Self::External(Arc::new(ExternalRuntime {
                settings: settings
                    .external
                    .clone()
                    .context("external administrator settings are missing")?,
                session_days: settings.session_days,
                pending: Arc::new(Mutex::new(HashMap::new())),
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
    attempts: Arc<Mutex<VecDeque<Instant>>>,
}

pub(super) struct ExternalRuntime {
    settings: ExternalAdminSettings,
    session_days: i64,
    pending: Arc<Mutex<HashMap<String, PendingOidc>>>,
}

struct PendingOidc {
    nonce: Nonce,
    pkce_verifier: PkceCodeVerifier,
    created_at: Instant,
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
    rate_limit_access_key(runtime).await?;
    if let Some(cache) = &state.cache {
        let admitted = cache
            .admit_fixed_window(
                "administrator-access-key",
                ACCESS_KEY_ATTEMPTS_PER_MINUTE as u64,
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
    let candidate = access_key.as_bytes().to_vec();
    let permit = state
        .password_workers
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| AdminAuthError::Unavailable)?;
    let verified = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        PasswordHash::new(&phc).ok().is_some_and(|parsed| {
            Argon2::default()
                .verify_password(&candidate, &parsed)
                .is_ok()
        })
    })
    .await
    .map_err(|_| AdminAuthError::Unavailable)?;
    if !verified {
        return Err(AdminAuthError::InvalidCredential);
    }
    runtime.attempts.lock().await.clear();
    administrator_session(state, runtime.session_days).await
}

async fn rate_limit_access_key(runtime: &AccessKeyRuntime) -> Result<(), AdminAuthError> {
    let now = Instant::now();
    let mut attempts = runtime.attempts.lock().await;
    while attempts
        .front()
        .is_some_and(|attempt| now.duration_since(*attempt) >= StdDuration::from_secs(60))
    {
        attempts.pop_front();
    }
    if attempts.len() >= ACCESS_KEY_ATTEMPTS_PER_MINUTE {
        return Err(AdminAuthError::RateLimited);
    }
    attempts.push_back(now);
    Ok(())
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

async fn start_external(State(state): State<AppState>) -> Result<Redirect, AdminAuthError> {
    let AdminAuthRuntime::External(runtime) = &state.admin_auth else {
        return Err(AdminAuthError::NotFound);
    };
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
    let mut pending = runtime.pending.lock().await;
    prune_pending(&mut pending);
    if pending.len() >= OIDC_PENDING_LIMIT {
        return Err(AdminAuthError::RateLimited);
    }
    pending.insert(
        csrf.secret().clone(),
        PendingOidc {
            nonce,
            pkce_verifier: verifier,
            created_at: Instant::now(),
        },
    );
    Ok(Redirect::to(url.as_str()))
}

#[derive(Deserialize)]
struct OidcCallback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn finish_external(
    State(state): State<AppState>,
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
    let pending = {
        let mut entries = runtime.pending.lock().await;
        prune_pending(&mut entries);
        entries.remove(&callback_state)
    }
    .ok_or(AdminAuthError::InvalidCredential)?;
    let (client, http_client) = oidc_client(&state, &runtime.settings).await?;
    let token_response = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| AdminAuthError::InvalidCredential)?
        .set_pkce_verifier(pending.pkce_verifier)
        .request_async(&http_client)
        .await
        .map_err(|_| AdminAuthError::InvalidCredential)?;
    let id_token = token_response
        .id_token()
        .ok_or(AdminAuthError::InvalidCredential)?;
    let verifier = client.id_token_verifier();
    let claims = id_token
        .claims(&verifier, &pending.nonce)
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

fn prune_pending(entries: &mut HashMap<String, PendingOidc>) {
    entries.retain(|_, value| value.created_at.elapsed() < OIDC_PENDING_LIFETIME);
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
