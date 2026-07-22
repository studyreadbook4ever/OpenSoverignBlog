export type ViewMode = "intent" | "markdown" | "markdown_source";

export interface IntentLayer {
  format: string;
  sourceHtml: string;
  rendererHints?: Record<string, string>;
  provenance?: {
    origin: string;
    sourceUri?: string;
    actorId?: string;
    generatedBy?: string;
  };
}

export interface OntologyStatement {
  subject: string;
  predicate: string;
  object: unknown;
  evidence?: string;
  confirmedByAuthor: boolean;
}

export interface OntologySidecar {
  schema: string;
  statements: OntologyStatement[];
}

export interface EmbedReference {
  id: string;
  provider: string;
  resourceId: string;
  canonicalUrl: string;
  title: string;
  consentPurposeIds: string[];
}

export type PublicAuthorshipKind = "human" | "ai_generated" | "ai_assisted" | "imported";

/** Portable public provenance. It intentionally contains no internal actor ID. */
export type PublicAuthorship =
  | { kind: "human"; generator?: never; humanReviewed: boolean }
  | {
      kind: Exclude<PublicAuthorshipKind, "human">;
      generator: string;
      humanReviewed: boolean;
    };

export interface AiSummaryProvenance {
  /** Portable provider label; imported content is not limited to built-ins. */
  provider: string;
  model: string;
  promptVersion: string;
  generatedAt: string;
  humanReviewed: boolean;
}

export type BuiltInAiSummaryProviderId = "openai" | "anthropic" | "google";

/** A reviewed, plain-text summary stored with an immutable revision. */
export interface AiSummary {
  text: string;
  sourceHash: string;
  provenance: AiSummaryProvenance;
}

export interface AiSummaryProvider {
  id: BuiltInAiSummaryProviderId;
  label: string;
  models: string[];
  defaultModel: string;
  credentialModes: Array<"one_shot">;
}

export interface AiSummaryProvidersResponse {
  providers: AiSummaryProvider[];
  maximumSourceBytes: number;
  credentialsPersisted: false;
}

export interface GenerateAiSummaryInput {
  provider: BuiltInAiSummaryProviderId;
  model: string;
  credentialMode: "one_shot";
  title: string;
  sourceMarkdown: string;
}

export interface GenerateAiSummaryResponse {
  candidate: AiSummary;
}

export interface PublishArtifact {
  view: ViewMode;
  html: string;
  sourceHash: string;
  artifactHash: string;
  rendererVersion: string;
  sanitizerPolicyVersion: string;
  requiredStyleAssets: string[];
  requiredScriptAssets: string[];
}

export interface PostSummary {
  id: string;
  title: string;
  /** Leaf slug retained for backwards-compatible display and routing logic. */
  slug: string;
  /** Published lookup path; category posts use `category/slug`. */
  routePath: string;
  /** Absolute, directly followable URL for the sanitized machine view. */
  apiHref: string;
  /** Absolute, directly followable URL for the portable Markdown source. */
  sourceHref: string;
  updatedAt: string;
  hasIntentView: boolean;
  hasOntology: boolean;
  authorship: PublicAuthorship;
}

export type ThemePresetId = "paper" | "ink" | "forest" | "terminal";

export interface UserSummary {
  id: string;
  handle: string;
  displayName: string;
  avatarUrl?: string;
}

export interface BlogTheme {
  presetId: ThemePresetId;
  /** Absolute same-origin URL for the blog's server-scoped stylesheet endpoint. */
  customCssUrl?: string;
}

export interface BlogSummary {
  id: string;
  handle: string;
  title: string;
  description?: string;
  owner: UserSummary;
  theme: BlogTheme;
  /** True only for the installation's provisioned blog with handle-free category URLs. */
  isPrimary: boolean;
  createdAt?: string;
}

export type CategoryStatus = "active" | "archived";

/** Public, site-scoped category metadata. Category slugs are immutable. */
export interface CategorySummary {
  id: string;
  slug: string;
  title: string;
  description?: string;
  /** Omission means that the category inherits its blog's theme. */
  themePreset?: ThemePresetId;
  status: CategoryStatus;
}

export interface CategoryListResponse {
  items: CategorySummary[];
}

export interface BlogCategoryResponse {
  category: CategorySummary;
  blog: BlogSummary;
  postCount: number;
}

export interface CreateCategoryInput {
  slug: string;
  title: string;
  description?: string;
  themePreset?: ThemePresetId;
}

export interface UpdateCategoryInput {
  title: string;
  description?: string;
  themePreset?: ThemePresetId;
}

export const ADMIN_TREE_SCHEMA_VERSION = "open-soverign-blog-admin-tree/1" as const;

export type AdminTreeNodeKind =
  | "group"
  | "site"
  | "category"
  | "document"
  | "revision"
  | "setting"
  | "module"
  | "runtime";

/**
 * The deliberately closed, metadata-only node projection returned to an
 * instance administrator. Unknown response fields are not copied into this
 * SDK model, which prevents an accidental server-side field addition from
 * becoming inspectable UI data by default.
 */
export interface AdminTreeNodeBase {
  id: string;
  parentId: string;
  label: string;
  hasChildren: boolean;
  entityId?: string;
  handle?: string;
  slug?: string;
  state?: string;
  revisionNumber?: number;
  requested?: boolean;
  operational?: boolean;
  summary?: string;
  createdAt?: string;
  updatedAt?: string;
}

export type AdminTreeNode = {
  [Kind in AdminTreeNodeKind]: AdminTreeNodeBase & { kind: Kind };
}[AdminTreeNodeKind];

export interface AdminTreePage {
  schemaVersion: typeof ADMIN_TREE_SCHEMA_VERSION;
  generatedAt: string;
  parentId: string;
  items: AdminTreeNode[];
  nextCursor?: string;
}

export interface AdminTreeQuery {
  parent?: string;
  cursor?: string;
  limit?: number;
}

export type Session =
  | {
      state: "anonymous";
      registrationOpen: boolean;
    }
  | {
      state: "authenticated";
      registrationOpen: boolean;
      user: UserSummary;
      blog?: BlogSummary;
      membershipRole?: SiteMembershipRole;
      instanceAdministrator: boolean;
    };

export type SiteMembershipRole = "owner" | "editor" | "writer";
export type CollaboratorRole = Exclude<SiteMembershipRole, "owner">;

export interface Collaborator {
  userId: string;
  email: string;
  handle: string;
  displayName: string;
  role: CollaboratorRole;
  createdAt: string;
}

export interface CollaboratorListResponse {
  items: Collaborator[];
}

export interface AddCollaboratorInput {
  email: string;
  role: CollaboratorRole;
}

export interface StudioSettings {
  blogId: string;
  themePreset: ThemePresetId;
  themeRevision: number;
  customCssEnabled: boolean;
  customCss?: string;
}

export interface UpdateStudioSettingsInput {
  themePreset: ThemePresetId;
  /** Omit to preserve, or pass null/empty text to clear. */
  customCss?: string | null;
}

export interface RegisterInput {
  email: string;
  password: string;
  handle: string;
  displayName: string;
}

export interface LoginInput {
  email: string;
  password: string;
}

export interface CreateBlogInput {
  handle: string;
  title: string;
  description?: string;
  themePreset: ThemePresetId;
}

export interface FeedPostSummary {
  id: string;
  title: string;
  slug: string;
  excerpt: string;
  publishedAt: string;
  updatedAt: string;
  author: UserSummary;
  blog: BlogSummary;
  tags: string[];
  commentCount: number;
  hasIntentView: boolean;
  authorship: PublicAuthorship;
  category?: CategorySummary;
  coverImageUrl?: string;
}

export interface FeedResponse {
  items: FeedPostSummary[];
}

export interface HomeCategorySection {
  category: CategorySummary;
  items: FeedPostSummary[];
}

export interface HomeResponse {
  pinnedItems: FeedPostSummary[];
  recentItems: FeedPostSummary[];
  /** Optional so a newer SDK can tolerate a rolling upgrade from an older server. */
  categorySections?: HomeCategorySection[];
}

export interface HomePinsResponse {
  documentIds: string[];
}

export interface PostView {
  id: string;
  title: string;
  canonicalSlug: string;
  requestedSlug: string;
  revisionId: string;
  markdown: string;
  embeds: EmbedReference[];
  artifact: PublishArtifact;
  ontology?: OntologySidecar;
  aiSummary?: AiSummary;
  authorship: PublicAuthorship;
}

export interface BlogPostView extends PostView {
  slug: string;
  excerpt?: string;
  publishedAt: string;
  updatedAt: string;
  author: UserSummary;
  blog: BlogSummary;
  tags: string[];
  category?: CategorySummary;
  coverImageUrl?: string;
}

export interface CommentView {
  id: string;
  postId: string;
  author: UserSummary;
  sourceMarkdown: string;
  artifactHtml: string;
  createdAt: string;
  updatedAt: string;
  canEdit?: boolean;
  canDelete?: boolean;
}

export interface CommentListResponse {
  items: CommentView[];
}

export interface CreateCommentInput {
  sourceMarkdown: string;
}

export interface Capabilities {
  version: string;
  views: ViewMode[];
  features: string[];
  modules: ModuleDescriptor[];
  unavailableByDefault: string[];
  mutationMechanisms: Array<"session">;
  mutationMode: "read_only" | "authenticated_members";
  /** Present on the v2 capability contract. Public reads remain anonymous in every profile. */
  publicAccess?: "anonymous_read";
  /**
   * Describes Studio authorization independently from the legacy mutation
   * transport. Older v1 servers omit this field.
   */
  studioAccess?: StudioAccess;
  /** Operational administrator authentication methods advertised by a v2 server. */
  auth?: AdminAuthCapabilities;
  /** Optional instance-wide attribution, licensing, privacy, and policy page. */
  references?: ReferencesDescriptor;
}

export interface ReferencesDescriptor {
  href: "/references";
  label: string;
}

export interface ReferencesPage {
  label: string;
  sourceMarkdown: string;
  artifactHtml: string;
  sourceHash: string;
  rendererVersion: string;
}

export type StudioAccess = "disabled" | "admin_only" | "members";
export type AdminAuthStatus = "disabled" | "ready" | "misconfigured";

export interface AdminAccessKeyMethod {
  id: "admin-access-key";
  kind: "access_key";
  flow: "secret_exchange";
  audience: "admin";
  label: string;
  actionHref: string;
}

export interface AdminExternalAuthMethod {
  id: "admin-external";
  kind: "external";
  flow: "redirect";
  audience: "admin";
  provider: string;
  label: string;
  actionHref: string;
}

export type AdminAuthMethod = AdminAccessKeyMethod | AdminExternalAuthMethod;

export interface AdminAuthCapabilities {
  status: AdminAuthStatus;
  methods: AdminAuthMethod[];
}

export interface AdminAccessKeyInput {
  accessKey: string;
}

export type DiscoveryAuth = "none" | "session" | "owner";
export type DiscoveryMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export interface DiscoveryEndpoint {
  href: string;
  methods: DiscoveryMethod[];
  auth: DiscoveryAuth;
  mutating: boolean;
  available: boolean;
}

export interface RedisCacheDependency {
  provider: "redis";
  state: "active" | "degraded" | "connecting" | "misconfigured";
  role: "discardable_public_derivative_cache";
  required: boolean;
}

export interface DisabledCacheDependency {
  provider: "none";
  state: "disabled";
  required: false;
}

export type DiscoveryCacheDependency = RedisCacheDependency | DisabledCacheDependency;
export type AdministratorAuthMode = "disabled" | "access_key" | "external";

export interface DiscoveryDocument {
  specVersion: "1.0";
  name: "OpenSoverignBlog";
  ai2aiVersion: "1.0";
  openapi: string;
  humanInstructions: string;
  endpoints: {
    capabilities: DiscoveryEndpoint;
    feed: DiscoveryEndpoint;
    blogs: DiscoveryEndpoint;
    publishedContent: DiscoveryEndpoint;
    comments: DiscoveryEndpoint;
    commentSubmission: DiscoveryEndpoint;
    proposeRevision: DiscoveryEndpoint;
    uploadFirstPartyAsset: DiscoveryEndpoint;
    runnerProfiles: DiscoveryEndpoint;
  };
  schemas: Record<string, string>;
  invariants: Record<string, boolean>;
  features: string[];
  modules: ModuleDescriptor[];
  operatorIntent: {
    localAuth: boolean;
    oauthRequested: boolean;
    administratorAuth: AdministratorAuthMode;
    comments: boolean;
    collaboration: boolean;
    customCss: boolean;
    agentDiscovery: boolean;
    deliveryOnly: boolean;
  };
  dependencies: {
    cache: DiscoveryCacheDependency;
    sourceOfTruth: ["sqlite", "content_addressed_blobs"];
  };
  externalProtocols: {
    a2a: {
      version: "1.0";
      status: "adapter_not_enabled";
      documentation: string;
    };
  };
}

export interface ModuleDescriptor {
  id: string;
  status: "active" | "available" | "degraded" | "disabled" | "misconfigured";
  requested: boolean;
  operational: boolean;
  reason: string;
}

export interface RunnerLimits {
  wallTimeMs: number;
  cpuTimeMs: number;
  memoryBytes: number;
  outputBytes: number;
  processLimit: number;
  network: "denied";
}

export interface RunnerProfile {
  id: string;
  digest: string;
  fenceAliases: string[];
  outputMode: "console" | "web_preview";
  maximumLimits: RunnerLimits;
  maximumSourceBytes: number;
}

export type CodeRunResponse =
  | {
      state: "queued";
      jobId: string;
      requestId: string;
      pollAfterMs: number;
    }
  | {
      state: "terminal";
      result: {
        jobId: string;
        requestId: string;
        attemptId: string;
        profileId: string;
        profileDigest: string;
        completedAt: string;
        outcome:
          | "succeeded"
          | "failed"
          | "timed_out"
          | "resource_limit_exceeded"
          | "cancelled"
          | "policy_rejected"
          | "runner_lost";
        exitCode: number | null;
        stdout: string;
        stderr: string;
        truncated: boolean;
      };
    };

export interface CreatePostInput {
  title: string;
  slug: string;
  sourceMarkdown: string;
  embeds?: EmbedReference[];
  intent?: IntentLayer;
  ontology?: OntologySidecar;
  authorship?: PublicAuthorship;
  aiSummary?: AiSummary;
  /** On a revision, omission inherits the placement and null clears it. */
  categoryId?: string | null;
}

export interface StudioPreviewResponse {
  artifact: PublishArtifact;
}

export interface ProposeRevisionInput extends CreatePostInput {
  baseRevisionId: string;
  idempotencyKey?: string;
}

export interface RevisionActor {
  kind: "human" | "agent" | "importer" | "system";
  id: string;
  displayName?: string;
}

export interface RevisionSnapshot {
  schemaVersion: string;
  id: string;
  documentId: string;
  revisionNumber: number;
  parentRevisionId?: string;
  title: string;
  slug: string;
  sourceMarkdown: string;
  embeds: EmbedReference[];
  intent?: IntentLayer;
  ontology?: OntologySidecar;
  authorship: PublicAuthorship;
  aiSummary?: AiSummary;
  actor: RevisionActor;
  contentHash: string;
  createdAt: string;
}

export interface DocumentSnapshot {
  schemaVersion: string;
  id: string;
  siteId: string;
  status: "draft" | "published" | "archived";
  currentRevisionId: string;
  publishedRevisionId?: string;
  revision: RevisionSnapshot;
  categoryId?: string;
  createdAt: string;
  updatedAt: string;
}

export interface AssetRecord {
  schemaVersion: string;
  digest: string;
  mediaType: string;
  size: number;
  originalFilename: string;
  createdAt: string;
}

export interface AssetUploadResponse {
  record: AssetRecord;
  url: string;
}

export interface VersionInfo {
  currentVersion: string;
  currentReleaseDate: string | null;
  latestVersion: string | null;
  latestReleaseDate: string | null;
  channel: string;
  updateAvailable: boolean;
  checkedAt: string | null;
  status: "disabled" | "offline" | "no_release" | "update_available" | "current";
  repositoryUrl: string;
  developerUrl: string;
  license: "Unlicense";
  licenseHref: "/UNLICENSE";
}

export type HealthBackupDependency =
  | {
      state: "not_applicable" | "externally_managed" | "unknown";
    }
  | {
      state: "waiting" | "running" | "healthy" | "degraded";
      intervalMinutes: number;
      retention: number;
      lastStartedAt: string | null;
      lastCompletedAt: string | null;
      lastGenerationAvailable: boolean;
      lastError: "backup_failed" | null;
    };

export interface Health {
  status: "ok" | "degraded";
  version: string;
  dependencies: {
    cache: DiscoveryCacheDependency;
    backups: HealthBackupDependency;
  };
  dataBoundary: {
    authoritative: ["sqlite", "content_addressed_blobs"];
    redisRole: "discardable_public_derivative_cache" | "disabled_by_installation";
  };
}

export interface ClientOptions {
  baseUrl?: string;
  fetch?: typeof globalThis.fetch;
}

export class OpenSoverignBlogError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = "OpenSoverignBlogError";
    this.status = status;
  }
}

export class OpenSoverignBlogClient {
  readonly #baseUrl: string;
  readonly #fetch: typeof globalThis.fetch;

  constructor(options: ClientOptions = {}) {
    this.#baseUrl = (options.baseUrl ?? "").replace(/\/$/, "");
    this.#fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  async discovery(signal?: AbortSignal): Promise<DiscoveryDocument> {
    return this.#request(
      "/.well-known/open-soverign-blog.json",
      withSignal(signal),
    );
  }

  async health(signal?: AbortSignal): Promise<Health> {
    return this.#request("/healthz", withSignal(signal));
  }

  async agentCompatibilityIndex(signal?: AbortSignal): Promise<string> {
    return this.#requestText("/agents.txt", signal);
  }

  async llmReaderIndex(signal?: AbortSignal): Promise<string> {
    return this.#requestText("/llms.txt", signal);
  }

  async capabilities(signal?: AbortSignal): Promise<Capabilities> {
    return this.#request("/api/v1/capabilities", withSignal(signal));
  }

  async references(signal?: AbortSignal): Promise<ReferencesPage> {
    return this.#request("/api/v1/references", withSignal(signal));
  }

  async codeRunnerProfiles(signal?: AbortSignal): Promise<RunnerProfile[]> {
    return this.#request("/api/v1/code-runner/profiles", withSignal(signal));
  }

  async submitCodeRun(
    profileId: string,
    source: string,
    signal?: AbortSignal,
  ): Promise<CodeRunResponse> {
    return this.#request(
      "/api/v1/code-runner/runs",
      {
        method: "POST",
        body: JSON.stringify({ profileId, source }),
        ...withSignal(signal),
      },
    );
  }

  async pollCodeRun(jobId: string, signal?: AbortSignal): Promise<CodeRunResponse> {
    return this.#request(
      `/api/v1/code-runner/runs/${encodeURIComponent(jobId)}`,
      withSignal(signal),
    );
  }

  async listPosts(signal?: AbortSignal): Promise<PostSummary[]> {
    return this.#request("/api/v1/posts", withSignal(signal));
  }

  async session(signal?: AbortSignal): Promise<Session> {
    return this.#request("/api/v1/session", withSignal(signal));
  }

  async register(input: RegisterInput, signal?: AbortSignal): Promise<Session> {
    return this.#request("/api/v1/auth/register", {
      method: "POST",
      body: JSON.stringify(input),
      ...withSignal(signal),
    });
  }

  async login(input: LoginInput, signal?: AbortSignal): Promise<Session> {
    return this.#request("/api/v1/auth/login", {
      method: "POST",
      body: JSON.stringify(input),
      ...withSignal(signal),
    });
  }

  /**
   * Exchanges a high-entropy administrator access key for the same HttpOnly
   * session used by the rest of Studio. The key is sent only in this request
   * body and is never installed as a Bearer credential by the SDK.
   */
  async loginWithAdminAccessKey(
    input: AdminAccessKeyInput,
    actionHref = "/api/v1/auth/access-key/session",
    signal?: AbortSignal,
  ): Promise<Session> {
    return this.#request(validateAdminAuthActionHref(actionHref), {
      method: "POST",
      body: JSON.stringify(input),
      headers: {
        "Cache-Control": "no-store",
        "Pragma": "no-cache",
      },
      ...withSignal(signal),
    });
  }

  /** Alias matching the capability's `adminAccess` terminology. */
  async adminAccessLogin(
    input: AdminAccessKeyInput,
    actionHref = "/api/v1/auth/access-key/session",
    signal?: AbortSignal,
  ): Promise<Session> {
    return this.loginWithAdminAccessKey(input, actionHref, signal);
  }

  async logout(signal?: AbortSignal): Promise<Session> {
    return this.#request("/api/v1/auth/logout", {
      method: "POST",
      body: JSON.stringify({}),
      ...withSignal(signal),
    });
  }

  async feed(signal?: AbortSignal): Promise<FeedResponse> {
    return this.#request("/api/v1/feed", withSignal(signal));
  }

  async home(signal?: AbortSignal): Promise<HomeResponse> {
    return this.#request("/api/v1/home", withSignal(signal));
  }

  async getHomePins(signal?: AbortSignal): Promise<HomePinsResponse> {
    return this.#request("/api/v1/admin/home/pins", withSignal(signal));
  }

  async replaceHomePins(
    documentIds: string[],
    signal?: AbortSignal,
  ): Promise<HomePinsResponse> {
    return this.#request("/api/v1/admin/home/pins", {
      method: "PUT",
      body: JSON.stringify({ documentIds }),
      ...withSignal(signal),
    });
  }

  async version(signal?: AbortSignal): Promise<VersionInfo> {
    return this.#request("/api/v1/version", withSignal(signal));
  }

  async listBlogs(signal?: AbortSignal): Promise<BlogSummary[]> {
    return this.#request("/api/v1/blogs", withSignal(signal));
  }

  async createBlog(input: CreateBlogInput, signal?: AbortSignal): Promise<BlogSummary> {
    return this.#request("/api/v1/blogs", {
      method: "POST",
      body: JSON.stringify(input),
      ...withSignal(signal),
    });
  }

  async getBlog(handle: string, signal?: AbortSignal): Promise<BlogSummary> {
    return this.#request(`/api/v1/blogs/${encodeURIComponent(handle)}`, withSignal(signal));
  }

  async getBlogPosts(handle: string, signal?: AbortSignal): Promise<FeedResponse> {
    return this.#request(
      `/api/v1/blogs/${encodeURIComponent(handle)}/posts`,
      withSignal(signal),
    );
  }

  async listBlogCategories(
    handle: string,
    signal?: AbortSignal,
  ): Promise<CategoryListResponse> {
    return this.#request(
      `/api/v1/blogs/${encodeURIComponent(handle)}/categories`,
      withSignal(signal),
    );
  }

  async getBlogCategory(
    handle: string,
    categorySlug: string,
    signal?: AbortSignal,
  ): Promise<BlogCategoryResponse> {
    return this.#request(
      `/api/v1/blogs/${encodeURIComponent(handle)}/categories/${encodeURIComponent(categorySlug)}`,
      withSignal(signal),
    );
  }

  async getBlogCategoryPosts(
    handle: string,
    categorySlug: string,
    signal?: AbortSignal,
  ): Promise<FeedResponse> {
    return this.#request(
      `/api/v1/blogs/${encodeURIComponent(handle)}/categories/${encodeURIComponent(categorySlug)}/posts`,
      withSignal(signal),
    );
  }

  async getBlogCategoryPost(
    handle: string,
    categorySlug: string,
    postSlug: string,
    view: ViewMode = "intent",
    signal?: AbortSignal,
  ): Promise<BlogPostView> {
    return this.#request(
      `/api/v1/blogs/${encodeURIComponent(handle)}/categories/${encodeURIComponent(categorySlug)}/posts/${encodeURIComponent(postSlug)}?view=${view}`,
      withSignal(signal),
    );
  }

  /** Categories owned by the configured on-premises primary site. */
  async listPrimaryCategories(signal?: AbortSignal): Promise<CategoryListResponse> {
    return this.#request("/api/v1/primary/categories", withSignal(signal));
  }

  async getPrimaryCategory(
    categorySlug: string,
    signal?: AbortSignal,
  ): Promise<BlogCategoryResponse> {
    return this.#request(
      `/api/v1/primary/categories/${encodeURIComponent(categorySlug)}`,
      withSignal(signal),
    );
  }

  async getPrimaryCategoryPosts(
    categorySlug: string,
    signal?: AbortSignal,
  ): Promise<FeedResponse> {
    return this.#request(
      `/api/v1/primary/categories/${encodeURIComponent(categorySlug)}/posts`,
      withSignal(signal),
    );
  }

  async getPrimaryCategoryPost(
    categorySlug: string,
    postSlug: string,
    view: ViewMode = "intent",
    signal?: AbortSignal,
  ): Promise<BlogPostView> {
    return this.#request(
      `/api/v1/primary/categories/${encodeURIComponent(categorySlug)}/posts/${encodeURIComponent(postSlug)}?view=${view}`,
      withSignal(signal),
    );
  }

  async getBlogPost(
    handle: string,
    slug: string,
    view: ViewMode = "intent",
    signal?: AbortSignal,
  ): Promise<BlogPostView> {
    return this.#request(
      `/api/v1/blogs/${encodeURIComponent(handle)}/posts/${encodeURIComponent(slug)}?view=${view}`,
      withSignal(signal),
    );
  }

  async listStudioDocuments(signal?: AbortSignal): Promise<DocumentSnapshot[]> {
    return this.#request("/api/v1/studio/documents", withSignal(signal));
  }

  async getStudioDocument(
    documentId: string,
    signal?: AbortSignal,
  ): Promise<DocumentSnapshot> {
    return this.#request(
      `/api/v1/studio/documents/${encodeURIComponent(documentId)}`,
      withSignal(signal),
    );
  }

  async listStudioCategories(signal?: AbortSignal): Promise<CategoryListResponse> {
    return this.#request("/api/v1/studio/categories", withSignal(signal));
  }

  async createStudioCategory(
    input: CreateCategoryInput,
    signal?: AbortSignal,
  ): Promise<CategorySummary> {
    return this.#request("/api/v1/studio/categories", {
      method: "POST",
      body: JSON.stringify(input),
      ...withSignal(signal),
    });
  }

  async updateStudioCategory(
    categoryId: string,
    input: UpdateCategoryInput,
    signal?: AbortSignal,
  ): Promise<CategorySummary> {
    return this.#request(
      `/api/v1/studio/categories/${encodeURIComponent(categoryId)}`,
      {
        method: "PUT",
        body: JSON.stringify(input),
        ...withSignal(signal),
      },
    );
  }

  async archiveStudioCategory(
    categoryId: string,
    signal?: AbortSignal,
  ): Promise<CategorySummary> {
    return this.#request(
      `/api/v1/studio/categories/${encodeURIComponent(categoryId)}/archive`,
      {
        method: "POST",
        ...withSignal(signal),
      },
    );
  }

  async createStudioDocument(
    input: CreatePostInput,
    signal?: AbortSignal,
  ): Promise<DocumentSnapshot> {
    return this.#request("/api/v1/studio/documents", {
      method: "POST",
      body: JSON.stringify(input),
      ...withSignal(signal),
    });
  }

  async createStudioRevision(
    documentId: string,
    input: ProposeRevisionInput,
    signal?: AbortSignal,
  ): Promise<DocumentSnapshot> {
    return this.#request(
      `/api/v1/studio/documents/${encodeURIComponent(documentId)}/revisions`,
      {
        method: "POST",
        body: JSON.stringify(input),
        ...withSignal(signal),
      },
    );
  }

  async publishStudioDocument(
    documentId: string,
    revisionId: string,
    signal?: AbortSignal,
  ): Promise<DocumentSnapshot> {
    return this.#request(
      `/api/v1/studio/documents/${encodeURIComponent(documentId)}/publish`,
      {
        method: "POST",
        body: JSON.stringify({ revisionId }),
        ...withSignal(signal),
      },
    );
  }

  async previewStudio(
    input: CreatePostInput,
    signal?: AbortSignal,
  ): Promise<StudioPreviewResponse> {
    return this.#request("/api/v1/studio/preview", {
      method: "POST",
      body: JSON.stringify(input),
      ...withSignal(signal),
    });
  }

  async aiSummaryProviders(signal?: AbortSignal): Promise<AiSummaryProvidersResponse> {
    return this.#request("/api/v1/studio/ai-summary/providers", withSignal(signal));
  }

  /**
   * Generates one candidate with a one-shot provider key. The key is placed in
   * a dedicated request header and is never part of the serializable post or
   * draft input types.
   */
  async generateAiSummary(
    input: GenerateAiSummaryInput,
    oneShotApiKey: string,
    signal?: AbortSignal,
  ): Promise<GenerateAiSummaryResponse> {
    return this.#request("/api/v1/studio/ai-summary/generate", {
      method: "POST",
      redirect: "error",
      body: JSON.stringify(input),
      headers: {
        "X-OSB-AI-One-Shot-Key": oneShotApiKey,
        "Cache-Control": "no-store",
        "Pragma": "no-cache",
      },
      ...withSignal(signal),
    });
  }

  async uploadStudioAsset(
    bytes: Blob,
    filename: string,
    signal?: AbortSignal,
  ): Promise<AssetUploadResponse> {
    return this.#request("/api/v1/studio/assets", {
      method: "POST",
      body: bytes,
      headers: {
        "Content-Type": bytes.type || "application/octet-stream",
        "X-OSB-Filename": filename,
      },
      ...withSignal(signal),
    });
  }

  async getStudioSettings(signal?: AbortSignal): Promise<StudioSettings> {
    return this.#request("/api/v1/studio/settings", withSignal(signal));
  }

  async updateStudioSettings(
    input: UpdateStudioSettingsInput,
    signal?: AbortSignal,
  ): Promise<StudioSettings> {
    return this.#request("/api/v1/studio/settings", {
      method: "PUT",
      body: JSON.stringify(input),
      ...withSignal(signal),
    });
  }

  async listStudioCollaborators(
    signal?: AbortSignal,
  ): Promise<CollaboratorListResponse> {
    return this.#request("/api/v1/studio/collaborators", withSignal(signal));
  }

  async addStudioCollaborator(
    input: AddCollaboratorInput,
    signal?: AbortSignal,
  ): Promise<Collaborator> {
    return this.#request("/api/v1/studio/collaborators", {
      method: "POST",
      body: JSON.stringify(input),
      ...withSignal(signal),
    });
  }

  async removeStudioCollaborator(
    userId: string,
    signal?: AbortSignal,
  ): Promise<Collaborator> {
    return this.#request(
      `/api/v1/studio/collaborators/${encodeURIComponent(userId)}`,
      {
        method: "DELETE",
        ...withSignal(signal),
      },
    );
  }

  async listComments(postId: string, signal?: AbortSignal): Promise<CommentListResponse> {
    return this.#request(
      `/api/v1/posts/${encodeURIComponent(postId)}/comments`,
      withSignal(signal),
    );
  }

  async createComment(
    postId: string,
    input: CreateCommentInput,
    signal?: AbortSignal,
  ): Promise<CommentView> {
    return this.#request(`/api/v1/posts/${encodeURIComponent(postId)}/comments`, {
      method: "POST",
      body: JSON.stringify(input),
      ...withSignal(signal),
    });
  }

  async listAdminDocuments(signal?: AbortSignal): Promise<DocumentSnapshot[]> {
    return this.#request("/api/v1/admin/documents", withSignal(signal));
  }

  async adminTree(
    query: AdminTreeQuery = {},
    signal?: AbortSignal,
  ): Promise<AdminTreePage> {
    const path = adminTreePath(query);
    const page = parseAdminTreePage(await this.#request<unknown>(path, {
      headers: {
        "Cache-Control": "no-store",
        "Pragma": "no-cache",
      },
      ...withSignal(signal),
    }));
    const requestedParent = query.parent ?? "root";
    if (page.parentId !== requestedParent) {
      throw new TypeError("administrator tree response parent does not match the request");
    }
    return page;
  }

  async getAdminDocument(
    documentId: string,
    signal?: AbortSignal,
  ): Promise<DocumentSnapshot> {
    return this.#request(
      `/api/v1/admin/documents/${encodeURIComponent(documentId)}`,
      withSignal(signal),
    );
  }

  async listAdminRevisions(
    documentId: string,
    signal?: AbortSignal,
  ): Promise<RevisionSnapshot[]> {
    return this.#request(
      `/api/v1/admin/documents/${encodeURIComponent(documentId)}/revisions`,
      withSignal(signal),
    );
  }

  async getPost(
    slug: string,
    view: ViewMode = "intent",
    signal?: AbortSignal,
  ): Promise<PostView> {
    const path = `/api/v1/posts/${encodeURIComponent(slug)}?view=${view}`;
    return this.#request(path, withSignal(signal));
  }

  async createPost(input: CreatePostInput, signal?: AbortSignal): Promise<DocumentSnapshot> {
    return this.#request(
      "/api/v1/posts",
      {
        method: "POST",
        body: JSON.stringify(input),
        ...withSignal(signal),
      },
    );
  }

  async proposeRevision(
    documentId: string,
    input: ProposeRevisionInput,
    signal?: AbortSignal,
  ): Promise<RevisionSnapshot> {
    return this.#request(
      `/api/v1/documents/${encodeURIComponent(documentId)}/revisions`,
      {
        method: "POST",
        body: JSON.stringify(input),
        ...withSignal(signal),
      },
    );
  }

  async publish(
    documentId: string,
    revisionId: string,
    signal?: AbortSignal,
  ): Promise<DocumentSnapshot> {
    return this.#request(
      `/api/v1/documents/${encodeURIComponent(documentId)}/publish`,
      {
        method: "POST",
        body: JSON.stringify({ revisionId }),
        ...withSignal(signal),
      },
    );
  }

  async uploadAsset(
    bytes: Blob,
    filename: string,
    signal?: AbortSignal,
  ): Promise<AssetUploadResponse> {
    return this.#request(
      "/api/v1/assets",
      {
        method: "POST",
        body: bytes,
        headers: {
          "Content-Type": bytes.type || "application/octet-stream",
          "X-OSB-Filename": filename,
        },
        ...withSignal(signal),
      },
    );
  }

  async #request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set("Accept", "application/json");
    if (init.body !== undefined && !headers.has("Content-Type")) {
      headers.set("Content-Type", "application/json");
    }
    const response = await this.#fetch(`${this.#baseUrl}${path}`, {
      ...init,
      headers,
      credentials: "same-origin",
    });
    if (!response.ok) {
      const payload = (await response.json().catch(() => null)) as
        | { message?: string }
        | null;
      throw new OpenSoverignBlogError(
        payload?.message ?? `OpenSoverignBlog request failed (${response.status})`,
        response.status,
      );
    }
    return (await response.json()) as T;
  }

  async #requestText(path: string, signal?: AbortSignal): Promise<string> {
    const response = await this.#fetch(`${this.#baseUrl}${path}`, {
      headers: { Accept: "text/markdown" },
      credentials: "same-origin",
      ...withSignal(signal),
    });
    if (!response.ok) {
      throw new OpenSoverignBlogError(
        `OpenSoverignBlog request failed (${response.status})`,
        response.status,
      );
    }
    return response.text();
  }
}

function withSignal(signal: AbortSignal | undefined): Pick<RequestInit, "signal"> {
  return signal ? { signal } : {};
}

function adminTreePath(query: AdminTreeQuery): string {
  const parameters = new URLSearchParams();
  if (query.parent !== undefined) parameters.set("parent", query.parent);
  if (query.cursor !== undefined) parameters.set("cursor", query.cursor);
  if (query.limit !== undefined) {
    if (!Number.isInteger(query.limit) || query.limit < 1 || query.limit > 200) {
      throw new TypeError("administrator tree limit must be an integer between 1 and 200");
    }
    parameters.set("limit", String(query.limit));
  }
  const search = parameters.toString();
  return `/api/v1/admin/tree${search ? `?${search}` : ""}`;
}

function parseAdminTreePage(value: unknown): AdminTreePage {
  const page = requireRecord(value, "administrator tree response");
  if (page.schemaVersion !== ADMIN_TREE_SCHEMA_VERSION) {
    throw new TypeError("administrator tree response has an unsupported schema version");
  }
  const generatedAt = requireString(page.generatedAt, "administrator tree generatedAt");
  const parentId = requireString(page.parentId, "administrator tree parentId");
  if (!Array.isArray(page.items) || page.items.length > 200) {
    throw new TypeError("administrator tree response items are invalid");
  }
  const items = page.items.map(parseAdminTreeNode);
  const nextCursor = optionalString(page, "nextCursor", "administrator tree nextCursor");
  return {
    schemaVersion: ADMIN_TREE_SCHEMA_VERSION,
    generatedAt,
    parentId,
    items,
    ...(nextCursor !== undefined ? { nextCursor } : {}),
  };
}

function parseAdminTreeNode(value: unknown): AdminTreeNode {
  const candidate = requireRecord(value, "administrator tree node");
  const kind = requireString(candidate.kind, "administrator tree node kind");
  if (!isAdminTreeNodeKind(kind)) {
    throw new TypeError("administrator tree node kind is unsupported");
  }
  if (typeof candidate.hasChildren !== "boolean") {
    throw new TypeError("administrator tree node hasChildren is invalid");
  }
  const revisionNumber = optionalNumber(
    candidate,
    "revisionNumber",
    "administrator tree revisionNumber",
  );
  if (revisionNumber !== undefined && (!Number.isSafeInteger(revisionNumber) || revisionNumber < 0)) {
    throw new TypeError("administrator tree revisionNumber is invalid");
  }
  const requested = optionalBoolean(candidate, "requested", "administrator tree requested");
  const operational = optionalBoolean(
    candidate,
    "operational",
    "administrator tree operational",
  );
  const node = {
    id: requireString(candidate.id, "administrator tree node id"),
    parentId: requireString(candidate.parentId, "administrator tree node parentId"),
    kind,
    label: requireString(candidate.label, "administrator tree node label"),
    hasChildren: candidate.hasChildren,
    ...copyOptionalString(candidate, "entityId"),
    ...copyOptionalString(candidate, "handle"),
    ...copyOptionalString(candidate, "slug"),
    ...copyOptionalString(candidate, "state"),
    ...(revisionNumber !== undefined ? { revisionNumber } : {}),
    ...(requested !== undefined ? { requested } : {}),
    ...(operational !== undefined ? { operational } : {}),
    ...copyOptionalString(candidate, "summary"),
    ...copyOptionalString(candidate, "createdAt"),
    ...copyOptionalString(candidate, "updatedAt"),
  };
  return node as AdminTreeNode;
}

function isAdminTreeNodeKind(value: string): value is AdminTreeNodeKind {
  return [
    "group",
    "site",
    "category",
    "document",
    "revision",
    "setting",
    "module",
    "runtime",
  ].includes(value);
}

function requireRecord(value: unknown, name: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TypeError(`${name} must be an object`);
  }
  return value as Record<string, unknown>;
}

function requireString(value: unknown, name: string): string {
  if (typeof value !== "string") throw new TypeError(`${name} must be a string`);
  return value;
}

function optionalString(
  value: Record<string, unknown>,
  key: string,
  name: string,
): string | undefined {
  const candidate = value[key];
  if (candidate === undefined) return undefined;
  return requireString(candidate, name);
}

function copyOptionalString<Key extends string>(
  value: Record<string, unknown>,
  key: Key,
): { [Property in Key]?: string } {
  const candidate = optionalString(value, key, `administrator tree ${key}`);
  return (candidate === undefined ? {} : { [key]: candidate }) as {
    [Property in Key]?: string;
  };
}

function optionalNumber(
  value: Record<string, unknown>,
  key: string,
  name: string,
): number | undefined {
  const candidate = value[key];
  if (candidate === undefined) return undefined;
  if (typeof candidate !== "number") throw new TypeError(`${name} must be a number`);
  return candidate;
}

function optionalBoolean(
  value: Record<string, unknown>,
  key: string,
  name: string,
): boolean | undefined {
  const candidate = value[key];
  if (candidate === undefined) return undefined;
  if (typeof candidate !== "boolean") throw new TypeError(`${name} must be a boolean`);
  return candidate;
}

function validateAdminAuthActionHref(value: string): string {
  if (
    typeof value !== "string"
    || !value.startsWith("/")
    || value.startsWith("//")
    || value.includes("\\")
    || value.includes("#")
  ) {
    throw new TypeError("administrator authentication action must be a root-relative API path");
  }
  const parsed = new URL(value, "https://open-soverign-blog.invalid");
  if (
    parsed.origin !== "https://open-soverign-blog.invalid"
    || !parsed.pathname.startsWith("/api/v1/auth/")
    || parsed.pathname.split("/").some((segment) => segment === "." || segment === "..")
  ) {
    throw new TypeError("administrator authentication action must target /api/v1/auth/");
  }
  return `${parsed.pathname}${parsed.search}`;
}
