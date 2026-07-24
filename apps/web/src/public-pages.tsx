import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type FormEvent,
} from "react";
import DOMPurify from "dompurify";
import type {
  AiSummary,
  BlogPostView,
  BlogSummary,
  Capabilities,
  CodeRunResponse,
  CommentView,
  FeedPostSummary,
  HomeResponse,
  PostSummary,
  PostView,
  RunnerProfile,
  ReferencesPage as ReferencesPageView,
  ThemePresetId,
  ViewMode,
} from "@opensoverignblog/sdk";
import { AdminAccessKeyForm } from "./admin-access";
import { usePublicReaderContentStatus, useSession } from "./app";
import {
  articleHref,
  articleViewFromSearch,
  publicCategoryPath,
  publicFeedPostPath,
} from "./article-location";
import { authorshipLabel, HUMAN_AUTHORSHIP, normalizedAuthorship } from "./authorship";
import {
  adminAuthChoices,
  safeAuthActionHref,
  studioAccessFor,
} from "./auth-policy";
import { safeBlogStylesheetUrl } from "./site-stylesheet";
import { installSocialEmbedHydration } from "./social-embeds";
import { presentHome } from "./home-presentation";
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
  text,
  uiLanguage,
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
  isPrimary: false,
};

export function FeedPage() {
  const { session, capabilities } = useSession();
  usePageTitle(text("홈", "Home"));
  const [home, setHome] = useState<HomeResponse>({
    pinnedItems: [],
    recentItems: [],
    categorySections: [],
    seriesSections: [],
  });
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>();
  usePublicReaderContentStatus(
    error ? "error" : loading ? "pending" : "ready",
  );

  useEffect(() => {
    const controller = new AbortController();
    void loadHome(controller.signal)
      .then(setHome)
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setError(asMessage(reason));
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, []);

  const presentation = useMemo(() => presentHome(home), [home]);
  const total = presentation.total;
  const writeHref = session?.state === "authenticated"
    ? (session.blog ? "/studio/new" : "/onboarding")
    : "/login";
  const accessKeyLogin = Boolean(
    capabilities && adminAuthChoices(capabilities).accessKeyMethods.length,
  );

  return (
    <div className="wiki-home">
      <aside className="wiki-sidebar" aria-label={text("홈 안내", "Home guide")}>
        <section>
          <h2>OpenSoverignBlog</h2>
          <p>{text("Markdown 원문과 서버를 작성자가 직접 소유하는 온프레미스 블로그 엔진입니다.", "A self-hosted blog engine where authors own both their Markdown source and server.")}</p>
        </section>
        <nav aria-label={text("빠른 이동", "Quick navigation")}>
          <strong>{text("빠른 이동", "Quick navigation")}</strong>
          {home.pinnedItems.length ? <a href="#home-pinned">{text("주요 글", "Featured posts")}</a> : null}
          {presentation.seriesSections.map((section) => (
            <a href={`#${section.anchorId}`} key={section.series.id}>{section.series.title}</a>
          ))}
          {presentation.categorySections.map((section) => (
            <a href={`#${section.anchorId}`} key={section.category.id}>{section.category.title}</a>
          ))}
          {presentation.recentItems.length ? <a href="#home-recent">{text("최근 글", "Recent posts")}</a> : null}
          {capabilities?.references ? (
            <AppLink href={capabilities.references.href}>{capabilities.references.label}</AppLink>
          ) : null}
          <a href={publicPath("/openapi/openapi.yaml")}>OpenAPI</a>
          <a href={publicPath("/AI2AI.md")}>{text("AI 접근 안내", "AI access guide")}</a>
        </nav>
        <dl>
          <div><dt>{text("공개 글", "Public posts")}</dt><dd>{total.toLocaleString(uiLanguage === "en" ? "en-US" : "ko-KR")}</dd></div>
          <div><dt>{text("운영 모드", "Mode")}</dt><dd>{capabilities && studioAccessFor(capabilities) === "disabled" ? text("읽기 전용", "Read only") : text("발행 가능", "Publishing enabled")}</dd></div>
        </dl>
        {capabilities && studioAccessFor(capabilities) !== "disabled" ? (
          <AppLink className="button button-primary wiki-write" href={writeHref}>
            {session?.state === "authenticated" && session.blog
              ? text("글 쓰기", "Write")
              : accessKeyLogin ? text("관리자 키 입력", "Enter administrator key") : text("관리 시작", "Start managing")}
          </AppLink>
        ) : null}
      </aside>

      <div className="wiki-main">
        <header className="wiki-welcome">
          <p className="eyebrow">Self-hosted publishing</p>
          <h1>{text("읽는 화면은 가볍게, 기록의 소유권은 분명하게.", "A lightweight reading experience, with ownership kept clear.")}</h1>
          <p>{text("이 홈은 발행된 불변 리비전만 보여 줍니다. 초안과 관리자 기능은 공개 캐시에서 분리됩니다.", "This home page shows only published immutable revisions. Drafts and administrator features stay outside the public cache.")}</p>
        </header>

        {loading ? <FeedSkeleton /> : null}
        {error ? <StatusMessage title={text("홈을 불러오지 못했습니다", "Could not load home")} detail={error} /> : null}
        {!loading && !error && total === 0 ? (
          <EmptyState
            {...(!capabilities || studioAccessFor(capabilities) === "disabled" ? {} : {
              actionHref: writeHref,
              actionLabel: session?.state === "authenticated"
                ? (session.blog ? text("첫 글 쓰기", "Write your first post") : text("블로그 만들기", "Create a blog"))
                : accessKeyLogin
                  ? text("관리자 키 입력", "Enter administrator key")
                  : studioAccessFor(capabilities) === "admin_only" ? text("관리자로 시작하기", "Start as administrator") : text("로그인하고 시작하기", "Log in to get started"),
            })}
            description={text("아직 발행된 글이 없습니다. 작은 메모부터 시작해 보세요.", "There are no published posts yet. Start with a small note.")}
            title={text("첫 이야기를 기다리고 있어요", "Waiting for the first story")}
          />
        ) : null}

        {home.pinnedItems.length ? (
          <HomePostSection id="home-pinned" items={home.pinnedItems} title={text("주요 글", "Featured posts")} tone="pinned" />
        ) : null}
        {presentation.seriesSections.map((section) => (
          <HomePostSection
            {...(section.series.description
              ? { description: section.series.description }
              : {})}
            href={publicCategoryPath({
              categorySlug: section.series.slug,
              handle: "",
              primary: true,
            })}
            id={section.anchorId}
            items={section.items}
            key={section.series.id}
            title={section.series.title}
            tone="series"
          />
        ))}
        {presentation.categorySections.map((section) => (
          <HomePostSection
            {...(section.category.description
              ? { description: section.category.description }
              : {})}
            href={publicCategoryPath({
              categorySlug: section.category.slug,
              handle: "",
              primary: true,
            })}
            id={section.anchorId}
            items={section.items}
            key={section.category.id}
            title={section.category.title}
            tone="category"
          />
        ))}
        {presentation.recentItems.length ? (
          <HomePostSection id="home-recent" items={presentation.recentItems} title={text("최근 변경", "Recent changes")} tone="recent" />
        ) : null}
      </div>
    </div>
  );
}

export function ReferencesPage({ capabilities }: { capabilities: Capabilities | undefined }) {
  const advertisedLabel = capabilities?.references?.label ?? text("레퍼런스", "References");
  const [page, setPage] = useState<ReferencesPageView>();
  const [error, setError] = useState<string>();
  usePublicReaderContentStatus(error ? "error" : page ? "ready" : "pending");
  usePageTitle(page?.label ?? advertisedLabel);

  useEffect(() => {
    const controller = new AbortController();
    setPage(undefined);
    setError(undefined);
    void client.references(controller.signal)
      .then(setPage)
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setError(asMessage(reason));
      });
    return () => controller.abort();
  }, []);

  if (error) return <StatusMessage title={text(`${advertisedLabel}를 불러오지 못했습니다`, `Could not load ${advertisedLabel}`)} detail={error} />;
  if (!page) return <PageLoading label={text(`${advertisedLabel}를 불러오는 중`, `Loading ${advertisedLabel}`)} />;

  return (
    <div className="osb-site-frame references-page">
      <article className="article-shell">
        <header className="article-header references-header">
          <p className="eyebrow">Global references</p>
          <h1>{page.label}</h1>
          <p className="article-deck">{text("출처, 라이선스, 개인정보와 운영 정책을 한곳에서 확인합니다.", "Review sources, licenses, privacy, and operational policies in one place.")}</p>
        </header>
        <ArticleBody capabilities={capabilities} html={page.artifactHtml} />
        <details className="artifact-proof">
          <summary>{text("문서 무결성 정보", "Document integrity")}</summary>
          <div><span>{page.rendererVersion}</span><code>{page.sourceHash}</code></div>
        </details>
      </article>
    </div>
  );
}

function HomePostSection({
  description,
  href,
  id,
  items,
  title,
  tone,
}: {
  description?: string;
  href?: string;
  id: string;
  items: FeedPostSummary[];
  title: string;
  tone: "pinned" | "series" | "category" | "recent";
}) {
  const collapsible = tone === "series" || tone === "category";
  const [expanded, setExpanded] = useState(true);
  const contentId = `${id}-content`;

  useEffect(() => {
    if (!collapsible) return;
    const revealFragmentTarget = () => {
      if (window.location.hash === `#${id}`) setExpanded(true);
    };
    revealFragmentTarget();
    window.addEventListener("hashchange", revealFragmentTarget);
    return () => window.removeEventListener("hashchange", revealFragmentTarget);
  }, [collapsible, id]);

  return (
    <section className={`wiki-panel wiki-panel-${tone}`} id={id} aria-labelledby={`${id}-title`}>
      <div className="wiki-panel-heading">
        <div className="wiki-panel-heading-copy">
          <h2 id={`${id}-title`}>
            {href ? <AppLink href={href}>{title}</AppLink> : title}
          </h2>
          {description ? <p className="wiki-panel-description">{description}</p> : null}
        </div>
        <div className="wiki-panel-controls">
          <span>{text(`${items.length}개`, `${items.length} posts`)}</span>
          {collapsible ? (
            <button
              aria-controls={contentId}
              aria-expanded={expanded}
              className="wiki-panel-toggle"
              onClick={() => setExpanded((value) => !value)}
              type="button"
            >
              <span aria-hidden="true">{expanded ? "−" : "+"}</span>
              <span>{expanded ? text("접기", "Collapse") : text("펼치기", "Expand")}</span>
            </button>
          ) : null}
        </div>
      </div>
      <div className="wiki-post-list" hidden={collapsible && !expanded} id={contentId}>
        {items.map((post) => <DensePostRow key={post.id} post={post} />)}
      </div>
    </section>
  );
}

function DensePostRow({ post }: { post: FeedPostSummary }) {
  const href = publicFeedPostPath(post);
  return (
    <article className="wiki-post-row">
      <div className="wiki-post-copy">
        <h3><AppLink href={href}>{post.title}</AppLink></h3>
        <p>{post.excerpt}</p>
        <div className="wiki-post-meta">
          <AppLink href={`/@${encodeURIComponent(post.blog.handle)}`}>@{post.blog.handle}</AppLink>
          <span>{post.author.displayName}</span>
          <time dateTime={post.publishedAt}>{formatDate(post.publishedAt)}</time>
          {post.commentCount ? <span>{text(`댓글 ${post.commentCount}`, `${post.commentCount} comments`)}</span> : null}
          <AuthorshipBadge value={post.authorship} />
        </div>
      </div>
      <span className="list-arrow" aria-hidden="true">›</span>
    </article>
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

async function loadHome(signal?: AbortSignal): Promise<HomeResponse> {
  try {
    return await client.home(signal);
  } catch (reason) {
    if (!isNotFound(reason)) throw reason;
    return {
      pinnedItems: [],
      recentItems: await loadFeed(signal),
      categorySections: [],
      seriesSections: [],
    };
  }
}

function legacyFeedPost(post: PostSummary): FeedPostSummary {
  return {
    id: post.id,
    title: post.title,
    slug: post.slug,
    excerpt: post.hasIntentView
      ? text("작성자의 의도와 portable Markdown을 함께 보존한 글입니다.", "This post preserves the author's intent alongside portable Markdown.")
      : text("언제든 내보낼 수 있는 portable Markdown 글입니다.", "This portable Markdown post can be exported at any time."),
    publishedAt: post.updatedAt,
    updatedAt: post.updatedAt,
    author: LEGACY_USER,
    blog: LEGACY_BLOG,
    tags: [],
    commentCount: 0,
    hasIntentView: post.hasIntentView,
    authorship: post.authorship ?? HUMAN_AUTHORSHIP,
  };
}

function AuthorshipBadge({ value }: { value: FeedPostSummary["authorship"] | undefined }) {
  const authorship = normalizedAuthorship(value);
  return (
    <span className={`authorship-badge authorship-${authorship.kind}`} title={text("휴대 가능한 공개 작성 출처", "Portable public authorship provenance")}>
      {authorshipLabel(authorship, uiLanguage)}
    </span>
  );
}

export function BlogPage({ handle }: { handle: string }) {
  const [blog, setBlog] = useState<BlogSummary>();
  const [posts, setPosts] = useState<FeedPostSummary[]>([]);
  const [error, setError] = useState<string>();
  usePublicReaderContentStatus(error ? "error" : blog ? "ready" : "pending");
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

  if (error) return <StatusMessage title={text("블로그를 찾을 수 없습니다", "Could not find blog")} detail={error} />;
  if (!blog) return <PageLoading label={text("블로그를 불러오는 중", "Loading blog")} />;
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
          <p>{blog.description || text("아직 블로그 소개가 없습니다.", "This blog does not have a description yet.")}</p>
          <div className="blog-owner">
            <span className="avatar" aria-hidden="true">{initials(blog.owner.displayName)}</span>
            <span><strong>{blog.owner.displayName}</strong><small>{text("이 블로그의 작성자", "Author of this blog")}</small></span>
          </div>
        </div>
          </section>
          <section className="blog-posts" aria-labelledby="blog-posts-title">
        <div className="section-heading">
          <div><p className="eyebrow">Archive</p><h2 id="blog-posts-title">{text("모든 글", "All posts")}</h2></div>
          <span className="result-count">{text(`${posts.length}개`, `${posts.length} posts`)}</span>
        </div>
        {posts.length ? (
          <div className="blog-list">
            {posts.map((post, index) => (
              <article className="blog-list-item" key={post.id}>
                <span className="post-order" aria-hidden="true">{String(index + 1).padStart(2, "0")}</span>
                <div>
                  <div className="post-card-meta">
                    <time dateTime={post.publishedAt}>{formatDate(post.publishedAt)}</time>
                    <AuthorshipBadge value={post.authorship} />
                  </div>
                  <h3><AppLink href={publicFeedPostPath(post)}>{post.title}</AppLink></h3>
                  <p>{post.excerpt}</p>
                </div>
                <span className="list-arrow" aria-hidden="true">↗</span>
              </article>
            ))}
          </div>
        ) : (
          <EmptyState description={text("아직 발행된 글이 없습니다.", "No posts have been published yet.")} title={text("빈 서가입니다", "This shelf is empty")} />
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
  categorySlug,
  primary = false,
}: {
  handle: string;
  slug: string;
  capabilities: Capabilities | undefined;
  legacy?: boolean;
  categorySlug?: string;
  primary?: boolean;
}) {
  const [view, setView] = useState<ViewMode>(() => articleViewFromSearch(window.location.search));
  const [post, setPost] = useState<BlogPostView>();
  const [error, setError] = useState<string>();
  usePublicReaderContentStatus(error ? "error" : post ? "ready" : "pending");
  const legacyArticle = legacy;
  usePageTitle(post?.title ?? text("글 읽기", "Read post"));

  useEffect(() => {
    const updateViewFromLocation = () => setView(articleViewFromSearch(window.location.search));
    window.addEventListener("popstate", updateViewFromLocation);
    return () => window.removeEventListener("popstate", updateViewFromLocation);
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    setPost(undefined);
    setError(undefined);
    void loadArticle(
      handle,
      slug,
      view,
      legacyArticle,
      categorySlug,
      primary,
      controller.signal,
    )
      .then((value) => {
        const canonicalCategory = value.category?.slug;
        const canonicalPrimary = Boolean(canonicalCategory && value.blog.isPrimary);
        if (
          value.requestedSlug !== value.canonicalSlug
          || categorySlug !== canonicalCategory
          || primary !== canonicalPrimary
        ) {
          navigate(articleHref({
            handle,
            slug: value.canonicalSlug,
            legacy: legacyArticle,
            view,
            ...(canonicalCategory ? { categorySlug: canonicalCategory } : {}),
            primary: canonicalPrimary,
          }), true);
          return;
        }
        setPost(value);
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setError(asMessage(reason));
      });
    return () => controller.abort();
  }, [handle, slug, view, legacyArticle, categorySlug, primary]);

  if (error) return <StatusMessage title={text("글을 불러오지 못했습니다", "Could not load post")} detail={error} />;
  if (!post) return <PageLoading label={text("글을 불러오는 중", "Loading post")} />;
  const storedAiSummary = post.aiSummary;
  // Public JSON is projected through the same source-bound validation as the
  // renderer, so an omitted value must never be reconstructed in the browser.
  const reviewedAiSummary = storedAiSummary?.provenance.humanReviewed
    ? storedAiSummary
    : undefined;
  const articleTheme = post.category?.themePreset ?? post.blog.theme.presetId;
  const selectView = (nextView: ViewMode) => {
    if (nextView === view && articleViewFromSearch(window.location.search) === nextView) return;
    navigate(articleHref({
      handle: post.blog.handle,
      slug: post.canonicalSlug,
      legacy: legacyArticle,
      view: nextView,
      ...(categorySlug ? { categorySlug } : {}),
      primary: Boolean(post.category && post.blog.isPrimary),
    }));
  };
  return (
    <>
      <BlogCustomStylesheet handle={post.blog.handle} href={post.blog.theme.customCssUrl} />
      <div className="osb-site-frame">
        <div className="article-page osb-site-theme" data-custom-css={post.blog.theme.customCssUrl ? "enabled" : "disabled"} data-site-id={post.blog.id} data-theme={articleTheme}>
          <article className="article-shell">
        <header className="article-header">
          <div className="article-kicker">
            <AppLink href={`/@${post.blog.handle}`}>@{post.blog.handle}</AppLink>
            <span aria-hidden="true">/</span>
            {post.category ? (
              <>
                <AppLink href={publicCategoryPath({
                  handle: post.blog.handle,
                  categorySlug: post.category.slug,
                  primary: post.blog.isPrimary,
                })}
                >{post.category.title}</AppLink>
                <span aria-hidden="true">/</span>
              </>
            ) : null}
            <time dateTime={post.publishedAt}>{formatDate(post.publishedAt)}</time>
            <AuthorshipBadge value={post.authorship} />
          </div>
          <h1>{post.title}</h1>
          {post.excerpt ? <p className="article-deck">{post.excerpt}</p> : null}
          <div className="article-author-row">
            <span className="avatar" aria-hidden="true">{initials(post.author.displayName)}</span>
            <div><strong>{post.author.displayName}</strong><span>{text("글쓴이", "Author")}</span></div>
            {post.tags.length ? (
              <ul className="tag-list" aria-label={text("태그", "Tags")}>
                {post.tags.map((tag) => <li key={tag}>#{tag}</li>)}
              </ul>
            ) : null}
          </div>
          <div className="projection-switcher" role="group" aria-label={text("콘텐츠 보기 방식", "Content view")}>
            <button aria-pressed={view === "intent"} onClick={() => selectView("intent")} type="button">{text("작성자 보기", "Author view")}</button>
            <button aria-pressed={view === "markdown_source"} onClick={() => selectView("markdown_source")} type="button">{text(".md 원문", ".md source")}</button>
          </div>
        </header>
        {reviewedAiSummary ? (
          <section className="public-ai-summary" aria-label={text("AI 요약 정보", "AI summary information")}>
            <p className="public-ai-summary-provenance">
              {text("AI가 만든 요약 초안을 사람이 검토함", "Human-reviewed AI summary draft")} · {publicAiProviderLabel(reviewedAiSummary.provenance.provider)} · {reviewedAiSummary.provenance.model} · {text(`${formatDate(reviewedAiSummary.provenance.generatedAt)} 생성`, `generated ${formatDate(reviewedAiSummary.provenance.generatedAt)}`)}
            </p>
          </section>
        ) : null}
        <ArticleBody capabilities={capabilities} html={post.artifact.html} />
        <details className="artifact-proof">
          <summary>{text("문서 무결성 정보", "Document integrity")}</summary>
          <div><span>{post.artifact.rendererVersion}</span><code>{post.artifact.artifactHash}</code></div>
        </details>
          </article>
          <CommentSection postId={post.id} />
        </div>
      </div>
    </>
  );
}

function publicAiProviderLabel(provider: AiSummary["provenance"]["provider"]): string {
  if (provider === "openai") return "OpenAI";
  if (provider === "anthropic") return "Anthropic";
  if (provider === "google") return "Google Gemini";
  return provider;
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
  categorySlug: string | undefined,
  primary: boolean,
  signal: AbortSignal,
): Promise<BlogPostView> {
  if (legacy) return legacyPostView(await client.getPost(slug, view, signal));
  if (categorySlug && primary) {
    return client.getPrimaryCategoryPost(categorySlug, slug, view, signal);
  }
  if (categorySlug) {
    return client.getBlogCategoryPost(handle, categorySlug, slug, view, signal);
  }
  return client.getBlogPost(handle, slug, view, signal);
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
    authorship: post.authorship ?? HUMAN_AUTHORSHIP,
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
  const safeInnerHtml = useMemo(() => ({ __html: sanitizedHtml }), [sanitizedHtml]);
  const socialEmbedsEnabled = capabilities?.features.includes("social_embeds") ?? false;

  useEffect(() => {
    const body = bodyRef.current;
    return body && socialEmbedsEnabled
      ? installSocialEmbedHydration(body, uiLanguage)
      : undefined;
  }, [sanitizedHtml, socialEmbedsEnabled]);

  useEffect(() => {
    if (
      !capabilities?.features.includes("code_runner")
      || studioAccessFor(capabilities) !== "admin_only"
      || session?.state !== "authenticated"
    ) {
      setRunnerProfiles((current) => current.length ? [] : current);
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
      button.textContent = text(`${profile.id}로 실행`, `Run with ${profile.id}`);
      const output = document.createElement("pre");
      output.className = "run-code-output";
      output.setAttribute("aria-live", "polite");
      output.hidden = true;
      button.addEventListener("click", async () => {
        button.disabled = true;
        output.hidden = false;
        output.textContent = text("격리 실행기에 제출하는 중…", "Submitting to isolated runner…");
        try {
          let result = await client.submitCodeRun(profile.id, code.textContent ?? "", controller.signal);
          let polls = 0;
          while (result.state === "queued" && polls < 120) {
            output.textContent = text(`대기열 ${result.jobId.slice(0, 8)}…`, `Queued ${result.jobId.slice(0, 8)}…`);
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

  return <div className="article-content" ref={bodyRef} dangerouslySetInnerHTML={safeInnerHtml} />;
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
    setStatus(text("댓글을 등록하는 중…", "Posting comment…"));
    try {
      const created = await client.createComment(postId, { sourceMarkdown: body.trim() });
      setComments((current) => [...current, created]);
      setBody("");
      setStatus(text("댓글이 등록되었습니다.", "Comment posted."));
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
        <div><p className="eyebrow">Conversation</p><h2 id="comments-title">{text(`댓글 ${comments.length}`, `Comments ${comments.length}`)}</h2></div>
      </div>
      {session?.state === "authenticated" && commentsWritable ? (
        <form className="comment-form" onSubmit={(event) => void submit(event)}>
          <label htmlFor="comment-body">{text("댓글 남기기", "Leave a comment")}</label>
          <textarea
            id="comment-body"
            maxLength={20_000}
            onChange={(event) => setBody(event.target.value)}
            placeholder={text("Markdown으로 생각을 나눠주세요.", "Share your thoughts in Markdown.")}
            required
            value={body}
          />
          <div><span className="field-hint">{body.length.toLocaleString(uiLanguage === "en" ? "en-US" : "ko-KR")} / 20,000</span><button className="button button-primary" disabled={submitting} type="submit">{text("댓글 등록", "Post comment")}</button></div>
        </form>
      ) : !capabilities ? (
        <div className="comment-signin"><p>{text("댓글 기능을 확인하는 중입니다.", "Checking comment availability.")}</p></div>
      ) : studioAccessFor(capabilities) === "disabled" ? (
        <div className="comment-signin"><p>{text("이 배포본은 공개 읽기 전용이라 새 댓글을 받지 않습니다.", "This deployment is public read-only and does not accept new comments.")}</p></div>
      ) : studioAccessFor(capabilities) === "admin_only" ? (
        <div className="comment-signin"><p>{text("단일 소유자 프로필에서는 커뮤니티 댓글을 사용하지 않습니다.", "Community comments are unavailable in the single-owner profile.")}</p></div>
      ) : (
        <div className="comment-signin"><p>{text("로그인하면 이 글에 의견을 남길 수 있습니다.", "Log in to share your thoughts on this post.")}</p><AppLink className="button button-ghost" href="/login">{text("로그인", "Log in")}</AppLink></div>
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
      ) : <p className="empty-comments">{text("첫 댓글을 남겨 대화를 시작해 보세요.", "Start the conversation with the first comment.")}</p>}
    </section>
  );
}

export function LoginPage() {
  const { session, capabilities, capabilitiesError, refreshCapabilities, setSession } = useSession();
  const [mode, setMode] = useState<"login" | "register">("login");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string>();
  usePageTitle(mode === "login" ? text("로그인", "Log in") : text("가입", "Sign up"));

  if (!capabilities) {
    return capabilitiesError
      ? <CapabilityRetry detail={capabilitiesError} onRetry={() => void refreshCapabilities()} />
      : <PageLoading label={text("로그인 기능을 확인하는 중", "Checking login options")} />;
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
        actionLabel={text("공개 글 읽기", "Read public posts")}
        description={text("이 인스턴스는 캐시 가능한 공개 읽기 전용으로 배포되어 로그인과 글쓰기를 제공하지 않습니다.", "This instance is deployed as cacheable public read-only and does not provide login or writing.")}
        title={text("읽기 전용 배포본입니다", "This deployment is read-only")}
      />
    );
  }

  if (!localAccounts && authChoices.status !== "ready") {
    return (
      <EmptyState
        actionHref="/"
        actionLabel={text("공개 글 읽기", "Read public posts")}
        description={authChoices.status === "misconfigured"
          ? text("관리자 인증 모듈의 설정이 완료되지 않았습니다. 공개 글은 계속 읽을 수 있습니다.", "The administrator authentication module is not fully configured. Public posts remain available.")
          : text("이 서버에는 사용할 수 있는 관리자 인증 방식이 없습니다.", "This server does not offer an available administrator authentication method.")}
        title={text("관리자 Studio를 열 수 없습니다", "Cannot open administrator Studio")}
      />
    );
  }

  if (!localAccounts && !accessKeyMethod && authChoices.externalMethods.length === 0) {
    return (
      <EmptyState
        actionHref="/"
        actionLabel={text("공개 글 읽기", "Read public posts")}
        description={text("서버가 안전하게 사용할 수 있는 관리자 인증 방법을 제공하지 않았습니다.", "The server did not advertise a safe administrator authentication method.")}
        title={text("관리자 인증을 사용할 수 없습니다", "Administrator authentication is unavailable")}
      />
    );
  }

  if (session?.state === "authenticated") {
    return (
      <EmptyState
        actionHref={session.blog ? "/studio" : "/onboarding"}
        actionLabel={session.blog ? text("Studio 열기", "Open Studio") : text("블로그 만들기", "Create blog")}
        description={text(`${session.user.displayName}님으로 로그인되어 있습니다.`, `Logged in as ${session.user.displayName}.`)}
        title={text("이미 로그인되어 있어요", "You are already logged in")}
      />
    );
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setStatus(mode === "login" ? text("로그인하는 중…", "Logging in…") : text("계정을 만드는 중…", "Creating account…"));
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

  const registrationOpen = session?.state === "anonymous" ? session.registrationOpen : true;
  const adminOnly = studioAccess === "admin_only";
  const hasAdminMethods = Boolean(accessKeyMethod || authChoices.externalMethods.length);
  return (
    <div className="auth-page">
      <section className="auth-story">
        <p className="eyebrow">{adminOnly ? "Private control, public reading" : "One account, your own space"}</p>
        <h1>{adminOnly
          ? (uiLanguage === "en" ? <>Write your words<br />from anywhere.</> : <>어디서든<br />내 글을 씁니다.</>)
          : (uiLanguage === "en" ? <>Make a place<br />for the writer.</> : <>쓰는 사람의<br />자리를 만듭니다.</>)}</h1>
        <p>{adminOnly
          ? text("공개 글은 누구나 읽고, 검증된 관리자 세션만 Studio에서 쓰고 고칠 수 있습니다.", "Anyone can read public posts, while only verified administrator sessions can write and edit in Studio.")
          : text("광장에서 글을 발견하고, 나만의 테마로 블로그를 만들고, Markdown 원문을 온전히 소유하세요.", "Discover writing, create a blog in your own theme, and fully own the Markdown source.")}</p>
        <div className="auth-quote" aria-hidden="true"><span>“</span><p>{text("좋은 도구는 글보다 앞에 나서지 않는다.", "A good tool never steps ahead of the writing.")}</p></div>
      </section>
      <section className="auth-panel" aria-labelledby="auth-title">
        <h2 id="auth-title">{adminOnly ? text("관리자 Studio 열기", "Open administrator Studio") : mode === "login" ? text("다시 만나 반가워요", "Welcome back") : text("새로운 공간을 시작하세요", "Start a new space")}</h2>
        <p>{adminOnly
          ? text("서버 운영자가 켜 둔 방법 중 하나로 관리자 세션을 시작합니다.", "Start an administrator session with one of the methods enabled by the server operator.")
          : mode === "login" ? text("사용할 인증 방법을 선택하세요.", "Choose an authentication method.") : text("로컬 계정을 만듭니다.", "Create a local account.")}</p>

        {authChoices.externalMethods.length ? (
          <div className="external-auth-methods" aria-label={text("외부 관리자 인증", "External administrator authentication")}>
            {authChoices.externalMethods.map((method) => {
              const href = safeAuthActionHref(method);
              return href ? <a className="button button-ghost button-wide" href={publicPath(href)} key={method.id}>{method.label}</a> : null;
            })}
          </div>
        ) : null}

        {accessKeyMethod ? (
          <AdminAccessKeyForm
            method={accessKeyMethod}
            onAuthenticated={(next) => {
              setSession(next);
              if (next.state === "authenticated") navigate(next.blog ? "/studio" : "/onboarding");
            }}
            showDivider={Boolean(authChoices.externalMethods.length)}
          />
        ) : null}

        {localAccounts ? (
          <div className={hasAdminMethods ? "local-auth-section has-admin-methods" : "local-auth-section"}>
            {hasAdminMethods ? <div className="auth-divider"><span>{text("로컬 계정", "Local account")}</span></div> : null}
            <div className="auth-tabs" role="group" aria-label={text("로컬 계정 인증 방식", "Local account authentication")}>
              <button aria-pressed={mode === "login"} onClick={() => setMode("login")} type="button">{text("로그인", "Log in")}</button>
              <button aria-pressed={mode === "register"} disabled={!registrationOpen} onClick={() => setMode("register")} type="button">{text("가입", "Sign up")}</button>
            </div>
            <form className="auth-form" onSubmit={(event) => void submit(event)}>
              {mode === "register" ? (
                <>
                  <label>{text("표시 이름", "Display name")}<input autoComplete="name" maxLength={80} name="displayName" required /></label>
                  <label>{text("사용자 핸들", "User handle")}<span className="input-prefix"><span>@</span><input autoComplete="username" maxLength={40} name="handle" pattern="[a-z0-9]+(?:-[a-z0-9]+)*" required /></span></label>
                </>
              ) : null}
              <label>{text("이메일", "Email")}<input autoComplete="email" name="email" required type="email" /></label>
              <label>{text("비밀번호", "Password")}<input autoComplete={mode === "login" ? "current-password" : "new-password"} minLength={8} name="password" required type="password" /></label>
              <button className="button button-primary button-wide" disabled={busy} type="submit">{mode === "login" ? text("로그인", "Log in") : text("계정 만들기", "Create account")}</button>
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
  usePageTitle(text("블로그 만들기", "Create blog"));

  if (!capabilities) {
    return capabilitiesError
      ? <CapabilityRetry detail={capabilitiesError} onRetry={() => void refreshCapabilities()} />
      : <PageLoading label={text("블로그 기능을 확인하는 중", "Checking blog capabilities")} />;
  }

  if (studioAccessFor(capabilities) === "disabled") {
    return <EmptyState actionHref="/" actionLabel={text("공개 글 읽기", "Read public posts")} description={text("이 인스턴스는 공개 읽기 전용으로 배포되어 블로그를 만들 수 없습니다.", "This instance is deployed public read-only, so blogs cannot be created.")} title={text("읽기 전용 배포본입니다", "This deployment is read-only")} />;
  }

  if (!session) return <PageLoading label={text("계정을 확인하는 중", "Checking account")} />;
  if (session.state === "anonymous") {
    return <EmptyState actionHref="/login" actionLabel={text("관리자 인증", "Administrator authentication")} description={text("블로그를 만들려면 먼저 서버가 제공하는 방법으로 인증해야 합니다.", "Authenticate using a method provided by the server before creating a blog.")} title={text("당신의 공간을 준비할까요?", "Ready to prepare your space?")} />;
  }
  if (session.blog) {
    return <EmptyState actionHref={`/@${session.blog.handle}`} actionLabel={text("내 블로그 보기", "View my blog")} description={text("선택한 테마와 공개 글을 내 블로그에서 확인할 수 있습니다.", "View your selected theme and public posts on your blog.")} title={text("블로그가 이미 준비되어 있어요", "Your blog is already ready")} />;
  }
  const authenticatedSession = session;

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setStatus(text("블로그를 만드는 중…", "Creating blog…"));
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
      <header className="onboarding-heading">
        <p className="step-label">{text("블로그 만들기 · 1분이면 충분해요", "Create a blog · it only takes a minute")}</p>
        <h1>{uiLanguage === "en" ? <>What kind of space<br />do you want to make?</> : <>어떤 공간을<br />만들고 싶나요?</>}</h1>
        <p>{text("정보와 첫 테마를 고르세요. 콘텐츠는 테마와 독립적으로 언제나 Markdown으로 남습니다.", "Choose your details and first theme. Your content always remains theme-independent Markdown.")}</p>
      </header>
      <form className="onboarding-form" onSubmit={(event) => void submit(event)}>
        <section className="onboarding-section" aria-labelledby="blog-info-title">
          <div className="numbered-heading"><span>01</span><div><h2 id="blog-info-title">{text("블로그 정보", "Blog information")}</h2><p>{text("독자가 기억할 이름과 주소를 정합니다.", "Choose a memorable name and address for readers.")}</p></div></div>
          <div className="field-grid">
            <label>{text("블로그 이름", "Blog name")}<input defaultValue={text(`${session.user.displayName}의 기록`, `${session.user.displayName}'s notes`)} maxLength={100} name="title" required /></label>
            <label>{text("주소", "Address")}<span className="input-prefix"><span>/@</span><input defaultValue={session.user.handle} maxLength={40} name="handle" pattern="[a-z0-9]+(?:-[a-z0-9]+)*" required /></span></label>
          </div>
          <label>{text("한 줄 소개", "Short description")}<textarea maxLength={240} name="description" placeholder={text("무엇을 기록하는 공간인지 알려주세요.", "Tell readers what you record here.")} rows={3} /></label>
        </section>
        <section className="onboarding-section" aria-labelledby="theme-title">
          <div className="numbered-heading"><span>02</span><div><h2 id="theme-title">{text("첫 테마", "First theme")}</h2><p>{text("모든 프리셋은 읽기와 대비 기준을 통과한 로컬 CSS입니다.", "Every preset is local CSS that meets readability and contrast standards.")}</p></div></div>
          <fieldset className="theme-grid"><legend className="sr-only">{text("블로그 테마 선택", "Choose blog theme")}</legend>
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
        <div className="onboarding-actions"><p>{text("선택한 테마:", "Selected theme:")} <strong>{THEME_PRESETS.find((item) => item.id === theme)?.name}</strong></p><button className="button button-primary" disabled={busy} type="submit">{text("내 블로그 만들기", "Create my blog")} <span aria-hidden="true">→</span></button></div>
        {status ? <p className="inline-status" role="status">{status}</p> : null}
      </form>
    </div>
  );
}

export function NotFoundPage() {
  usePublicReaderContentStatus("error");
  usePageTitle(text("페이지 없음", "Page not found"));
  return <EmptyState actionHref="/" actionLabel={text("피드로 돌아가기", "Back to feed")} description={text("주소가 바뀌었거나 존재하지 않는 페이지입니다.", "The address may have changed, or this page does not exist.")} title={text("길을 잃은 것 같아요", "This page seems to be lost")} />;
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
      <h1>{text("서버 기능을 확인하지 못했습니다", "Could not check server capabilities")}</h1>
      <p>{detail}</p>
      <button className="button button-primary" onClick={onRetry} type="button">{text("다시 시도", "Try again")}</button>
    </section>
  );
}

function PageLoading({ label }: { label: string }) {
  return <div className="page-loading" role="status"><span aria-hidden="true" /><p>{label}…</p></div>;
}

function FeedSkeleton() {
  return <div aria-label={text("피드를 불러오는 중", "Loading feed")} className="post-grid" role="status">{[0, 1, 2].map((value) => <div className="post-card skeleton-card" key={value}><span /><span /><span /></div>)}</div>;
}

function formatCodeRun(response: CodeRunResponse): string {
  if (response.state === "queued") return text("실행기가 제한 시간 안에 완료되지 않았습니다.", "The runner did not finish within the time limit.");
  const { result } = response;
  const sections = [`${result.outcome}${result.exitCode === null ? "" : ` (exit ${result.exitCode})`}`];
  if (result.stdout) sections.push(`stdout:\n${result.stdout}`);
  if (result.stderr) sections.push(`stderr:\n${result.stderr}`);
  if (result.truncated) sections.push(text("정책에 따라 출력 일부가 잘렸습니다.", "Part of the output was truncated by policy."));
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
