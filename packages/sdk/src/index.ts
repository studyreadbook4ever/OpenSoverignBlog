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
  slug: string;
  updatedAt: string;
  hasIntentView: boolean;
  hasOntology: boolean;
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
  createdAt?: string;
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
  coverImageUrl?: string;
}

export interface FeedResponse {
  items: FeedPostSummary[];
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
}

export interface BlogPostView extends PostView {
  slug: string;
  excerpt?: string;
  publishedAt: string;
  updatedAt: string;
  author: UserSummary;
  blog: BlogSummary;
  tags: string[];
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
  mutationMechanisms: Array<"session" | "owner_token">;
  mutationMode: "read_only" | "single_owner_token" | "authenticated_members";
  /** Present on the v2 capability contract. Public reads remain anonymous in every profile. */
  publicAccess?: "anonymous_read";
  /**
   * Describes Studio authorization independently from the legacy mutation
   * transport. Older v1 servers omit this field.
   */
  studioAccess?: StudioAccess;
  /** Operational administrator authentication methods advertised by a v2 server. */
  auth?: AdminAuthCapabilities;
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

export interface DiscoveryCacheDependency {
  provider: "redis";
  state: "active" | "degraded" | "connecting" | "misconfigured";
  role?: "discardable_public_derivative_cache";
  required?: boolean;
  topology?: "standalone" | "sentinel";
  namespace?: string;
  contentRelease?: string;
  hits?: number;
  misses?: number;
  errors?: number;
  lastSuccessUnix?: number | null;
  lastError?: string | null;
}

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

export interface ClientOptions {
  baseUrl?: string;
  getAdminToken?: () => string | undefined;
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
  readonly #getAdminToken: (() => string | undefined) | undefined;

  constructor(options: ClientOptions = {}) {
    this.#baseUrl = (options.baseUrl ?? "").replace(/\/$/, "");
    this.#fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
    this.#getAdminToken = options.getAdminToken;
  }

  async discovery(signal?: AbortSignal): Promise<DiscoveryDocument> {
    return this.#request(
      "/.well-known/open-soverign-blog.json",
      withSignal(signal),
    );
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
      true,
    );
  }

  async pollCodeRun(jobId: string, signal?: AbortSignal): Promise<CodeRunResponse> {
    return this.#request(
      `/api/v1/code-runner/runs/${encodeURIComponent(jobId)}`,
      withSignal(signal),
      true,
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
    return this.#request("/api/v1/admin/documents", withSignal(signal), true);
  }

  async getAdminDocument(
    documentId: string,
    signal?: AbortSignal,
  ): Promise<DocumentSnapshot> {
    return this.#request(
      `/api/v1/admin/documents/${encodeURIComponent(documentId)}`,
      withSignal(signal),
      true,
    );
  }

  async listAdminRevisions(
    documentId: string,
    signal?: AbortSignal,
  ): Promise<RevisionSnapshot[]> {
    return this.#request(
      `/api/v1/admin/documents/${encodeURIComponent(documentId)}/revisions`,
      withSignal(signal),
      true,
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
      true,
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
      true,
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
      true,
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
      true,
    );
  }

  async #request<T>(path: string, init: RequestInit = {}, authenticated = false): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set("Accept", "application/json");
    if (init.body !== undefined && !headers.has("Content-Type")) {
      headers.set("Content-Type", "application/json");
    }
    if (authenticated) {
      const token = this.#getAdminToken?.();
      if (token) headers.set("Authorization", `Bearer ${token}`);
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
