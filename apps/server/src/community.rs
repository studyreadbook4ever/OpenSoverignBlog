use std::sync::{Arc, OnceLock};

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use osb_assets_fs::{AssetError, AssetRecord};
use osb_feature_comments::{
    Comment as ValidatedComment, CommentStatus as ValidatedCommentStatus, CommentSubmission,
};
use osb_kernel::{
    CONTENT_SCHEMA_VERSION, ContentRepository, DocumentSnapshot, EmbedReference, IntentLayer,
    NewDocument, OntologySidecar, ProposedRevision, RepositoryError, RevisionActor,
    RevisionActorKind, RevisionSnapshot, content_hash,
};
use osb_renderer::{PublishArtifact, ViewMode, render_revision, summarize_markdown};
use osb_storage_sqlite::{
    CommentRecord, SessionAuthMethod, SiteMembershipRecord, SiteMembershipRole, SiteRecord,
    SqliteRepository, ThemeProfile, UserRecord,
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{AppState, ViewQuery, begin_public_mutation};

const SESSION_COOKIE: &str = "osb_session";
const SESSION_LIFETIME_DAYS: i64 = 30;
const PUBLIC_CACHE: &str = "public, max-age=0, s-maxage=60, stale-while-revalidate=300";

pub fn routes(state: AppState) -> Router<AppState> {
    let mut public = Router::new()
        .route("/api/v1/feed", get(feed))
        .route("/api/v1/blogs", get(list_blogs))
        .route("/api/v1/blogs/{handle}", get(get_blog))
        .route("/api/v1/blogs/{handle}/posts", get(list_blog_posts))
        .route("/api/v1/blogs/{handle}/posts/{slug}", get(get_blog_post));
    if state.custom_css_enabled {
        public = public.route(
            "/api/v1/blogs/{handle}/custom.css",
            get(get_blog_custom_css),
        );
    }
    if state.comments_enabled {
        public = public.route("/api/v1/posts/{id}/comments", get(list_comments));
    }

    let mut private_reads = Router::new()
        .route("/api/v1/session", get(session))
        .route("/api/v1/studio/documents", get(list_studio_documents))
        .route("/api/v1/studio/documents/{id}", get(get_studio_document))
        .route("/api/v1/studio/settings", get(get_studio_settings));
    if state.collaboration_enabled {
        private_reads = private_reads.route(
            "/api/v1/studio/collaborators",
            get(list_studio_collaborators),
        );
    }
    let private_reads = private_reads.route_layer(middleware::from_fn(private_no_store));

    let mut mutations = Router::new()
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/blogs", post(create_blog))
        .route("/api/v1/studio/documents", post(create_studio_document))
        .route(
            "/api/v1/studio/documents/{id}/revisions",
            post(create_studio_revision),
        )
        .route(
            "/api/v1/studio/documents/{id}/publish",
            post(publish_studio_document),
        )
        .route("/api/v1/studio/preview", post(preview_studio))
        .route("/api/v1/studio/assets", post(upload_studio_asset))
        .route("/api/v1/studio/settings", put(update_studio_settings));
    if state.local_auth_enabled {
        mutations = mutations
            .route("/api/v1/auth/register", post(register))
            .route("/api/v1/auth/login", post(login));
    }
    if state.comments_enabled {
        mutations = mutations.route("/api/v1/posts/{id}/comments", post(create_comment));
    }
    if state.collaboration_enabled {
        mutations = mutations
            .route(
                "/api/v1/studio/collaborators",
                post(add_studio_collaborator),
            )
            .route(
                "/api/v1/studio/collaborators/{userId}",
                delete(remove_studio_collaborator),
            );
    }
    let mutations = mutations
        // Delivery-only rejection happens before body buffering/JSON parsing.
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            delivery_guard,
        ))
        .route_layer(middleware::from_fn_with_state(state, origin_guard))
        .route_layer(middleware::from_fn(private_no_store));

    public.merge(private_reads).merge(mutations)
}

async fn origin_guard(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !super::admin_auth::request_origin_is_valid(&state, request.headers()) {
        CommunityApiError::Unauthorized.into_response()
    } else {
        next.run(request).await
    }
}

async fn delivery_guard(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if state.delivery_only {
        CommunityApiError::ReadOnly.into_response()
    } else {
        next.run(request).await
    }
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

async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, CommunityApiError> {
    let user = resolve_session_user(&state, &headers).await?;
    let payload = session_payload(&state, user).await?;
    Ok(Json(payload).into_response())
}

async fn register(
    State(state): State<AppState>,
    Json(input): Json<RegisterInput>,
) -> Result<Response, CommunityApiError> {
    ensure_mutable(&state)?;
    if !state.registration_open {
        return Err(CommunityApiError::RegistrationClosed);
    }
    let email = validate_email(&input.email)?;
    validate_handle(&input.handle, "user handle")?;
    let display_name = validate_text(&input.display_name, "display name", 80)?;
    validate_password(&input.password)?;
    let password_phc = hash_password(Arc::clone(&state.password_workers), input.password).await?;
    let repository = Arc::clone(&state.repository);
    let handle = input.handle;
    let user = repository_task(move || {
        repository.create_user(&email, &handle, &display_name, &password_phc)
    })
    .await?;
    authenticated_response(&state, user, StatusCode::CREATED).await
}

async fn login(
    State(state): State<AppState>,
    Json(input): Json<LoginInput>,
) -> Result<Response, CommunityApiError> {
    ensure_mutable(&state)?;
    let email = validate_email(&input.email)?;
    validate_password_for_login(&input.password)?;
    let repository = Arc::clone(&state.repository);
    let candidate = repository_optional(move || repository.find_user_by_email(&email)).await?;
    let expected = candidate.as_ref().map(|user| user.password_phc.clone());
    let verified = verify_password(
        Arc::clone(&state.password_workers),
        input.password,
        expected,
    )
    .await?;
    let user = candidate
        .filter(|_| verified)
        .ok_or(CommunityApiError::InvalidLogin)?;
    authenticated_response(&state, user, StatusCode::OK).await
}

async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, CommunityApiError> {
    ensure_mutable(&state)?;
    if let Some(hash) = session_hash_from_headers(&headers) {
        let repository = Arc::clone(&state.repository);
        repository_task(move || repository.revoke_session(&hash).map(|_| ())).await?;
    }
    let payload = SessionPayload::Anonymous {
        registration_open: state.registration_open,
    };
    let mut response = Json(payload).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_session_cookie(
            state.secure_session_cookie,
            &session_cookie_path(&state),
        ))
        .map_err(internal_error)?,
    );
    Ok(response)
}

pub(super) async fn authenticated_response(
    state: &AppState,
    user: UserRecord,
    status: StatusCode,
) -> Result<Response, CommunityApiError> {
    let mut raw_token = [0_u8; 32];
    OsRng.fill_bytes(&mut raw_token);
    let token_hash: [u8; 32] = Sha256::digest(raw_token).into();
    let token = URL_SAFE_NO_PAD.encode(raw_token);
    let expires_at = Utc::now() + Duration::days(SESSION_LIFETIME_DAYS);
    let repository = Arc::clone(&state.repository);
    let user_id = user.id;
    repository_task(move || {
        repository
            .create_session(user_id, &token_hash, expires_at)
            .map(|_| ())
    })
    .await?;
    let payload = session_payload(state, Some(user)).await?;
    let mut response = (status, Json(payload)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie(
            &token,
            state.secure_session_cookie,
            &session_cookie_path(state),
            SESSION_LIFETIME_DAYS * 24 * 60 * 60,
        ))
        .map_err(internal_error)?,
    );
    Ok(response)
}

pub(super) async fn administrator_authenticated_response(
    state: &AppState,
    user: UserRecord,
    status: StatusCode,
    auth_method: SessionAuthMethod,
    session_days: i64,
) -> Result<Response, CommunityApiError> {
    let mut raw_token = [0_u8; 32];
    OsRng.fill_bytes(&mut raw_token);
    let token_hash: [u8; 32] = Sha256::digest(raw_token).into();
    let token = URL_SAFE_NO_PAD.encode(raw_token);
    let expires_at = Utc::now() + Duration::days(session_days);
    let repository = Arc::clone(&state.repository);
    let binding_fingerprint = state.admin_auth.binding_fingerprint();
    repository_task(move || {
        repository
            .create_primary_owner_session(
                &token_hash,
                expires_at,
                auth_method,
                &binding_fingerprint,
            )
            .map(|_| ())
    })
    .await?;
    let payload = session_payload(state, Some(user)).await?;
    let mut response = (status, Json(payload)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie(
            &token,
            state.secure_session_cookie,
            &session_cookie_path(state),
            session_days * 24 * 60 * 60,
        ))
        .map_err(internal_error)?,
    );
    Ok(response)
}

async fn session_payload(
    state: &AppState,
    user: Option<UserRecord>,
) -> Result<SessionPayload, CommunityApiError> {
    if state.delivery_only || user.is_none() {
        return Ok(SessionPayload::Anonymous {
            registration_open: state.registration_open && !state.delivery_only,
        });
    }
    let user = user.expect("checked above");
    let repository = Arc::clone(&state.repository);
    let user_id = user.id;
    let collaboration_enabled = state.collaboration_enabled;
    let (blog, membership_role) = repository_task(move || {
        match repository.get_admin_control_plane() {
            Ok(control) if control.owner_user_id == user_id && !control.setup_complete => {
                return Ok((None, None));
            }
            Ok(_) | Err(RepositoryError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let sites = if collaboration_enabled {
            repository.list_accessible_sites(user_id, 1)?
        } else {
            repository.list_owned_sites(user_id, 1)?
        };
        let Some(site) = sites.into_iter().next() else {
            return Ok((None, None));
        };
        let membership = repository.get_site_membership(user_id, site.id)?;
        let owner = repository.get_user_by_id(site.owner_user_id)?;
        Ok((Some(blog_summary(site, owner)), Some(membership.role)))
    })
    .await?;
    Ok(SessionPayload::Authenticated {
        session: Box::new(AuthenticatedSession {
            registration_open: state.registration_open,
            user: user_summary(user),
            blog,
            membership_role,
        }),
    })
}

async fn resolve_session_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<UserRecord>, CommunityApiError> {
    if state.delivery_only {
        return Ok(None);
    }
    let Some(token_hash) = session_hash_from_headers(headers) else {
        return Ok(None);
    };
    let repository = Arc::clone(&state.repository);
    let local_auth_enabled = state.local_auth_enabled;
    let admin_auth_mode = state.admin_auth.mode();
    repository_optional(move || {
        let session = repository.get_session(&token_hash)?;
        let enabled = match session.auth_method {
            SessionAuthMethod::Legacy => local_auth_enabled,
            SessionAuthMethod::AccessKey => {
                admin_auth_mode == super::config::AdminAuthMode::AccessKey
            }
            SessionAuthMethod::External => {
                admin_auth_mode == super::config::AdminAuthMode::External
            }
        };
        if !enabled {
            return Err(RepositoryError::NotFound);
        }
        repository.get_user_by_id(session.user_id)
    })
    .await
}

async fn require_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<UserRecord, CommunityApiError> {
    ensure_mutable(state)?;
    resolve_session_user(state, headers)
        .await?
        .ok_or(CommunityApiError::Unauthorized)
}

async fn list_blogs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, CommunityApiError> {
    let repository = Arc::clone(&state.repository);
    let blogs = repository_task(move || {
        repository
            .list_sites(500)?
            .into_iter()
            .map(|site| {
                let owner = repository.get_user_by_id(site.owner_user_id)?;
                Ok(blog_summary(site, owner))
            })
            .collect::<Result<Vec<_>, RepositoryError>>()
    })
    .await?;
    public_json(&headers, &blogs)
}

async fn get_blog(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(handle): Path<String>,
) -> Result<Response, CommunityApiError> {
    validate_handle(&handle, "blog handle")?;
    let repository = Arc::clone(&state.repository);
    let custom_css_enabled = state.custom_css_enabled;
    let blog = repository_task(move || {
        let site = repository.get_site_by_handle(&handle)?;
        let owner = repository.get_user_by_id(site.owner_user_id)?;
        Ok(blog_summary_with_css(
            site,
            owner,
            custom_css_enabled,
            &state.seo_policy,
        ))
    })
    .await?;
    public_json(&headers, &blog)
}

async fn get_blog_custom_css(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(handle): Path<String>,
) -> Result<Response, CommunityApiError> {
    validate_handle(&handle, "blog handle")?;
    let repository = Arc::clone(&state.repository);
    let (site_id, theme_revision, custom_css) = repository_task(move || {
        let site = repository.get_site_by_handle(&handle)?;
        Ok((
            site.id,
            site.theme_revision,
            site.custom_css.unwrap_or_default(),
        ))
    })
    .await?;
    let stylesheet = scoped_site_stylesheet(site_id, &custom_css);
    public_bytes(
        &headers,
        stylesheet.into_bytes(),
        &theme_revision.to_be_bytes(),
        HeaderValue::from_static("text/css; charset=utf-8"),
    )
}

fn scoped_site_stylesheet(site_id: Uuid, custom_css: &str) -> String {
    if custom_css.is_empty() {
        return String::new();
    }
    // Storage rejects user at-rules, escapes, and structurally unbalanced
    // blocks. This generated wrapper therefore cannot be closed by owner input.
    // Serving it from a same-origin URL satisfies the default CSP.
    format!("@scope (.osb-site-theme[data-site-id=\"{site_id}\"]) {{\n{custom_css}\n}}\n")
}

async fn create_blog(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateBlogInput>,
) -> Result<Response, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    validate_handle(&input.handle, "blog handle")?;
    let title = validate_text(&input.title, "blog title", 100)?;
    let description = input
        .description
        .as_deref()
        .map(|value| validate_text(value, "blog description", 240))
        .transpose()?;
    let _cache_mutation = begin_public_mutation(&state);
    let repository = Arc::clone(&state.repository);
    let user_id = user.id;
    let handle = input.handle;
    let site = repository_task(move || {
        match repository.get_admin_control_plane() {
            Ok(control) if control.owner_user_id == user_id && !control.setup_complete => {
                return repository.complete_primary_owner_setup(
                    user_id,
                    &handle,
                    &title,
                    description.as_deref(),
                    input.theme_preset,
                );
            }
            Ok(_) | Err(RepositoryError::NotFound) => {}
            Err(error) => return Err(error),
        }
        if !repository.list_owned_sites(user_id, 1)?.is_empty() {
            return Err(RepositoryError::Validation(
                "an account can own one blog in this deployment".into(),
            ));
        }
        repository.create_site(
            user_id,
            &handle,
            &title,
            description.as_deref(),
            input.theme_preset,
        )
    })
    .await?;
    Ok((StatusCode::CREATED, Json(blog_summary(site, user))).into_response())
}

async fn feed(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, CommunityApiError> {
    let repository = Arc::clone(&state.repository);
    let items = repository_task(move || {
        repository
            .list_published_across_sites(100)?
            .into_iter()
            .map(|document| feed_item(&repository, document))
            .collect::<Result<Vec<_>, RepositoryError>>()
    })
    .await?;
    public_json(&headers, &FeedResponse { items })
}

async fn list_blog_posts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(handle): Path<String>,
) -> Result<Response, CommunityApiError> {
    validate_handle(&handle, "blog handle")?;
    let repository = Arc::clone(&state.repository);
    let items = repository_task(move || {
        let site = repository.get_site_by_handle(&handle)?;
        repository
            .list_published(site.id, 500)?
            .into_iter()
            .map(|document| feed_item(&repository, document))
            .collect::<Result<Vec<_>, RepositoryError>>()
    })
    .await?;
    public_json(&headers, &FeedResponse { items })
}

fn feed_item(
    repository: &SqliteRepository,
    document: DocumentSnapshot,
) -> Result<FeedPostSummary, RepositoryError> {
    let site = repository.get_site_by_id(document.site_id)?;
    let owner = repository.get_user_by_id(site.owner_user_id)?;
    let comment_count = repository.count_approved_comments(site.id, document.id)?;
    let published_at = document.revision.created_at;
    Ok(FeedPostSummary {
        id: document.id,
        title: document.revision.title.clone(),
        slug: document.revision.slug.clone(),
        excerpt: summarize_markdown(&document.revision.source_markdown, 220),
        published_at,
        // Draft timestamps are intentionally excluded from the public wire and
        // its ETag. A new draft cannot perturb the currently published object.
        updated_at: published_at,
        author: user_summary(owner.clone()),
        blog: blog_summary(site, owner),
        tags: Vec::new(),
        comment_count,
        has_intent_view: document.revision.intent.is_some(),
        cover_image_url: None,
    })
}

async fn get_blog_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((handle, slug)): Path<(String, String)>,
    Query(query): Query<ViewQuery>,
) -> Result<Response, CommunityApiError> {
    validate_handle(&handle, "blog handle")?;
    let view = query.view.unwrap_or(ViewMode::Intent);
    let repository = Arc::clone(&state.repository);
    let lookup_slug = slug.clone();
    let (document, site, owner) = repository_task(move || {
        let site = repository.get_site_by_handle(&handle)?;
        let owner = repository.get_user_by_id(site.owner_user_id)?;
        let document = repository.get_published_by_slug(site.id, &lookup_slug)?;
        Ok((document, site, owner))
    })
    .await?;
    let artifact = render_revision(&document.revision, view);
    let published_at = document.revision.created_at;
    let etag_seed = (
        document.revision.id,
        site.theme_revision,
        artifact.artifact_hash.clone(),
    );
    let response = BlogPostView {
        id: document.id,
        title: document.revision.title.clone(),
        canonical_slug: document.revision.slug.clone(),
        requested_slug: slug,
        revision_id: document.revision.id,
        markdown: document.revision.source_markdown.clone(),
        embeds: document.revision.embeds.clone(),
        artifact,
        ontology: document.revision.ontology.clone(),
        slug: document.revision.slug,
        excerpt: Some(summarize_markdown(&document.revision.source_markdown, 220)),
        published_at,
        updated_at: published_at,
        author: user_summary(owner.clone()),
        blog: blog_summary_with_css(site, owner, state.custom_css_enabled, &state.seo_policy),
        tags: Vec::new(),
        cover_image_url: None,
    };
    // The article cache key follows immutable publication/theme artifacts.
    // Comment activity is fetched and cached independently.
    public_json_with_seed(&headers, &response, &etag_seed)
}

async fn list_studio_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<DocumentSnapshot>>, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = studio_access(&state, user.id).await?;
    let repository = Arc::clone(&state.repository);
    Ok(Json(
        repository_task(move || {
            repository.list_documents_in_writable_site(user.id, access.site.id, 500)
        })
        .await?,
    ))
}

async fn get_studio_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(document_id): Path<Uuid>,
) -> Result<Json<DocumentSnapshot>, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = studio_access(&state, user.id).await?;
    let repository = Arc::clone(&state.repository);
    Ok(Json(
        repository_task(move || {
            repository.get_document_in_writable_site(user.id, access.site.id, document_id)
        })
        .await?,
    ))
}

async fn create_studio_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<StudioDocumentInput>,
) -> Result<Response, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = studio_access(&state, user.id).await?;
    let document = new_document(access.site.id, &user, input);
    let repository = Arc::clone(&state.repository);
    let actor_id = user.id;
    let document =
        repository_task(move || repository.create_document_in_writable_site(actor_id, document))
            .await?;
    Ok((StatusCode::CREATED, Json(document)).into_response())
}

async fn create_studio_revision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(document_id): Path<Uuid>,
    Json(input): Json<StudioRevisionInput>,
) -> Result<Response, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = studio_access(&state, user.id).await?;
    let proposal = ProposedRevision {
        document_id,
        base_revision_id: input.base_revision_id,
        title: input.title,
        slug: input.slug,
        source_markdown: input.source_markdown,
        embeds: input.embeds,
        intent: input.intent,
        ontology: input.ontology,
        actor: revision_actor(&user),
        idempotency_key: input.idempotency_key,
    };
    let repository = Arc::clone(&state.repository);
    let actor_id = user.id;
    let document = repository_task(move || {
        repository.revise_document_in_writable_site(actor_id, access.site.id, proposal)?;
        repository.get_document_in_writable_site(actor_id, access.site.id, document_id)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(document)).into_response())
}

async fn publish_studio_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(document_id): Path<Uuid>,
    Json(input): Json<PublishInput>,
) -> Result<Json<DocumentSnapshot>, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = owner_studio_access(&state, user.id).await?;
    let _cache_mutation = begin_public_mutation(&state);
    let repository = Arc::clone(&state.repository);
    Ok(Json(
        repository_task(move || {
            repository.publish_document_in_owned_site(
                user.id,
                access.site.id,
                document_id,
                input.revision_id,
            )
        })
        .await?,
    ))
}

async fn preview_studio(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<StudioDocumentInput>,
) -> Result<Json<PreviewResponse>, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    // Requiring a writable site makes the preview boundary identical to every
    // other Studio handler instead of becoming an unscoped rendering oracle.
    let access = studio_access(&state, user.id).await?;
    let input = new_document(access.site.id, &user, input);
    input
        .validate()
        .map_err(|error| CommunityApiError::BadRequest(error.to_string()))?;
    let revision = RevisionSnapshot {
        schema_version: CONTENT_SCHEMA_VERSION.into(),
        id: Uuid::now_v7(),
        document_id: Uuid::now_v7(),
        revision_number: 1,
        parent_revision_id: None,
        title: input.title,
        slug: input.slug,
        source_markdown: input.source_markdown,
        embeds: input.embeds,
        intent: input.intent,
        ontology: input.ontology,
        actor: input.actor,
        content_hash: String::new(),
        created_at: Utc::now(),
    };
    let mut revision = revision;
    revision.content_hash = content_hash(
        &revision.title,
        &revision.slug,
        &revision.source_markdown,
        &revision.embeds,
        revision.intent.as_ref(),
        revision.ontology.as_ref(),
    );
    Ok(Json(PreviewResponse {
        artifact: render_revision(&revision, ViewMode::Intent),
    }))
}

async fn upload_studio_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    studio_access(&state, user.id).await?;
    let filename = headers
        .get("x-osb-filename")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("asset")
        .to_owned();
    let claimed_media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let assets = Arc::clone(&state.assets);
    let record = tokio::task::spawn_blocking(move || {
        assets.put(&body, &filename, claimed_media_type.as_deref())
    })
    .await
    .map_err(|error| CommunityApiError::Internal(format!("asset worker failed: {error}")))?
    .map_err(map_asset_error)?;
    let url = state
        .seo_policy
        .public_route_url(&format!("/media/{}", record.digest))
        .map_err(|error| {
            CommunityApiError::Internal(format!("public asset URL is invalid: {error}"))
        })?
        .to_string();
    Ok((
        StatusCode::CREATED,
        Json(StudioAssetUploadResponse { record, url }),
    )
        .into_response())
}

async fn get_studio_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StudioSettings>, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = owner_studio_access(&state, user.id).await?;
    Ok(Json(studio_settings(access.site, state.custom_css_enabled)))
}

async fn update_studio_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<UpdateStudioSettingsInput>,
) -> Result<Json<StudioSettings>, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = owner_studio_access(&state, user.id).await?;
    if !state.custom_css_enabled && input.custom_css.provided {
        return Err(CommunityApiError::BadRequest(
            "custom CSS is disabled for this deployment".into(),
        ));
    }
    let _cache_mutation = begin_public_mutation(&state);
    let repository = Arc::clone(&state.repository);
    let site = if input.custom_css.provided {
        repository_task(move || {
            repository.change_site_appearance(
                user.id,
                access.site.id,
                input.theme_preset,
                input.custom_css.value.as_deref(),
            )
        })
        .await?
    } else {
        repository_task(move || {
            repository.change_site_theme(user.id, access.site.id, input.theme_preset)
        })
        .await?
    };
    Ok(Json(studio_settings(site, state.custom_css_enabled)))
}

async fn list_studio_collaborators(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CollaboratorListResponse>, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = owner_studio_access(&state, user.id).await?;
    let repository = Arc::clone(&state.repository);
    let items = repository_task(move || {
        repository
            .list_site_memberships(user.id, access.site.id, 500)?
            .into_iter()
            .filter(|membership| !membership.role.is_owner())
            .map(|membership| {
                let member = repository.get_user_by_id(membership.user_id)?;
                Ok(collaborator_view(membership, member))
            })
            .collect::<Result<Vec<_>, RepositoryError>>()
    })
    .await?;
    Ok(Json(CollaboratorListResponse { items }))
}

async fn add_studio_collaborator(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<AddCollaboratorInput>,
) -> Result<Response, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = owner_studio_access(&state, user.id).await?;
    let email = validate_email(&input.email)?;
    let repository = Arc::clone(&state.repository);
    let collaborator = repository_task(move || {
        let membership =
            repository.add_site_collaborator(user.id, access.site.id, &email, input.role)?;
        let member = repository.get_user_by_id(membership.user_id)?;
        Ok(collaborator_view(membership, member))
    })
    .await?;
    Ok((StatusCode::CREATED, Json(collaborator)).into_response())
}

async fn remove_studio_collaborator(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(collaborator_user_id): Path<Uuid>,
) -> Result<Json<CollaboratorView>, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let access = owner_studio_access(&state, user.id).await?;
    let repository = Arc::clone(&state.repository);
    Ok(Json(
        repository_task(move || {
            let membership = repository.remove_site_collaborator(
                user.id,
                access.site.id,
                collaborator_user_id,
            )?;
            let member = repository.get_user_by_id(membership.user_id)?;
            Ok(collaborator_view(membership, member))
        })
        .await?,
    ))
}

async fn list_comments(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(document_id): Path<Uuid>,
) -> Result<Response, CommunityApiError> {
    let repository = Arc::clone(&state.repository);
    let items = repository_task(move || {
        let document = repository.get_published_document_by_id(document_id)?;
        // Community sites are resolved explicitly; legacy single-site content
        // does not accidentally acquire a cross-model comment surface.
        repository.get_site_by_id(document.site_id)?;
        repository
            .list_approved_comments(document.site_id, document.id, 1_000)?
            .into_iter()
            .map(|comment| {
                let author = repository.get_user_by_id(comment.author_user_id)?;
                Ok(comment_view(comment, author, false))
            })
            .collect::<Result<Vec<_>, RepositoryError>>()
    })
    .await?;
    public_json(&headers, &CommentListResponse { items })
}

async fn create_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(document_id): Path<Uuid>,
    Json(input): Json<CreateCommentInput>,
) -> Result<Response, CommunityApiError> {
    let user = require_user(&state, &headers).await?;
    let _cache_mutation = begin_public_mutation(&state);
    let repository = Arc::clone(&state.repository);
    let source_markdown = input.source_markdown;
    let author = user.clone();
    let comment = repository_task(move || {
        let document = repository.get_published_document_by_id(document_id)?;
        repository.get_site_by_id(document.site_id)?;
        CommentSubmission {
            site_id: document.site_id,
            document_id: document.id,
            author_reference: user.id.to_string(),
            source_markdown: source_markdown.clone(),
        }
        .validate()
        .map_err(|error| RepositoryError::Validation(error.to_string()))?;
        repository.create_comment(user.id, document.site_id, document.id, &source_markdown)
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(comment_view(comment, author, true)),
    )
        .into_response())
}

#[derive(Debug)]
struct StudioAccess {
    site: SiteRecord,
    role: SiteMembershipRole,
}

async fn studio_access(state: &AppState, user_id: Uuid) -> Result<StudioAccess, CommunityApiError> {
    let repository = Arc::clone(&state.repository);
    let collaboration_enabled = state.collaboration_enabled;
    repository_task(move || {
        let sites = if collaboration_enabled {
            repository.list_accessible_sites(user_id, 1)?
        } else {
            repository.list_owned_sites(user_id, 1)?
        };
        let site = sites.into_iter().next().ok_or(RepositoryError::NotFound)?;
        let role = repository.get_site_membership(user_id, site.id)?.role;
        Ok(StudioAccess { site, role })
    })
    .await
    .map_err(|error| match error {
        CommunityApiError::NotFound => {
            CommunityApiError::BadRequest("create a blog before using Studio".into())
        }
        other => other,
    })
}

async fn owner_studio_access(
    state: &AppState,
    user_id: Uuid,
) -> Result<StudioAccess, CommunityApiError> {
    let access = studio_access(state, user_id).await?;
    if access.role.is_owner() {
        Ok(access)
    } else {
        Err(CommunityApiError::Forbidden(
            "only the blog owner can publish or manage settings and collaborators".into(),
        ))
    }
}

fn new_document(site_id: Uuid, user: &UserRecord, input: StudioDocumentInput) -> NewDocument {
    NewDocument {
        site_id,
        title: input.title,
        slug: input.slug,
        source_markdown: input.source_markdown,
        embeds: input.embeds,
        intent: input.intent,
        ontology: input.ontology,
        actor: revision_actor(user),
    }
}

fn revision_actor(user: &UserRecord) -> RevisionActor {
    RevisionActor {
        kind: RevisionActorKind::Human,
        id: user.id.to_string(),
        display_name: Some(user.display_name.clone()),
    }
}

fn comment_view(comment: CommentRecord, author: UserRecord, can_edit: bool) -> CommentView {
    let renderable = ValidatedComment {
        id: comment.id,
        site_id: comment.site_id,
        document_id: comment.document_id,
        author_reference: author.id.to_string(),
        source_markdown: comment.source_markdown.clone(),
        status: ValidatedCommentStatus::Approved,
        created_at: comment.created_at,
        updated_at: comment.updated_at,
    };
    CommentView {
        id: comment.id,
        post_id: comment.document_id,
        author: user_summary(author),
        source_markdown: comment.source_markdown,
        artifact_html: renderable.render_if_approved().unwrap_or_default(),
        created_at: comment.created_at,
        updated_at: comment.updated_at,
        can_edit,
        can_delete: can_edit,
    }
}

fn user_summary(user: UserRecord) -> UserSummary {
    UserSummary {
        id: user.id,
        handle: user.handle,
        display_name: user.display_name,
        avatar_url: None,
    }
}

fn blog_summary(site: SiteRecord, owner: UserRecord) -> BlogSummary {
    BlogSummary {
        id: site.id,
        handle: site.handle,
        title: site.title,
        description: site.description,
        owner: user_summary(owner),
        theme: BlogTheme {
            preset_id: site.theme_profile,
            custom_css_url: None,
        },
        created_at: Some(site.created_at),
    }
}

fn blog_summary_with_css(
    site: SiteRecord,
    owner: UserRecord,
    custom_css_enabled: bool,
    policy: &osb_feature_seo::SeoPolicy,
) -> BlogSummary {
    let custom_css_url = (custom_css_enabled && site.custom_css.is_some()).then(|| {
        policy
            .public_route_url(&format!("/api/v1/blogs/{}/custom.css", site.handle))
            .expect("validated policy and persisted handle form a safe public CSS URL")
            .to_string()
    });
    let mut summary = blog_summary(site, owner);
    summary.theme.custom_css_url = custom_css_url;
    summary
}

fn studio_settings(site: SiteRecord, custom_css_enabled: bool) -> StudioSettings {
    StudioSettings {
        blog_id: site.id,
        theme_preset: site.theme_profile,
        theme_revision: site.theme_revision,
        custom_css_enabled,
        custom_css: custom_css_enabled.then_some(site.custom_css).flatten(),
    }
}

fn collaborator_view(membership: SiteMembershipRecord, user: UserRecord) -> CollaboratorView {
    CollaboratorView {
        user_id: user.id,
        email: user.email,
        handle: user.handle,
        display_name: user.display_name,
        role: membership.role,
        created_at: membership.created_at,
    }
}

fn public_json<T: Serialize>(
    request_headers: &HeaderMap,
    value: &T,
) -> Result<Response, CommunityApiError> {
    let bytes = serde_json::to_vec(value).map_err(internal_error)?;
    public_json_bytes(request_headers, bytes.clone(), &bytes)
}

fn public_json_with_seed<T: Serialize, S: Serialize>(
    request_headers: &HeaderMap,
    value: &T,
    etag_seed: &S,
) -> Result<Response, CommunityApiError> {
    let bytes = serde_json::to_vec(value).map_err(internal_error)?;
    let seed = serde_json::to_vec(etag_seed).map_err(internal_error)?;
    public_json_bytes(request_headers, bytes, &seed)
}

fn public_json_bytes(
    request_headers: &HeaderMap,
    bytes: Vec<u8>,
    etag_seed: &[u8],
) -> Result<Response, CommunityApiError> {
    public_bytes(
        request_headers,
        bytes,
        etag_seed,
        HeaderValue::from_static("application/json; charset=utf-8"),
    )
}

fn public_bytes(
    request_headers: &HeaderMap,
    bytes: Vec<u8>,
    etag_seed: &[u8],
    content_type: HeaderValue,
) -> Result<Response, CommunityApiError> {
    let etag = format!("\"sha256:{:x}\"", Sha256::digest(etag_seed));
    let not_modified = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|values| {
            values
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == etag || candidate == "*")
        });
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut response = Response::new(Body::from(bytes));
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
        response
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(PUBLIC_CACHE),
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(internal_error)?,
    );
    Ok(response)
}

async fn repository_task<T, F>(operation: F) -> Result<T, CommunityApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, RepositoryError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| CommunityApiError::Internal(format!("repository worker failed: {error}")))?
        .map_err(CommunityApiError::from)
}

async fn repository_optional<T, F>(operation: F) -> Result<Option<T>, CommunityApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, RepositoryError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| CommunityApiError::Internal(format!("repository worker failed: {error}")))?
        .map(Some)
        .or_else(|error| match error {
            RepositoryError::NotFound => Ok(None),
            other => Err(CommunityApiError::from(other)),
        })
}

async fn hash_password(
    workers: Arc<tokio::sync::Semaphore>,
    password: String,
) -> Result<String, CommunityApiError> {
    let _permit = workers
        .acquire_owned()
        .await
        .map_err(|error| CommunityApiError::Internal(format!("password worker closed: {error}")))?;
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|value| value.to_string())
            .map_err(|error| {
                CommunityApiError::Internal(format!("password hashing failed: {error}"))
            })
    })
    .await
    .map_err(|error| CommunityApiError::Internal(format!("password worker failed: {error}")))?
}

async fn verify_password(
    workers: Arc<tokio::sync::Semaphore>,
    supplied: String,
    expected_phc: Option<String>,
) -> Result<bool, CommunityApiError> {
    let _permit = workers
        .acquire_owned()
        .await
        .map_err(|error| CommunityApiError::Internal(format!("password worker closed: {error}")))?;
    tokio::task::spawn_blocking(move || {
        static DUMMY_PHC: OnceLock<String> = OnceLock::new();
        let expected = expected_phc.unwrap_or_else(|| {
            DUMMY_PHC
                .get_or_init(|| {
                    let salt = SaltString::generate(&mut OsRng);
                    Argon2::default()
                        .hash_password(b"dummy-password-never-authenticates", &salt)
                        .expect("the static Argon2 parameters are valid")
                        .to_string()
                })
                .clone()
        });
        let parsed = PasswordHash::new(&expected).map_err(|error| {
            CommunityApiError::Internal(format!("stored password hash is invalid: {error}"))
        })?;
        Ok(Argon2::default()
            .verify_password(supplied.as_bytes(), &parsed)
            .is_ok())
    })
    .await
    .map_err(|error| CommunityApiError::Internal(format!("password worker failed: {error}")))?
}

pub(super) fn session_hash_from_headers(headers: &HeaderMap) -> Option<[u8; 32]> {
    let encoded = headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| (name == SESSION_COOKIE).then_some(value))?;
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let raw: [u8; 32] = decoded.try_into().ok()?;
    Some(Sha256::digest(raw).into())
}

fn session_cookie(token: &str, secure: bool, path: &str, max_age_seconds: i64) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path={path}; Max-Age={max_age_seconds}{}",
        if secure { "; Secure" } else { "" }
    )
}

fn clear_session_cookie(secure: bool, path: &str) -> String {
    format!(
        "{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path={path}; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT{}",
        if secure { "; Secure" } else { "" }
    )
}

fn session_cookie_path(state: &AppState) -> String {
    let path = state.seo_policy.public_url.path().trim_end_matches('/');
    if path.is_empty() {
        "/".into()
    } else {
        path.into()
    }
}

fn ensure_mutable(state: &AppState) -> Result<(), CommunityApiError> {
    if state.delivery_only {
        Err(CommunityApiError::ReadOnly)
    } else {
        Ok(())
    }
}

fn validate_email(value: &str) -> Result<String, CommunityApiError> {
    let normalized = value.trim().to_ascii_lowercase();
    let valid = normalized.is_ascii()
        && (3..=254).contains(&normalized.len())
        && !normalized.chars().any(char::is_control)
        && normalized.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
        });
    if valid {
        Ok(normalized)
    } else {
        Err(CommunityApiError::BadRequest(
            "email must be a valid ASCII address".into(),
        ))
    }
}

fn validate_handle(value: &str, label: &str) -> Result<(), CommunityApiError> {
    let bytes = value.as_bytes();
    let valid = (3..=40).contains(&bytes.len())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(CommunityApiError::BadRequest(format!(
            "{label} must be 3-40 lowercase ASCII letters, digits, or interior hyphens"
        )))
    }
}

fn validate_text(value: &str, label: &str, maximum: usize) -> Result<String, CommunityApiError> {
    let value = value.trim();
    let length = value.chars().count();
    if (1..=maximum).contains(&length) && !value.chars().any(char::is_control) {
        Ok(value.to_owned())
    } else {
        Err(CommunityApiError::BadRequest(format!(
            "{label} must be 1-{maximum} non-control characters"
        )))
    }
}

fn validate_password(value: &str) -> Result<(), CommunityApiError> {
    if (8..=1024).contains(&value.len()) && !value.contains('\0') {
        Ok(())
    } else {
        Err(CommunityApiError::BadRequest(
            "password must be 8-1024 bytes".into(),
        ))
    }
}

fn validate_password_for_login(value: &str) -> Result<(), CommunityApiError> {
    if value.is_empty() || value.len() > 4096 || value.contains('\0') {
        Err(CommunityApiError::InvalidLogin)
    } else {
        Ok(())
    }
}

fn internal_error(error: impl std::fmt::Display) -> CommunityApiError {
    CommunityApiError::Internal(error.to_string())
}

fn map_asset_error(error: AssetError) -> CommunityApiError {
    match error {
        AssetError::TooLarge { .. } => CommunityApiError::PayloadTooLarge(error.to_string()),
        AssetError::UnsafeFormat { .. }
        | AssetError::UnsupportedFormat
        | AssetError::ClaimedMediaTypeMismatch { .. } => {
            CommunityApiError::UnsupportedMediaType(error.to_string())
        }
        AssetError::InvalidDigest => CommunityApiError::BadRequest(error.to_string()),
        AssetError::NotFound { .. } => CommunityApiError::NotFound,
        AssetError::MetadataMissing { .. }
        | AssetError::IntegrityMismatch { .. }
        | AssetError::Io(_)
        | AssetError::Metadata(_) => CommunityApiError::Internal(error.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegisterInput {
    email: String,
    password: String,
    handle: String,
    display_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginInput {
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateBlogInput {
    handle: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    theme_preset: ThemeProfile,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AddCollaboratorInput {
    email: String,
    role: SiteMembershipRole,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateStudioSettingsInput {
    theme_preset: ThemeProfile,
    #[serde(default)]
    custom_css: CssUpdate,
}

#[derive(Debug, Default)]
struct CssUpdate {
    provided: bool,
    value: Option<String>,
}

impl<'de> Deserialize<'de> for CssUpdate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self {
            provided: true,
            value: Option::<String>::deserialize(deserializer)?,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StudioDocumentInput {
    title: String,
    slug: String,
    source_markdown: String,
    #[serde(default)]
    embeds: Vec<EmbedReference>,
    #[serde(default)]
    intent: Option<IntentLayer>,
    #[serde(default)]
    ontology: Option<OntologySidecar>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StudioRevisionInput {
    base_revision_id: Uuid,
    title: String,
    slug: String,
    source_markdown: String,
    #[serde(default)]
    embeds: Vec<EmbedReference>,
    #[serde(default)]
    intent: Option<IntentLayer>,
    #[serde(default)]
    ontology: Option<OntologySidecar>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublishInput {
    revision_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateCommentInput {
    source_markdown: String,
}

#[derive(Debug, Serialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum SessionPayload {
    Anonymous {
        registration_open: bool,
    },
    Authenticated {
        #[serde(flatten)]
        session: Box<AuthenticatedSession>,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthenticatedSession {
    registration_open: bool,
    user: UserSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    blog: Option<BlogSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    membership_role: Option<SiteMembershipRole>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UserSummary {
    id: Uuid,
    handle: String,
    display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlogTheme {
    preset_id: ThemeProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_css_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlogSummary {
    id: Uuid,
    handle: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    owner: UserSummary,
    theme: BlogTheme,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
struct FeedResponse {
    items: Vec<FeedPostSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedPostSummary {
    id: Uuid,
    title: String,
    slug: String,
    excerpt: String,
    published_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    author: UserSummary,
    blog: BlogSummary,
    tags: Vec<String>,
    comment_count: usize,
    has_intent_view: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    cover_image_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlogPostView {
    id: Uuid,
    title: String,
    canonical_slug: String,
    requested_slug: String,
    revision_id: Uuid,
    markdown: String,
    embeds: Vec<EmbedReference>,
    artifact: PublishArtifact,
    #[serde(skip_serializing_if = "Option::is_none")]
    ontology: Option<OntologySidecar>,
    slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt: Option<String>,
    published_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    author: UserSummary,
    blog: BlogSummary,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cover_image_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct PreviewResponse {
    artifact: PublishArtifact,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StudioAssetUploadResponse {
    record: AssetRecord,
    url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StudioSettings {
    blog_id: Uuid,
    theme_preset: ThemeProfile,
    theme_revision: u64,
    custom_css_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_css: Option<String>,
}

#[derive(Debug, Serialize)]
struct CollaboratorListResponse {
    items: Vec<CollaboratorView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollaboratorView {
    user_id: Uuid,
    email: String,
    handle: String,
    display_name: String,
    role: SiteMembershipRole,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct CommentListResponse {
    items: Vec<CommentView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommentView {
    id: Uuid,
    post_id: Uuid,
    author: UserSummary,
    source_markdown: String,
    artifact_html: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    can_edit: bool,
    can_delete: bool,
}

#[derive(Debug)]
pub(super) enum CommunityApiError {
    Unauthorized,
    Forbidden(String),
    InvalidLogin,
    RegistrationClosed,
    ReadOnly,
    NotFound,
    Conflict(String),
    BadRequest(String),
    PayloadTooLarge(String),
    UnsupportedMediaType(String),
    Internal(String),
}

impl From<RepositoryError> for CommunityApiError {
    fn from(error: RepositoryError) -> Self {
        match error {
            RepositoryError::NotFound => Self::NotFound,
            RepositoryError::DuplicateSlug
            | RepositoryError::RevisionConflict
            | RepositoryError::DuplicateIdempotencyKey => Self::Conflict(error.to_string()),
            RepositoryError::Validation(message) => Self::BadRequest(message),
            RepositoryError::Storage(message) => Self::Internal(message),
        }
    }
}

impl IntoResponse for CommunityApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "sign in to continue".to_owned(),
            ),
            Self::InvalidLogin => (
                StatusCode::UNAUTHORIZED,
                "invalid_login",
                "email or password is incorrect".to_owned(),
            ),
            Self::Forbidden(message) => (StatusCode::FORBIDDEN, "forbidden", message),
            Self::RegistrationClosed => (
                StatusCode::FORBIDDEN,
                "registration_closed",
                "registration is disabled for this deployment".to_owned(),
            ),
            Self::ReadOnly => (
                StatusCode::SERVICE_UNAVAILABLE,
                "delivery_only",
                "this deployment serves published content only".to_owned(),
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "the requested resource was not found".to_owned(),
            ),
            Self::Conflict(message) => (StatusCode::CONFLICT, "conflict", message),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, "bad_request", message),
            Self::PayloadTooLarge(message) => {
                (StatusCode::PAYLOAD_TOO_LARGE, "asset_too_large", message)
            }
            Self::UnsupportedMediaType(message) => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_asset",
                message,
            ),
            Self::Internal(message) => {
                tracing::error!(error = %message, "community request failed internally");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "the request could not be completed".to_owned(),
                )
            }
        };
        (
            status,
            Json(serde_json::json!({ "error": code, "message": message })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site_with_css() -> SiteRecord {
        SiteRecord {
            id: Uuid::now_v7(),
            handle: "css-site".into(),
            title: "CSS site".into(),
            description: None,
            owner_user_id: Uuid::now_v7(),
            theme_profile: ThemeProfile::Paper,
            theme_revision: 2,
            custom_css: Some(".article-content { color: rebeccapurple; }".into()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn owner(id: Uuid) -> UserRecord {
        UserRecord {
            id,
            email: "owner@example.test".into(),
            handle: "owner".into(),
            display_name: "Owner".into(),
            password_phc: "$argon2id$hidden".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn blog_theme_css_is_exposed_only_when_the_runtime_feature_is_enabled() {
        let site = site_with_css();
        let user = owner(site.owner_user_id);
        let policy = osb_feature_seo::SeoPolicy {
            public_url: url::Url::parse("https://blog.example/base").unwrap(),
            article_base_path: "blog".into(),
            no_index: false,
        };
        let enabled = blog_summary_with_css(site.clone(), user.clone(), true, &policy).theme;
        assert_eq!(
            enabled.custom_css_url.as_deref(),
            Some("https://blog.example/base/api/v1/blogs/css-site/custom.css")
        );
        let disabled = blog_summary_with_css(site, user, false, &policy).theme;
        assert_eq!(disabled.custom_css_url, None);
    }

    #[test]
    fn first_party_stylesheet_is_scoped_to_the_exact_site_identity() {
        let site_id = Uuid::parse_str("018f0000-0000-7000-8000-000000000123").unwrap();
        let stylesheet = scoped_site_stylesheet(site_id, ".article-content { color: purple; }");
        assert!(stylesheet.starts_with(
            "@scope (.osb-site-theme[data-site-id=\"018f0000-0000-7000-8000-000000000123\"]) {"
        ));
        assert!(stylesheet.contains(".article-content { color: purple; }"));
    }

    #[test]
    fn settings_input_distinguishes_preserve_from_an_explicit_clear() {
        let preserve: UpdateStudioSettingsInput =
            serde_json::from_value(serde_json::json!({ "themePreset": "paper" })).unwrap();
        assert!(!preserve.custom_css.provided);

        let clear: UpdateStudioSettingsInput = serde_json::from_value(serde_json::json!({
            "themePreset": "ink",
            "customCss": null
        }))
        .unwrap();
        assert!(clear.custom_css.provided);
        assert_eq!(clear.custom_css.value, None);
    }
}
