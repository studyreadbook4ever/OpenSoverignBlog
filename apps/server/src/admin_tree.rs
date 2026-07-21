use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use osb_kernel::DocumentStatus;
use osb_storage_sqlite::{
    CategoryMetadataRecord, CategoryStatus, CurrentDocumentMetadataRecord, RevisionMetadataRecord,
    SiteMetadataRecord,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ApiError, AppState, ModuleStatus, community, repository_task};

const TREE_SCHEMA_VERSION: &str = "open-soverign-blog-admin-tree/1";
const ROOT_NODE_ID: &str = "root";
const DEFAULT_PAGE_SIZE: usize = 100;
const MAX_PAGE_SIZE: usize = 200;
const MAX_PARENT_ID_LENGTH: usize = 128;
const MAX_CURSOR_LENGTH: usize = 32;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct AdminTreeQuery {
    parent: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AdminTreePage {
    schema_version: &'static str,
    generated_at: DateTime<Utc>,
    parent_id: String,
    items: Vec<AdminTreeNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// A deliberately closed projection of the running installation.
///
/// This type must remain metadata-only. In particular, do not add source
/// Markdown, custom CSS, filesystem/database paths, environment variables,
/// credential material, user identity fields, or provider configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminTreeNode {
    id: String,
    parent_id: String,
    kind: AdminTreeNodeKind,
    label: String,
    has_children: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operational: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AdminTreeNodeKind {
    Group,
    Site,
    Category,
    Document,
    Revision,
    Setting,
    Module,
    Runtime,
}

pub(super) async fn get_admin_tree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminTreeQuery>,
) -> Result<Json<AdminTreePage>, ApiError> {
    // The outer admin guard intentionally allows the narrowly scoped MCP
    // content routes. Repeat the stronger persisted control-plane check here
    // so this installation-wide view is browser-instance-admin only. A blog
    // owner/editor/writer session and the test-only bearer both fail closed.
    require_instance_administrator(&state, &headers).await?;

    let parent_id = normalize_parent_id(query.parent)?;
    let limit = validate_page_size(query.limit)?;
    let offset = decode_cursor(query.cursor.as_deref())?;
    // The repository receives the decoded offset directly and returns only a
    // page plus one look-ahead row. Earlier rows and private content payloads
    // are never hydrated merely to expand a later tree page.
    let fetch_limit = limit.saturating_add(1);
    let nodes = nodes_for_parent(&state, &parent_id, offset, fetch_limit).await?;
    let (items, next_cursor) = finish_page(nodes, offset, limit)?;

    Ok(Json(AdminTreePage {
        schema_version: TREE_SCHEMA_VERSION,
        generated_at: Utc::now(),
        parent_id,
        items,
        next_cursor,
    }))
}

async fn require_instance_administrator(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let token_hash = community::session_hash_from_headers(headers).ok_or(ApiError::Unauthorized)?;
    let repository = state.repository.clone();
    repository_task(move || {
        repository
            .get_primary_owner_session(&token_hash)
            .map(|_| ())
    })
    .await
    .map_err(|error| match error {
        ApiError::Repository(osb_kernel::RepositoryError::NotFound) => ApiError::Unauthorized,
        other => other,
    })
}

async fn nodes_for_parent(
    state: &AppState,
    parent_id: &str,
    offset: usize,
    fetch_limit: usize,
) -> Result<Vec<AdminTreeNode>, ApiError> {
    match parent_id {
        ROOT_NODE_ID => Ok(page_static(root_nodes(), offset, fetch_limit)),
        "group:content" => {
            let repository = state.repository.clone();
            let sites =
                repository_task(move || repository.list_site_metadata_page(offset, fetch_limit))
                    .await?;
            Ok(sites.into_iter().map(site_node).collect())
        }
        "group:assets" => Ok(page_static(
            vec![static_node(
                "runtime:asset-store",
                "group:assets",
                AdminTreeNodeKind::Runtime,
                "Content-addressed asset store",
                false,
                Some("enabled"),
            )],
            offset,
            fetch_limit,
        )),
        "group:configuration" => Ok(page_static(configuration_nodes(state), offset, fetch_limit)),
        "group:modules" => Ok(page_static(module_nodes(state), offset, fetch_limit)),
        "group:runtime" => Ok(page_static(runtime_nodes(state), offset, fetch_limit)),
        _ => {
            if let Some(id) = parse_uuid_node(parent_id, "site:")? {
                let repository = state.repository.clone();
                // Uncategorized is the first logical child without occupying a
                // database row, so translate the logical offset by exactly one.
                let include_uncategorized = offset == 0;
                let category_offset = offset.saturating_sub(1);
                let category_limit = fetch_limit.saturating_sub(usize::from(include_uncategorized));
                let categories = repository_task(move || {
                    repository.list_category_metadata_page(
                        id,
                        true,
                        category_offset,
                        category_limit,
                    )
                })
                .await?;
                let mut nodes = Vec::with_capacity(
                    categories
                        .len()
                        .saturating_add(usize::from(include_uncategorized)),
                );
                if include_uncategorized {
                    nodes.push(uncategorized_node(id));
                }
                nodes.extend(categories.into_iter().map(category_node));
                return Ok(nodes);
            }
            if let Some(category_parent) = parse_category_parent(parent_id)? {
                let repository = state.repository.clone();
                let document_parent_id = category_parent.node_id();
                let documents = repository_task(move || {
                    repository.list_current_document_metadata_page(
                        category_parent.site_id,
                        category_parent.category_id,
                        offset,
                        fetch_limit,
                    )
                })
                .await?;
                return Ok(documents
                    .into_iter()
                    .map(|document| document_node(document, &document_parent_id))
                    .collect());
            }
            if let Some(id) = parse_uuid_node(parent_id, "document:")? {
                let repository = state.repository.clone();
                let revisions = repository_task(move || {
                    repository.list_revision_metadata_page(id, offset, fetch_limit)
                })
                .await?;
                return Ok(revisions.into_iter().map(revision_node).collect());
            }
            if is_leaf_node_id(parent_id)? {
                return Err(ApiError::BadRequest(
                    "the selected program-tree node has no children".into(),
                ));
            }
            Err(ApiError::BadRequest(
                "unknown program-tree parent node".into(),
            ))
        }
    }
}

fn root_nodes() -> Vec<AdminTreeNode> {
    [
        ("group:content", "Content"),
        ("group:assets", "Assets"),
        ("group:configuration", "Configuration"),
        ("group:modules", "Modules"),
        ("group:runtime", "Runtime"),
    ]
    .into_iter()
    .map(|(id, label)| {
        static_node(
            id,
            ROOT_NODE_ID,
            AdminTreeNodeKind::Group,
            label,
            true,
            None,
        )
    })
    .collect()
}

fn site_node(site: SiteMetadataRecord) -> AdminTreeNode {
    AdminTreeNode {
        id: format!("site:{}", site.id),
        parent_id: "group:content".into(),
        kind: AdminTreeNodeKind::Site,
        label: site.title,
        has_children: true,
        entity_id: Some(site.id),
        handle: Some(site.handle),
        slug: None,
        state: None,
        revision_number: None,
        requested: None,
        operational: None,
        summary: None,
        created_at: Some(site.created_at),
        updated_at: Some(site.updated_at),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CategoryParent {
    site_id: Uuid,
    category_id: Option<Uuid>,
}

impl CategoryParent {
    fn node_id(self) -> String {
        match self.category_id {
            Some(category_id) => format!("category:{}:{category_id}", self.site_id),
            None => format!("category:{}:uncategorized", self.site_id),
        }
    }
}

fn category_node(category: CategoryMetadataRecord) -> AdminTreeNode {
    let parent = CategoryParent {
        site_id: category.site_id,
        category_id: Some(category.id),
    };
    AdminTreeNode {
        id: parent.node_id(),
        parent_id: format!("site:{}", category.site_id),
        kind: AdminTreeNodeKind::Category,
        label: category.title,
        has_children: true,
        entity_id: Some(category.id),
        handle: None,
        slug: Some(category.slug),
        state: Some(category_status(category.status).into()),
        revision_number: None,
        requested: None,
        operational: None,
        summary: None,
        created_at: Some(category.created_at),
        updated_at: Some(category.updated_at),
    }
}

fn uncategorized_node(site_id: Uuid) -> AdminTreeNode {
    let parent = CategoryParent {
        site_id,
        category_id: None,
    };
    AdminTreeNode {
        id: parent.node_id(),
        parent_id: format!("site:{site_id}"),
        kind: AdminTreeNodeKind::Category,
        label: "Uncategorized".into(),
        has_children: true,
        entity_id: None,
        handle: None,
        slug: None,
        state: Some("uncategorized".into()),
        revision_number: None,
        requested: None,
        operational: None,
        summary: None,
        created_at: None,
        updated_at: None,
    }
}

fn document_node(document: CurrentDocumentMetadataRecord, parent_id: &str) -> AdminTreeNode {
    AdminTreeNode {
        id: format!("document:{}", document.id),
        parent_id: parent_id.into(),
        kind: AdminTreeNodeKind::Document,
        label: document.title,
        has_children: true,
        entity_id: Some(document.id),
        handle: None,
        slug: Some(document.slug),
        state: Some(document_status(document.status).into()),
        revision_number: Some(document.revision_number),
        requested: None,
        operational: None,
        summary: None,
        created_at: Some(document.created_at),
        updated_at: Some(document.updated_at),
    }
}

fn revision_node(revision: RevisionMetadataRecord) -> AdminTreeNode {
    AdminTreeNode {
        id: format!("revision:{}", revision.id),
        parent_id: format!("document:{}", revision.document_id),
        kind: AdminTreeNodeKind::Revision,
        label: format!("Revision {}", revision.revision_number),
        has_children: false,
        entity_id: Some(revision.id),
        handle: None,
        slug: Some(revision.slug),
        state: None,
        revision_number: Some(revision.revision_number),
        requested: None,
        operational: None,
        summary: None,
        created_at: Some(revision.created_at),
        updated_at: None,
    }
}

fn configuration_nodes(state: &AppState) -> Vec<AdminTreeNode> {
    [
        (
            "setting:administrator-auth",
            "Administrator authentication",
            state.admin_auth.mode().as_str(),
        ),
        (
            "setting:member-auth",
            "Member authentication",
            enabled(state.local_auth_enabled),
        ),
        (
            "setting:registration",
            "Member registration",
            enabled(state.registration_open),
        ),
        (
            "setting:comments",
            "Comments",
            enabled(state.comments_enabled),
        ),
        (
            "setting:collaboration",
            "Studio collaboration",
            enabled(state.collaboration_enabled),
        ),
        (
            "setting:custom-css",
            "Custom CSS",
            enabled(state.custom_css_enabled),
        ),
        (
            "setting:agent-discovery",
            "Agent discovery",
            enabled(state.agent_discovery_enabled),
        ),
    ]
    .into_iter()
    .map(|(id, label, value)| {
        static_node(
            id,
            "group:configuration",
            AdminTreeNodeKind::Setting,
            label,
            false,
            Some(value),
        )
    })
    .collect()
}

fn module_nodes(state: &AppState) -> Vec<AdminTreeNode> {
    state
        .features
        .modules()
        .iter()
        .map(|module| AdminTreeNode {
            id: format!("module:{}", module.id),
            parent_id: "group:modules".into(),
            kind: AdminTreeNodeKind::Module,
            label: module.id.into(),
            has_children: false,
            entity_id: None,
            handle: None,
            slug: None,
            state: Some(module_status(module.status).into()),
            revision_number: None,
            requested: Some(module.requested),
            operational: Some(module.operational),
            summary: Some(module.reason.clone()),
            created_at: None,
            updated_at: None,
        })
        .collect()
}

fn runtime_nodes(state: &AppState) -> Vec<AdminTreeNode> {
    let backup_state = if state.delivery_only {
        "not_applicable"
    } else if state.backup.is_some() {
        "managed"
    } else {
        "externally_managed"
    };
    [
        (
            "runtime:server",
            "OpenSoverignBlog server",
            env!("CARGO_PKG_VERSION"),
        ),
        (
            "runtime:mutation-mode",
            "Mutation mode",
            if state.delivery_only {
                "read_only"
            } else {
                "writable"
            },
        ),
        (
            "runtime:public-cache",
            "Public derivative cache",
            enabled(state.cache.is_some()),
        ),
        ("runtime:backups", "Backups", backup_state),
    ]
    .into_iter()
    .map(|(id, label, value)| {
        static_node(
            id,
            "group:runtime",
            AdminTreeNodeKind::Runtime,
            label,
            false,
            Some(value),
        )
    })
    .collect()
}

fn static_node(
    id: &str,
    parent_id: &str,
    kind: AdminTreeNodeKind,
    label: &str,
    has_children: bool,
    state: Option<&str>,
) -> AdminTreeNode {
    AdminTreeNode {
        id: id.into(),
        parent_id: parent_id.into(),
        kind,
        label: label.into(),
        has_children,
        entity_id: None,
        handle: None,
        slug: None,
        state: state.map(str::to_owned),
        revision_number: None,
        requested: None,
        operational: None,
        summary: None,
        created_at: None,
        updated_at: None,
    }
}

fn normalize_parent_id(parent: Option<String>) -> Result<String, ApiError> {
    let parent = parent.unwrap_or_else(|| ROOT_NODE_ID.into());
    if parent.is_empty()
        || parent.len() > MAX_PARENT_ID_LENGTH
        || parent.chars().any(char::is_control)
    {
        return Err(ApiError::BadRequest(
            "program-tree parent node is invalid".into(),
        ));
    }
    Ok(parent)
}

fn validate_page_size(limit: Option<usize>) -> Result<usize, ApiError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "program-tree limit must be between 1 and {MAX_PAGE_SIZE}"
        )));
    }
    Ok(limit)
}

fn decode_cursor(cursor: Option<&str>) -> Result<usize, ApiError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_LENGTH {
        return Err(ApiError::BadRequest(
            "program-tree cursor is invalid".into(),
        ));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| ApiError::BadRequest("program-tree cursor is invalid".into()))?;
    let bytes: [u8; 8] = decoded
        .try_into()
        .map_err(|_| ApiError::BadRequest("program-tree cursor is invalid".into()))?;
    let offset = u64::from_be_bytes(bytes);
    if offset > i64::MAX as u64 {
        return Err(ApiError::BadRequest(
            "program-tree cursor is invalid".into(),
        ));
    }
    usize::try_from(offset)
        .map_err(|_| ApiError::BadRequest("program-tree cursor is invalid".into()))
}

fn encode_cursor(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode((offset as u64).to_be_bytes())
}

fn page_static<T>(items: Vec<T>, offset: usize, limit: usize) -> Vec<T> {
    items.into_iter().skip(offset).take(limit).collect()
}

/// Converts a repository look-ahead page (`limit + 1`) into the public page.
/// The input already starts at `offset`; do not skip it a second time.
fn finish_page<T>(
    mut items: Vec<T>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<T>, Option<String>), ApiError> {
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next_cursor = if has_more {
        let next_offset = offset
            .checked_add(limit)
            .ok_or_else(|| ApiError::BadRequest("program-tree cursor is invalid".into()))?;
        Some(encode_cursor(next_offset))
    } else {
        None
    };
    Ok((items, next_cursor))
}

fn parse_uuid_node(value: &str, prefix: &str) -> Result<Option<Uuid>, ApiError> {
    let Some(id) = value.strip_prefix(prefix) else {
        return Ok(None);
    };
    if id.is_empty() || id.contains(':') {
        return Err(ApiError::BadRequest(
            "program-tree entity node is invalid".into(),
        ));
    }
    Uuid::parse_str(id)
        .map(Some)
        .map_err(|_| ApiError::BadRequest("program-tree entity node is invalid".into()))
}

fn parse_category_parent(value: &str) -> Result<Option<CategoryParent>, ApiError> {
    let Some(value) = value.strip_prefix("category:") else {
        return Ok(None);
    };
    let mut parts = value.split(':');
    let site_id = parts.next().unwrap_or_default();
    let category_id = parts.next().unwrap_or_default();
    if site_id.is_empty() || category_id.is_empty() || parts.next().is_some() {
        return Err(ApiError::BadRequest(
            "program-tree category node is invalid".into(),
        ));
    }
    let site_id = Uuid::parse_str(site_id)
        .map_err(|_| ApiError::BadRequest("program-tree category node is invalid".into()))?;
    let category_id =
        if category_id == "uncategorized" {
            None
        } else {
            Some(Uuid::parse_str(category_id).map_err(|_| {
                ApiError::BadRequest("program-tree category node is invalid".into())
            })?)
        };
    Ok(Some(CategoryParent {
        site_id,
        category_id,
    }))
}

fn is_leaf_node_id(value: &str) -> Result<bool, ApiError> {
    if parse_uuid_node(value, "revision:")?.is_some() {
        return Ok(true);
    }
    Ok(value.starts_with("setting:")
        || value.starts_with("module:")
        || value.starts_with("runtime:"))
}

const fn enabled(value: bool) -> &'static str {
    if value { "enabled" } else { "disabled" }
}

const fn document_status(status: DocumentStatus) -> &'static str {
    match status {
        DocumentStatus::Draft => "draft",
        DocumentStatus::Published => "published",
        DocumentStatus::Archived => "archived",
    }
}

const fn category_status(status: CategoryStatus) -> &'static str {
    match status {
        CategoryStatus::Active => "active",
        CategoryStatus::Archived => "archived",
    }
}

const fn module_status(status: ModuleStatus) -> &'static str {
    match status {
        ModuleStatus::Active => "active",
        ModuleStatus::Available => "available",
        ModuleStatus::Degraded => "degraded",
        ModuleStatus::Disabled => "disabled",
        ModuleStatus::Misconfigured => "misconfigured",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn pagination_cursor_is_opaque_bounded_and_round_trips() {
        let nodes = root_nodes();
        let first_lookup = page_static(nodes.clone(), 0, 3);
        let (first, cursor) =
            finish_page(first_lookup, 0, 2).unwrap_or_else(|_| panic!("valid first page"));
        assert_eq!(first.len(), 2);
        let cursor = cursor.expect("another page");
        assert!(!cursor.contains('2'));
        let offset = decode_cursor(Some(&cursor))
            .unwrap_or_else(|_| panic!("generated cursor must round-trip"));
        assert_eq!(offset, 2);

        let second_lookup = page_static(nodes, offset, 21);
        let (second, next) =
            finish_page(second_lookup, offset, 20).unwrap_or_else(|_| panic!("valid last page"));
        assert_eq!(second.len(), 3);
        assert!(next.is_none());
        assert!(decode_cursor(Some("not-a-cursor")).is_err());
        assert!(decode_cursor(Some(&URL_SAFE_NO_PAD.encode(u64::MAX.to_be_bytes()))).is_err());
    }

    #[test]
    fn lookahead_page_does_not_skip_the_decoded_offset_twice() {
        let repository_page = vec![500, 501, 502];
        let (page, cursor) =
            finish_page(repository_page, 500, 2).unwrap_or_else(|_| panic!("valid page"));
        assert_eq!(page, vec![500, 501]);
        assert_eq!(
            decode_cursor(cursor.as_deref()).unwrap_or_else(|_| panic!("generated cursor")),
            502
        );
    }

    #[test]
    fn static_tree_projection_has_no_open_metadata_bag() {
        let encoded = serde_json::to_value(root_nodes()).unwrap();
        let nodes = encoded.as_array().unwrap();
        assert_eq!(nodes.len(), 5);
        for node in nodes {
            let fields = node.as_object().unwrap();
            assert!(!fields.contains_key("metadata"));
            assert!(!fields.contains_key("path"));
            assert!(!fields.contains_key("environment"));
            assert!(!fields.contains_key("config"));
        }
    }

    #[test]
    fn document_projection_drops_private_revision_payloads() {
        let document = CurrentDocumentMetadataRecord {
            id: Uuid::now_v7(),
            site_id: Uuid::now_v7(),
            status: DocumentStatus::Draft,
            current_revision_id: Uuid::now_v7(),
            published_revision_id: None,
            title: "Safe title".into(),
            slug: "safe-slug".into(),
            revision_number: 1,
            category_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let value =
            serde_json::to_value(document_node(document, "category:test:uncategorized")).unwrap();
        let encoded = serde_json::to_string(&value).unwrap();
        assert_eq!(value["label"], Value::String("Safe title".into()));
        assert_eq!(value["slug"], Value::String("safe-slug".into()));
        for secret in [
            "TOP SECRET MARKDOWN",
            "sensitive-internal-actor",
            "Sensitive Person",
            "sensitive-content-hash",
            "sourceMarkdown",
            "customCss",
        ] {
            assert!(!encoded.contains(secret), "leaked {secret}");
        }
    }

    #[test]
    fn category_projection_exposes_only_closed_navigation_metadata() {
        let category = CategoryMetadataRecord {
            id: Uuid::now_v7(),
            site_id: Uuid::now_v7(),
            slug: "notes".into(),
            title: "Notes".into(),
            status: CategoryStatus::Archived,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let value = serde_json::to_value(category_node(category)).unwrap();
        let encoded = serde_json::to_string(&value).unwrap();
        assert_eq!(value["kind"], Value::String("category".into()));
        assert_eq!(value["label"], Value::String("Notes".into()));
        assert_eq!(value["slug"], Value::String("notes".into()));
        assert_eq!(value["state"], Value::String("archived".into()));
        for secret in [
            "PRIVATE CATEGORY DESCRIPTION",
            "description",
            "createdByUserId",
            "themeProfile",
        ] {
            assert!(!encoded.contains(secret), "leaked {secret}");
        }
    }
}
