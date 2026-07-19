import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type FormEvent,
} from "react";
import DOMPurify from "dompurify";
import type {
  AdminAccessKeyMethod,
  BlogPostView,
  BlogSummary,
  Capabilities,
  CodeRunResponse,
  CommentView,
  FeedPostSummary,
  PostSummary,
  PostView,
  RunnerProfile,
  ThemePresetId,
  ViewMode,
} from "@opensoverignblog/sdk";
import { useSession } from "./app";
import {
  adminAuthChoices,
  isLegacyOwnerBearerMode,
  safeAuthActionHref,
  studioAccessFor,
} from "./auth-policy";
import { safeBlogStylesheetUrl } from "./site-stylesheet";
import {
  AppLink,
  THEME_PRESETS,
  asMessage,
  basePath,
  client,
  formatDate,
  initials,
  isNotFound,
  navigate,
  publicPath,
  usePageTitle,
} from "./lib";

const LEGACY_USER = {
  id: "legacy-owner",
  handle: "open-soverign",
  displayName: "OpenSoverignBlog",
};

const LEGACY_BLOG: BlogSummary = {
  id: "legacy-site",
  handle: "open-soverign",
  title: "OpenSoverign Notes",
  description: "Portable Markdown and immutable ideas.",
  owner: LEGACY_USER,
  theme: { presetId: "paper" },
};

export function FeedPage() {
  const { session, capabilities } = useSession();
  usePageTitle("피드");
  const [items, setItems] = useState<FeedPostSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();

  useEffect(() => {
    const controller = new AbortController();
    void loadFeed(controller.signal)
      .then(setItems)
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setError(asMessage(reason));
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, []);

  return (
    <div className="feed-page">
      <section className="feed-hero" aria-labelledby="feed-title">
        <div>
          <p className="eyebrow">Independent publishing network</p>
          <h1 id="feed-title">좋은 글이 머무는<br />조용한 광장</h1>
        </div>
        <p className="feed-hero-copy">
          여러 사람이 각자의 목소리와 테마로 발행한 글을 한곳에서 발견하세요.
          원문 Markdown은 언제나 작성자에게 남습니다.
        </p>
      </section>

      <div className="section-heading">
        <div>
          <p className="eyebrow">Latest stories</p>
          <h2>새로 도착한 글</h2>
        </div>
        <span className="result-count">{items.length}개의 글</span>
      </div>

      {loading ? <FeedSkeleton /> : null}
      {error ? <StatusMessage title="피드를 불러오지 못했습니다" detail={error} /> : null}
      {!loading && !error && items.length === 0 ? (
        <EmptyState
          {...(!capabilities || studioAccessFor(capabilities) === "disabled" ? {} : {
            actionHref: session?.state === "authenticated" ? (session.blog ? "/studio/write" : "/onboarding") : "/login",
            actionLabel: session?.state === "authenticated"
              ? (session.blog ? "첫 글 쓰기" : "블로그 만들기")
              : studioAccessFor(capabilities) === "admin_only" ? "관리자로 시작하기" : "로그인하고 시작하기",
          })}
          description="아직 발행된 글이 없습니다. 작은 메모부터 시작해 보세요."
          title="첫 이야기를 기다리고 있어요"
        />
      ) : null}
      {items.length > 0 ? (
        <div className="post-grid" aria-label="공개 글 목록">
          {items.map((post) => <PostCard key={post.id} post={post} />)}
        </div>
      ) : null}
    </div>
  );
}

async function loadFeed(signal?: AbortSignal): Promise<FeedPostSummary[]> {
  try {
    return (await client.feed(signal)).items;
  } catch (reason) {
    if (!isNotFound(reason)) throw reason;
    return (await client.listPosts(signal)).map(legacyFeedPost);
  }
}

function legacyFeedPost(post: PostSummary): FeedPostSummary {
  return {
    id: post.id,
    title: post.title,
    slug: post.slug,
    excerpt: post.hasIntentView
      ? "작성자의 의도와 portable Markdown을 함께 보존한 글입니다."
      : "언제든 내보낼 수 있는 portable Markdown 글입니다.",
    publishedAt: post.updatedAt,
    updatedAt: post.updatedAt,
    author: LEGACY_USER,
    blog: LEGACY_BLOG,
    tags: [],
    commentCount: 0,
    hasIntentView: post.hasIntentView,
  };
}

function PostCard({ post }: { post: FeedPostSummary }) {
  const href = `/@${encodeURIComponent(post.blog.handle)}/${encodeURIComponent(post.slug)}`;
  return (
    <article className="post-card">
      <AppLink aria-label={`${post.title} 글 표지`} className="post-card-visual" data-theme={post.blog.theme.presetId} href={href} tabIndex={-1}>
        {post.coverImageUrl ? (
          <img alt="" src={post.coverImageUrl} />
        ) : (
          <span aria-hidden="true">{post.title.slice(0, 1)}</span>
        )}
      </AppLink>
      <div className="post-card-body">
        <div className="post-card-meta">
          <AppLink href={`/@${encodeURIComponent(post.blog.handle)}`}>@{post.blog.handle}</AppLink>
          <time dateTime={post.publishedAt}>{formatDate(post.publishedAt)}</time>
        </div>
        <h3><AppLink href={href}>{post.title}</AppLink></h3>
        <p>{post.excerpt}</p>
        <div className="post-card-footer">
          <span className="author-inline">
            <span className="avatar avatar-tiny" aria-hidden="true">{initials(post.author.displayName)}</span>
            {post.author.displayName}
          </span>
          <span>댓글 {post.commentCount}</span>
        </div>
      </div>
    </article>
  );
}

export function BlogPage({ handle }: { handle: string }) {
  const [blog, setBlog] = useState<BlogSummary>();
  const [posts, setPosts] = useState<FeedPostSummary[]>([]);
  const [error, setError] = useState<string>();
  usePageTitle(blog?.title ?? `@${handle}`);

  useEffect(() => {
    const controller = new AbortController();
    setBlog(undefined);
    setPosts([]);
    setError(undefined);
    void loadBlogPage(handle, controller.signal)
      .then(([nextBlog, nextPosts]) => {
        if (controller.signal.aborted) return;
        setBlog(nextBlog);
        setPosts(nextPosts);
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setError(asMessage(reason));
      });
    return () => controller.abort();
  }, [handle]);

  if (error) return <StatusMessage title="블로그를 찾을 수 없습니다" detail={error} />;
  if (!blog) return <PageLoading label="블로그를 불러오는 중" />;
  return (
    <>
      <BlogCustomStylesheet handle={blog.handle} href={blog.theme.customCssUrl} />
      <div className="osb-site-frame">
        <div className="blog-page osb-site-theme" data-custom-css={blog.theme.customCssUrl ? "enabled" : "disabled"} data-site-id={blog.id} data-theme={blog.theme.presetId}>
          <section className="blog-profile">
        <span className="blog-monogram" aria-hidden="true">{initials(blog.title)}</span>
        <div>
          <p className="blog-handle">@{blog.handle}</p>
          <h1>{blog.title}</h1>
          <p>{blog.description || "아직 블로그 소개가 없습니다."}</p>
          <div className="blog-owner">
            <span className="avatar" aria-hidden="true">{initials(blog.owner.displayName)}</span>
            <span><strong>{blog.owner.displayName}</strong><small>이 블로그의 작성자</small></span>
          </div>
        </div>
          </section>
          <section className="blog-posts" aria-labelledby="blog-posts-title">
        <div className="section-heading">
          <div><p className="eyebrow">Archive</p><h2 id="blog-posts-title">모든 글</h2></div>
          <span className="result-count">{posts.length}개</span>
        </div>
        {posts.length ? (
          <div className="blog-list">
            {posts.map((post, index) => (
              <article className="blog-list-item" key={post.id}>
                <span className="post-order" aria-hidden="true">{String(index + 1).padStart(2, "0")}</span>
                <div>
                  <div className="post-card-meta"><time dateTime={post.publishedAt}>{formatDate(post.publishedAt)}</time></div>
                  <h3><AppLink href={`/@${encodeURIComponent(handle)}/${encodeURIComponent(post.slug)}`}>{post.title}</AppLink></h3>
                  <p>{post.excerpt}</p>
                </div>
                <span className="list-arrow" aria-hidden="true">↗</span>
              </article>
            ))}
          </div>
        ) : (
          <EmptyState description="아직 발행된 글이 없습니다." title="빈 서가입니다" />
        )}
          </section>
        </div>
      </div>
    </>
  );
}

export function ArticlePage({
  handle,
  slug,
  capabilities,
  legacy = false,
}: {
  handle: string;
  slug: string;
  capabilities: Capabilities | undefined;
  legacy?: boolean;
}) {
  const [view, setView] = useState<ViewMode>("intent");
  const [post, setPost] = useState<BlogPostView>();
  const [error, setError] = useState<string>();
  const legacyArticle = legacy || (
    capabilities !== undefined
    && isLegacyOwnerBearerMode(capabilities)
    && handle === LEGACY_BLOG.handle
  );
  usePageTitle(post?.title ?? "글 읽기");

  useEffect(() => {
    const controller = new AbortController();
    setPost(undefined);
    setError(undefined);
    void loadArticle(
      handle,
      slug,
      view,
      legacyArticle,
      controller.signal,
    )
      .then((value) => {
        if (value.requestedSlug !== value.canonicalSlug) {
          navigate(
            legacyArticle
              ? `/blog/${encodeURIComponent(value.canonicalSlug)}`
              : `/@${encodeURIComponent(handle)}/${encodeURIComponent(value.canonicalSlug)}`,
            true,
          );
          return;
        }
        setPost(value);
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setError(asMessage(reason));
      });
    return () => controller.abort();
  }, [handle, slug, view, legacyArticle]);

  if (error) return <StatusMessage title="글을 불러오지 못했습니다" detail={error} />;
  if (!post) return <PageLoading label="글을 불러오는 중" />;
  return (
    <>
      <BlogCustomStylesheet handle={post.blog.handle} href={post.blog.theme.customCssUrl} />
      <div className="osb-site-frame">
        <div className="article-page osb-site-theme" data-custom-css={post.blog.theme.customCssUrl ? "enabled" : "disabled"} data-site-id={post.blog.id} data-theme={post.blog.theme.presetId}>
          <article className="article-shell">
        <header className="article-header">
          <div className="article-kicker">
            <AppLink href={`/@${post.blog.handle}`}>@{post.blog.handle}</AppLink>
            <span aria-hidden="true">/</span>
            <time dateTime={post.publishedAt}>{formatDate(post.publishedAt)}</time>
          </div>
          <h1>{post.title}</h1>
          {post.excerpt ? <p className="article-deck">{post.excerpt}</p> : null}
          <div className="article-author-row">
            <span className="avatar" aria-hidden="true">{initials(post.author.displayName)}</span>
            <div><strong>{post.author.displayName}</strong><span>글쓴이</span></div>
            {post.tags.length ? (
              <ul className="tag-list" aria-label="태그">
                {post.tags.map((tag) => <li key={tag}>#{tag}</li>)}
              </ul>
            ) : null}
          </div>
          <div className="projection-switcher" role="group" aria-label="콘텐츠 보기 방식">
            <button aria-pressed={view === "intent"} onClick={() => setView("intent")} type="button">작성자 보기</button>
            <button aria-pressed={view === "markdown_source"} onClick={() => setView("markdown_source")} type="button">.md 원문</button>
          </div>
        </header>
        <ArticleBody capabilities={capabilities} html={post.artifact.html} />
        <details className="artifact-proof">
          <summary>문서 무결성 정보</summary>
          <div><span>{post.artifact.rendererVersion}</span><code>{post.artifact.artifactHash}</code></div>
        </details>
          </article>
          <CommentSection postId={post.id} />
        </div>
      </div>
    </>
  );
}

function BlogCustomStylesheet({ handle, href }: { handle: string; href: string | undefined }) {
  const safeHref = safeBlogStylesheetUrl(href, handle, window.location.origin, basePath);
  return safeHref ? <link data-osb-blog-custom-css href={safeHref} rel="stylesheet" /> : null;
}

async function loadArticle(
  handle: string,
  slug: string,
  view: ViewMode,
  legacy: boolean,
  signal: AbortSignal,
): Promise<BlogPostView> {
  return legacy
    ? legacyPostView(await client.getPost(slug, view, signal))
    : client.getBlogPost(handle, slug, view, signal);
}

async function loadBlogPosts(handle: string, signal: AbortSignal): Promise<FeedPostSummary[]> {
  try {
    return (await client.getBlogPosts(handle, signal)).items;
  } catch (reason) {
    if (!isNotFound(reason) || handle !== LEGACY_BLOG.handle) throw reason;
    return (await loadFeed(signal)).filter((post) => post.blog.handle === handle);
  }
}

async function loadBlogPage(
  handle: string,
  signal: AbortSignal,
): Promise<[BlogSummary, FeedPostSummary[]]> {
  try {
    return await Promise.all([
      client.getBlog(handle, signal),
      loadBlogPosts(handle, signal),
    ]);
  } catch (reason) {
    if (!isNotFound(reason) || handle !== LEGACY_BLOG.handle) throw reason;
    const posts = (await loadFeed(signal)).filter((post) => post.blog.handle === handle);
    return [posts[0]?.blog ?? LEGACY_BLOG, posts];
  }
}

function legacyPostView(post: PostView): BlogPostView {
  const now = new Date().toISOString();
  return {
    ...post,
    slug: post.canonicalSlug,
    publishedAt: now,
    updatedAt: now,
    author: LEGACY_USER,
    blog: LEGACY_BLOG,
    tags: [],
  };
}

function ArticleBody({ html, capabilities }: { html: string; capabilities: Capabilities | undefined }) {
  const { session } = useSession();
  const bodyRef = useRef<HTMLDivElement>(null);
  const [runnerProfiles, setRunnerProfiles] = useState<RunnerProfile[]>([]);
  const sanitizedHtml = useMemo(
    () => DOMPurify.sanitize(html, {
      USE_PROFILES: { html: true },
      FORBID_TAGS: ["iframe", "object", "embed", "script", "style", "svg"],
      FORBID_ATTR: ["style"],
    }),
    [html],
  );

  useEffect(() => {
    if (
      !capabilities?.features.includes("code_runner")
      || studioAccessFor(capabilities) !== "admin_only"
      || session?.state !== "authenticated"
    ) {
      setRunnerProfiles([]);
      return;
    }
    const controller = new AbortController();
    void client.codeRunnerProfiles(controller.signal).then(setRunnerProfiles).catch(() => setRunnerProfiles([]));
    return () => controller.abort();
  }, [capabilities, session]);

  useEffect(() => {
    const body = bodyRef.current;
    if (!body) return;
    const controller = new AbortController();
    const mathNodes = Array.from(body.querySelectorAll<HTMLElement>(".osb-math"));
    if (mathNodes.length) {
      void import("katex").then(({ default: katex }) => {
        if (controller.signal.aborted) return;
        mathNodes.forEach((node) => {
          katex.render(node.textContent ?? "", node, {
            displayMode: node.classList.contains("osb-math-display"),
            throwOnError: false,
            strict: "warn",
            trust: false,
          });
        });
      });
    }
    if (!capabilities?.features.includes("code_runner")) return () => controller.abort();
    body.querySelectorAll("pre > code[class*='language-']").forEach((code) => {
      const profile = runnerProfiles.find((candidate) =>
        candidate.outputMode === "console" &&
        candidate.fenceAliases.some((alias) => code.classList.contains(`language-${alias}`)),
      );
      if (!profile) return;
      const pre = code.parentElement;
      if (!pre || pre.previousElementSibling?.classList.contains("run-code")) return;
      const button = document.createElement("button");
      button.type = "button";
      button.className = "run-code";
      button.textContent = `${profile.id}로 실행`;
      const output = document.createElement("pre");
      output.className = "run-code-output";
      output.setAttribute("aria-live", "polite");
      output.hidden = true;
      button.addEventListener("click", async () => {
        button.disabled = true;
        output.hidden = false;
        output.textContent = "격리 실행기에 제출하는 중…";
        try {
          let result = await client.submitCodeRun(profile.id, code.textContent ?? "", controller.signal);
          let polls = 0;
          while (result.state === "queued" && polls < 120) {
            output.textContent = `대기열 ${result.jobId.slice(0, 8)}…`;
            await abortableDelay(Math.min(Math.max(result.pollAfterMs, 250), 5_000), controller.signal);
            result = await client.pollCodeRun(result.jobId, controller.signal);
            polls += 1;
          }
          output.textContent = formatCodeRun(result);
        } catch (reason) {
          if (!controller.signal.aborted) output.textContent = asMessage(reason);
        } finally {
          button.disabled = false;
        }
      });
      pre.before(button);
      pre.after(output);
    });
    return () => controller.abort();
  }, [sanitizedHtml, capabilities, runnerProfiles]);

  return <div className="article-content" ref={bodyRef} dangerouslySetInnerHTML={{ __html: sanitizedHtml }} />;
}

function CommentSection({ postId }: { postId: string }) {
  const { session, capabilities } = useSession();
  const [comments, setComments] = useState<CommentView[]>([]);
  const [available, setAvailable] = useState(false);
  const [body, setBody] = useState("");
  const [status, setStatus] = useState<string>();
  const [submitting, setSubmitting] = useState(false);
  const commentsWritable = capabilities !== undefined
    && studioAccessFor(capabilities) === "members";

  useEffect(() => {
    const controller = new AbortController();
    void client.listComments(postId, controller.signal)
      .then((value) => {
        setComments(value.items);
        setAvailable(true);
      })
      .catch((reason: unknown) => {
        if (!isNotFound(reason) && !controller.signal.aborted) {
          setAvailable(true);
          setStatus(asMessage(reason));
        }
      });
    return () => controller.abort();
  }, [postId]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!body.trim()) return;
    setSubmitting(true);
    setStatus("댓글을 등록하는 중…");
    try {
      const created = await client.createComment(postId, { sourceMarkdown: body.trim() });
      setComments((current) => [...current, created]);
      setBody("");
      setStatus("댓글이 등록되었습니다.");
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      setSubmitting(false);
    }
  }

  if (!available) return null;
  return (
    <section className="comments-section" aria-labelledby="comments-title">
      <div className="section-heading">
        <div><p className="eyebrow">Conversation</p><h2 id="comments-title">댓글 {comments.length}</h2></div>
      </div>
      {session?.state === "authenticated" && commentsWritable ? (
        <form className="comment-form" onSubmit={(event) => void submit(event)}>
          <label htmlFor="comment-body">댓글 남기기</label>
          <textarea
            id="comment-body"
            maxLength={20_000}
            onChange={(event) => setBody(event.target.value)}
            placeholder="Markdown으로 생각을 나눠주세요."
            required
            value={body}
          />
          <div><span className="field-hint">{body.length.toLocaleString()} / 20,000</span><button className="button button-primary" disabled={submitting} type="submit">댓글 등록</button></div>
        </form>
      ) : !capabilities ? (
        <div className="comment-signin"><p>댓글 기능을 확인하는 중입니다.</p></div>
      ) : studioAccessFor(capabilities) === "disabled" ? (
        <div className="comment-signin"><p>이 배포본은 공개 읽기 전용이라 새 댓글을 받지 않습니다.</p></div>
      ) : studioAccessFor(capabilities) === "admin_only" ? (
        <div className="comment-signin"><p>단일 소유자 프로필에서는 커뮤니티 댓글을 사용하지 않습니다.</p></div>
      ) : (
        <div className="comment-signin"><p>로그인하면 이 글에 의견을 남길 수 있습니다.</p><AppLink className="button button-ghost" href="/login">로그인</AppLink></div>
      )}
      {status ? <p className="inline-status" role="status">{status}</p> : null}
      {comments.length ? (
        <ol className="comment-list">
          {comments.map((comment) => (
            <li key={comment.id}>
              <span className="avatar" aria-hidden="true">{initials(comment.author.displayName)}</span>
              <div className="comment-main">
                <div><strong>{comment.author.displayName}</strong><time dateTime={comment.createdAt}>{formatDate(comment.createdAt)}</time></div>
                {comment.artifactHtml ? (
                  <div className="comment-content" dangerouslySetInnerHTML={{ __html: DOMPurify.sanitize(comment.artifactHtml, { USE_PROFILES: { html: true }, FORBID_ATTR: ["style"] }) }} />
                ) : <p>{comment.sourceMarkdown}</p>}
              </div>
            </li>
          ))}
        </ol>
      ) : <p className="empty-comments">첫 댓글을 남겨 대화를 시작해 보세요.</p>}
    </section>
  );
}

export function LoginPage() {
  const { session, capabilities, capabilitiesError, refreshCapabilities, setSession } = useSession();
  const [mode, setMode] = useState<"login" | "register">("login");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string>();
  const [accessKey, setAccessKey] = useState("");
  usePageTitle(mode === "login" ? "로그인" : "가입");

  if (!capabilities) {
    return capabilitiesError
      ? <CapabilityRetry detail={capabilitiesError} onRetry={() => void refreshCapabilities()} />
      : <PageLoading label="로그인 기능을 확인하는 중" />;
  }

  const studioAccess = studioAccessFor(capabilities);
  const authChoices = adminAuthChoices(capabilities);
  const accessKeyMethod = authChoices.accessKeyMethods[0];
  const localAccounts = studioAccess === "members"
    && capabilities.mutationMechanisms.includes("session");

  if (studioAccess === "disabled") {
    return (
      <EmptyState
        actionHref="/"
        actionLabel="공개 글 읽기"
        description="이 인스턴스는 캐시 가능한 공개 읽기 전용으로 배포되어 로그인과 글쓰기를 제공하지 않습니다."
        title="읽기 전용 배포본입니다"
      />
    );
  }

  if (!localAccounts && authChoices.status !== "ready") {
    return (
      <EmptyState
        actionHref="/"
        actionLabel="공개 글 읽기"
        description={authChoices.status === "misconfigured"
          ? "관리자 인증 모듈의 설정이 완료되지 않았습니다. 공개 글은 계속 읽을 수 있습니다."
          : "이 서버에는 사용할 수 있는 관리자 인증 방식이 없습니다."}
        title="관리자 Studio를 열 수 없습니다"
      />
    );
  }

  if (!localAccounts && !accessKeyMethod && authChoices.externalMethods.length === 0) {
    return (
      <EmptyState
        actionHref="/"
        actionLabel="공개 글 읽기"
        description="서버가 안전하게 사용할 수 있는 관리자 인증 방법을 제공하지 않았습니다."
        title="관리자 인증을 사용할 수 없습니다"
      />
    );
  }

  if (session?.state === "authenticated") {
    return (
      <EmptyState
        actionHref={session.blog ? "/studio" : "/onboarding"}
        actionLabel={session.blog ? "Studio 열기" : "블로그 만들기"}
        description={`${session.user.displayName}님으로 로그인되어 있습니다.`}
        title="이미 로그인되어 있어요"
      />
    );
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setStatus(mode === "login" ? "로그인하는 중…" : "계정을 만드는 중…");
    const data = new FormData(event.currentTarget);
    try {
      const next = mode === "login"
        ? await client.login({ email: String(data.get("email") ?? ""), password: String(data.get("password") ?? "") })
        : await client.register({
            email: String(data.get("email") ?? ""),
            password: String(data.get("password") ?? ""),
            handle: String(data.get("handle") ?? ""),
            displayName: String(data.get("displayName") ?? ""),
          });
      setSession(next);
      if (next.state === "authenticated") navigate(next.blog ? "/studio" : "/onboarding");
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  async function submitAccessKey(
    event: FormEvent<HTMLFormElement>,
    method: AdminAccessKeyMethod,
  ) {
    event.preventDefault();
    if (!accessKey || busy) return;
    setBusy(true);
    setStatus("관리자 권한을 확인하는 중…");
    try {
      const next = await client.loginWithAdminAccessKey(
        { accessKey },
        method.actionHref,
      );
      setSession(next);
      if (next.state === "authenticated") navigate(next.blog ? "/studio" : "/onboarding");
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      setAccessKey("");
      setBusy(false);
    }
  }

  const registrationOpen = session?.state === "anonymous" ? session.registrationOpen : true;
  const adminOnly = studioAccess === "admin_only";
  const hasAdminMethods = Boolean(accessKeyMethod || authChoices.externalMethods.length);
  return (
    <div className="auth-page">
      <section className="auth-story">
        <p className="eyebrow">{adminOnly ? "Private control, public reading" : "One account, your own space"}</p>
        <h1>{adminOnly ? <>어디서든<br />내 글을 씁니다.</> : <>쓰는 사람의<br />자리를 만듭니다.</>}</h1>
        <p>{adminOnly
          ? "공개 글은 누구나 읽고, 검증된 관리자 세션만 Studio에서 쓰고 고칠 수 있습니다."
          : "광장에서 글을 발견하고, 나만의 테마로 블로그를 만들고, Markdown 원문을 온전히 소유하세요."}</p>
        <div className="auth-quote" aria-hidden="true"><span>“</span><p>좋은 도구는 글보다 앞에 나서지 않는다.</p></div>
      </section>
      <section className="auth-panel" aria-labelledby="auth-title">
        <h2 id="auth-title">{adminOnly ? "관리자 Studio 열기" : mode === "login" ? "다시 만나 반가워요" : "새로운 공간을 시작하세요"}</h2>
        <p>{adminOnly
          ? "서버 운영자가 켜 둔 방법 중 하나로 관리자 세션을 시작합니다."
          : mode === "login" ? "사용할 인증 방법을 선택하세요." : "로컬 계정을 만듭니다."}</p>

        {authChoices.externalMethods.length ? (
          <div className="external-auth-methods" aria-label="외부 관리자 인증">
            {authChoices.externalMethods.map((method) => {
              const href = safeAuthActionHref(method);
              return href ? <a className="button button-ghost button-wide" href={publicPath(href)} key={method.id}>{method.label}</a> : null;
            })}
          </div>
        ) : null}

        {accessKeyMethod ? (
          <form className="auth-form admin-access-form" onSubmit={(event) => void submitAccessKey(event, accessKeyMethod)}>
            {authChoices.externalMethods.length ? <div className="auth-divider"><span>또는</span></div> : null}
            <label htmlFor="admin-access-key">
              {accessKeyMethod.label}
              <input
                autoCapitalize="none"
                autoComplete="off"
                id="admin-access-key"
                maxLength={512}
                minLength={32}
                onChange={(event) => setAccessKey(event.target.value)}
                required
                spellCheck={false}
                type="password"
                value={accessKey}
              />
            </label>
            <p className="field-hint">접근 키는 세션을 만드는 이 요청에만 사용되며 브라우저 저장소에 보관하지 않습니다.</p>
            <button className="button button-primary button-wide" disabled={busy || !accessKey} type="submit">관리자로 계속</button>
          </form>
        ) : null}

        {localAccounts ? (
          <div className={hasAdminMethods ? "local-auth-section has-admin-methods" : "local-auth-section"}>
            {hasAdminMethods ? <div className="auth-divider"><span>로컬 계정</span></div> : null}
            <div className="auth-tabs" role="group" aria-label="로컬 계정 인증 방식">
              <button aria-pressed={mode === "login"} onClick={() => setMode("login")} type="button">로그인</button>
              <button aria-pressed={mode === "register"} disabled={!registrationOpen} onClick={() => setMode("register")} type="button">가입</button>
            </div>
            <form className="auth-form" onSubmit={(event) => void submit(event)}>
              {mode === "register" ? (
                <>
                  <label>표시 이름<input autoComplete="name" maxLength={80} name="displayName" required /></label>
                  <label>사용자 핸들<span className="input-prefix"><span>@</span><input autoComplete="username" maxLength={40} name="handle" pattern="[a-z0-9]+(?:-[a-z0-9]+)*" required /></span></label>
                </>
              ) : null}
              <label>이메일<input autoComplete="email" name="email" required type="email" /></label>
              <label>비밀번호<input autoComplete={mode === "login" ? "current-password" : "new-password"} minLength={8} name="password" required type="password" /></label>
              <button className="button button-primary button-wide" disabled={busy} type="submit">{mode === "login" ? "로그인" : "계정 만들기"}</button>
            </form>
          </div>
        ) : null}
        {status ? <p className="inline-status" role="status">{status}</p> : null}
      </section>
    </div>
  );
}

export function OnboardingPage() {
  const { session, capabilities, capabilitiesError, refreshCapabilities, setSession } = useSession();
  const [theme, setTheme] = useState<ThemePresetId>("paper");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string>();
  usePageTitle("블로그 만들기");

  if (!capabilities) {
    return capabilitiesError
      ? <CapabilityRetry detail={capabilitiesError} onRetry={() => void refreshCapabilities()} />
      : <PageLoading label="블로그 기능을 확인하는 중" />;
  }

  if (studioAccessFor(capabilities) === "disabled") {
    return <EmptyState actionHref="/" actionLabel="공개 글 읽기" description="이 인스턴스는 공개 읽기 전용으로 배포되어 블로그를 만들 수 없습니다." title="읽기 전용 배포본입니다" />;
  }

  if (!session) return <PageLoading label="계정을 확인하는 중" />;
  if (session.state === "anonymous") {
    return <EmptyState actionHref="/login" actionLabel="관리자 인증" description="블로그를 만들려면 먼저 서버가 제공하는 방법으로 인증해야 합니다." title="당신의 공간을 준비할까요?" />;
  }
  if (session.blog) {
    return <EmptyState actionHref={`/@${session.blog.handle}`} actionLabel="내 블로그 보기" description="선택한 테마와 공개 글을 내 블로그에서 확인할 수 있습니다." title="블로그가 이미 준비되어 있어요" />;
  }
  const authenticatedSession = session;

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setStatus("블로그를 만드는 중…");
    const data = new FormData(event.currentTarget);
    try {
      const description = String(data.get("description") ?? "");
      const blog = await client.createBlog({
        handle: String(data.get("handle") ?? ""),
        title: String(data.get("title") ?? ""),
        themePreset: theme,
        ...(description ? { description } : {}),
      });
      setSession({ ...authenticatedSession, blog });
      navigate(`/@${blog.handle}`);
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="onboarding-page">
      <header className="onboarding-heading"><p className="step-label">블로그 만들기 · 1분이면 충분해요</p><h1>어떤 공간을<br />만들고 싶나요?</h1><p>정보와 첫 테마를 고르세요. 콘텐츠는 테마와 독립적으로 언제나 Markdown으로 남습니다.</p></header>
      <form className="onboarding-form" onSubmit={(event) => void submit(event)}>
        <section className="onboarding-section" aria-labelledby="blog-info-title">
          <div className="numbered-heading"><span>01</span><div><h2 id="blog-info-title">블로그 정보</h2><p>독자가 기억할 이름과 주소를 정합니다.</p></div></div>
          <div className="field-grid">
            <label>블로그 이름<input defaultValue={`${session.user.displayName}의 기록`} maxLength={100} name="title" required /></label>
            <label>주소<span className="input-prefix"><span>/@</span><input defaultValue={session.user.handle} maxLength={40} name="handle" pattern="[a-z0-9]+(?:-[a-z0-9]+)*" required /></span></label>
          </div>
          <label>한 줄 소개<textarea maxLength={240} name="description" placeholder="무엇을 기록하는 공간인지 알려주세요." rows={3} /></label>
        </section>
        <section className="onboarding-section" aria-labelledby="theme-title">
          <div className="numbered-heading"><span>02</span><div><h2 id="theme-title">첫 테마</h2><p>모든 프리셋은 읽기와 대비 기준을 통과한 로컬 CSS입니다.</p></div></div>
          <fieldset className="theme-grid"><legend className="sr-only">블로그 테마 선택</legend>
            {THEME_PRESETS.map((preset) => (
              <label className="theme-option" data-theme={preset.id} key={preset.id}>
                <input checked={theme === preset.id} name="theme" onChange={() => setTheme(preset.id)} type="radio" value={preset.id} />
                <span className="theme-preview" aria-hidden="true"><span className="preview-nav">OSB / NOTE</span><strong>{preset.sampleTitle}</strong><span className="preview-lines" /></span>
                <span className="theme-copy"><strong>{preset.name}</strong><small>{preset.description}</small></span>
                <span className="theme-check" aria-hidden="true">✓</span>
              </label>
            ))}
          </fieldset>
        </section>
        <div className="onboarding-actions"><p>선택한 테마: <strong>{THEME_PRESETS.find((item) => item.id === theme)?.name}</strong></p><button className="button button-primary" disabled={busy} type="submit">내 블로그 만들기 <span aria-hidden="true">→</span></button></div>
        {status ? <p className="inline-status" role="status">{status}</p> : null}
      </form>
    </div>
  );
}

export function NotFoundPage() {
  usePageTitle("페이지 없음");
  return <EmptyState actionHref="/" actionLabel="피드로 돌아가기" description="주소가 바뀌었거나 존재하지 않는 페이지입니다." title="길을 잃은 것 같아요" />;
}

function EmptyState({ title, description, actionHref, actionLabel }: { title: string; description: string; actionHref?: string; actionLabel?: string }) {
  return <section className="empty-state"><span className="empty-symbol" aria-hidden="true">✦</span><h1>{title}</h1><p>{description}</p>{actionHref && actionLabel ? <AppLink className="button button-primary" href={actionHref}>{actionLabel}</AppLink> : null}</section>;
}

function StatusMessage({ title, detail }: { title: string; detail: string }) {
  return <section className="status-message" role="alert"><span aria-hidden="true">!</span><div><h1>{title}</h1><p>{detail}</p></div></section>;
}

function CapabilityRetry({ detail, onRetry }: { detail: string; onRetry: () => void }) {
  return (
    <section className="empty-state" role="alert">
      <span className="empty-symbol" aria-hidden="true">!</span>
      <h1>서버 기능을 확인하지 못했습니다</h1>
      <p>{detail}</p>
      <button className="button button-primary" onClick={onRetry} type="button">다시 시도</button>
    </section>
  );
}

function PageLoading({ label }: { label: string }) {
  return <div className="page-loading" role="status"><span aria-hidden="true" /><p>{label}…</p></div>;
}

function FeedSkeleton() {
  return <div aria-label="피드를 불러오는 중" className="post-grid" role="status">{[0, 1, 2].map((value) => <div className="post-card skeleton-card" key={value}><span /><span /><span /></div>)}</div>;
}

function formatCodeRun(response: CodeRunResponse): string {
  if (response.state === "queued") return "실행기가 제한 시간 안에 완료되지 않았습니다.";
  const { result } = response;
  const sections = [`${result.outcome}${result.exitCode === null ? "" : ` (exit ${result.exitCode})`}`];
  if (result.stdout) sections.push(`stdout:\n${result.stdout}`);
  if (result.stderr) sections.push(`stderr:\n${result.stderr}`);
  if (result.truncated) sections.push("정책에 따라 출력 일부가 잘렸습니다.");
  return sections.join("\n\n");
}

function abortableDelay(milliseconds: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const onAbort = () => {
      window.clearTimeout(handle);
      reject(new DOMException("Aborted", "AbortError"));
    };
    const handle = window.setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, milliseconds);
    if (signal.aborted) onAbort();
    else signal.addEventListener("abort", onAbort, { once: true });
  });
}
