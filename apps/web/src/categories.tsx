import {
  useEffect,
  useId,
  useState,
  type FormEvent,
} from "react";
import type {
  BlogCategoryResponse,
  BlogSeriesResponse,
  Capabilities,
  CategorySummary,
  CreateCategoryInput,
  FeedPostSummary,
  ThemePresetId,
  UpdateCategoryInput,
} from "@opensoverignblog/sdk";
import { AdminAccessKeyForm } from "./admin-access";
import { usePublicReaderContentStatus, useSession } from "./app";
import { adminAuthChoices, studioAccessFor } from "./auth-policy";
import { publicCategoryPath, publicCategoryPostPath } from "./article-location";
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
  text,
  usePageTitle,
} from "./lib";

export interface CategoryPageProps {
  handle: string;
  categorySlug: string;
  /** The provisioned on-premises blog owns short `/category` URLs. */
  primary?: boolean;
}

export function categoryHref(
  handle: string,
  categorySlug: string,
  primary = false,
): string {
  return publicCategoryPath({ handle, categorySlug, primary });
}

export function categoryPostHref(
  handle: string,
  categorySlug: string,
  postSlug: string,
  primary = false,
): string {
  return publicCategoryPostPath({ handle, categorySlug, postSlug, primary });
}

export function CategoryPage({
  handle,
  categorySlug,
  primary = false,
}: CategoryPageProps) {
  const [collection, setCollection] = useState<
    | { kind: "category"; page: BlogCategoryResponse }
    | { kind: "series"; page: BlogSeriesResponse }
  >();
  const [posts, setPosts] = useState<FeedPostSummary[]>([]);
  const [error, setError] = useState<string>();
  usePublicReaderContentStatus(
    error ? "error" : collection ? "ready" : "pending",
  );
  const collectionTitle = collection?.kind === "series"
    ? collection.page.series.title
    : collection?.page.category.title;
  usePageTitle(collection ? `${collectionTitle} · ${collection.page.blog.title}` : categorySlug);

  useEffect(() => {
    const controller = new AbortController();
    setCollection(undefined);
    setPosts([]);
    setError(undefined);
    void (async () => {
      try {
        const seriesPage = primary
          ? await client.getPrimarySeries(categorySlug, controller.signal)
          : await client.getBlogSeries(handle, categorySlug, controller.signal);
        const response = primary
          ? await client.getPrimarySeriesPosts(categorySlug, controller.signal)
          : await client.getBlogSeriesPosts(handle, categorySlug, controller.signal);
        return { kind: "series" as const, page: seriesPage, posts: response.items };
      } catch (reason) {
        if (!isNotFound(reason)) throw reason;
        const pageRequest = primary
          ? client.getPrimaryCategory(categorySlug, controller.signal)
          : client.getBlogCategory(handle, categorySlug, controller.signal);
        const postsRequest = primary
          ? client.getPrimaryCategoryPosts(categorySlug, controller.signal)
          : client.getBlogCategoryPosts(handle, categorySlug, controller.signal);
        const [page, response] = await Promise.all([pageRequest, postsRequest]);
        return { kind: "category" as const, page, posts: response.items };
      }
    })()
      .then((nextCollection) => {
        if (controller.signal.aborted) return;
        setCollection(nextCollection.kind === "series"
          ? { kind: "series", page: nextCollection.page }
          : { kind: "category", page: nextCollection.page });
        setPosts(nextCollection.posts);
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setError(asMessage(reason));
      });
    return () => controller.abort();
  }, [handle, categorySlug, primary]);

  if (error) {
    return (
      <section className="empty-state" role="alert">
        <span className="empty-symbol" aria-hidden="true">!</span>
        <h1>{text("글 묶음을 불러오지 못했습니다", "Could not load collection")}</h1>
        <p>{error}</p>
      </section>
    );
  }
  if (!collection) {
    return <div className="page-loading" role="status"><span aria-hidden="true" /><p>{text("글 묶음을 불러오는 중…", "Loading collection…")}</p></div>;
  }

  const { blog, postCount } = collection.page;
  const category = collection.kind === "series"
    ? collection.page.series
    : collection.page.category;
  const isSeries = collection.kind === "series";
  const customCssHref = safeBlogStylesheetUrl(
    blog.theme.customCssUrl,
    blog.handle,
    window.location.origin,
    basePath,
  );
  const primaryPath = blog.isPrimary;
  const blogHref = primaryPath ? "/" : `/@${encodeURIComponent(blog.handle)}`;
  const themePreset = category.themePreset ?? blog.theme.presetId;

  return (
    <>
      {customCssHref ? <link data-osb-blog-custom-css href={customCssHref} rel="stylesheet" /> : null}
      <div className="osb-site-frame">
        <div
          className="blog-page osb-site-theme"
          data-custom-css={customCssHref ? "enabled" : "disabled"}
          data-site-id={blog.id}
          data-theme={themePreset}
        >
          <header className="blog-profile">
            <span className="blog-monogram" aria-hidden="true">{initials(category.title)}</span>
            <div>
              <p className="blog-handle">
                <AppLink href={blogHref}>@{blog.handle}</AppLink>
                <span aria-hidden="true"> / </span>
                {category.slug}
              </p>
              <h1>{category.title}</h1>
              <p>{category.description || text("이 카테고리의 글을 한곳에 모았습니다.", "Posts in this category, collected in one place.")}</p>
              <div className="blog-owner">
                <span className="avatar" aria-hidden="true">{initials(blog.owner.displayName)}</span>
                <span>
                  <strong>{blog.owner.displayName}</strong>
                  <small>
                    {isSeries
                      ? text(`${blog.title}의 시리즈`, `Series in ${blog.title}`)
                      : text(`${blog.title}의 카테고리`, `Category in ${blog.title}`)}
                  </small>
                </span>
              </div>
            </div>
          </header>

          <section className="blog-posts" aria-labelledby="category-posts-title">
            <div className="section-heading">
              <div>
                <p className="eyebrow">{isSeries ? "Series" : "Category archive"}</p>
                <h2 id="category-posts-title">
                  {isSeries
                    ? text(`${category.title} 읽는 순서`, `${category.title} reading order`)
                    : text(`${category.title}의 글`, `Posts in ${category.title}`)}
                </h2>
              </div>
              <span className="result-count">{text(`${postCount}개`, `${postCount} posts`)}</span>
            </div>
            {posts.length ? (
              <div className="blog-list">
                {posts.map((post, index) => (
                  <CategoryPostRow
                    categorySlug={category.slug}
                    handle={blog.handle}
                    index={index}
                    key={post.id}
                    post={post}
                    primary={primaryPath}
                  />
                ))}
              </div>
            ) : (
              <div className="dashboard-empty">
                <span aria-hidden="true">□</span>
                <h3>{text("아직 발행된 글이 없습니다", "No published posts yet")}</h3>
                <p>
                  {isSeries
                    ? text("이 시리즈에 첫 글이 발행되면 여기에 나타납니다.", "The first post published in this series will appear here.")
                    : text("이 카테고리에 첫 글이 발행되면 여기에 나타납니다.", "The first post published in this category will appear here.")}
                </p>
              </div>
            )}
          </section>
        </div>
      </div>
    </>
  );
}

function CategoryPostRow({
  categorySlug,
  handle,
  index,
  post,
  primary,
}: {
  categorySlug: string;
  handle: string;
  index: number;
  post: FeedPostSummary;
  primary: boolean;
}) {
  const href = categoryPostHref(handle, post.category?.slug ?? categorySlug, post.slug, primary);
  return (
    <article className="blog-list-item">
      <span className="post-order" aria-hidden="true">{String(index + 1).padStart(2, "0")}</span>
      <div>
        <div className="post-card-meta">
          <time dateTime={post.publishedAt}>{formatDate(post.publishedAt)}</time>
          <span>{post.author.displayName}</span>
        </div>
        <h3><AppLink href={href}>{post.title}</AppLink></h3>
        <p>{post.excerpt}</p>
      </div>
      <span className="list-arrow" aria-hidden="true">↗</span>
    </article>
  );
}

export interface StudioCategoriesPageProps {
  capabilities: Capabilities | undefined;
  /** Use short public URLs when this Studio owns the provisioned primary site. */
  primary?: boolean;
}

type Notice = { kind: "success" | "error"; text: string };

export function StudioCategoriesPage({
  capabilities,
  primary = false,
}: StudioCategoriesPageProps) {
  const { session, capabilitiesError, refreshCapabilities } = useSession();
  const [categories, setCategories] = useState<CategorySummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string>();
  const [notice, setNotice] = useState<Notice>();
  const [busyId, setBusyId] = useState<string>();
  const [creating, setCreating] = useState(false);
  const [slug, setSlug] = useState("");
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [themePreset, setThemePreset] = useState<"" | ThemePresetId>("");
  usePageTitle(text("카테고리 관리", "Manage categories"));

  const studioAccess = capabilities ? studioAccessFor(capabilities) : undefined;
  const authenticated = session?.state === "authenticated" && Boolean(session.blog);
  const owner = authenticated && (!session.membershipRole || session.membershipRole === "owner");
  const canLoad = Boolean(studioAccess !== "disabled" && authenticated);

  useEffect(() => {
    if (!canLoad) {
      setLoading(false);
      return;
    }
    const controller = new AbortController();
    setLoading(true);
    setLoadError(undefined);
    void client.listStudioCategories(controller.signal)
      .then((response) => {
        if (!controller.signal.aborted) setCategories(response.items);
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setLoadError(asMessage(reason));
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [canLoad]);

  if (!capabilities && capabilitiesError) {
    return (
      <CategoryAccessGate
        detail={text(`서버 기능을 확인하지 못했습니다: ${capabilitiesError}`, `Could not check server capabilities: ${capabilitiesError}`)}
        retry={() => void refreshCapabilities()}
      />
    );
  }
  if (!capabilities || !session) {
    return <div className="dashboard-loading" role="status">{text("Studio 접근 권한을 확인하는 중…", "Checking Studio access…")}</div>;
  }
  if (studioAccess === "disabled") {
    return <CategoryAccessGate detail={text("이 인스턴스는 읽기 전용으로 배포되어 카테고리 Studio를 사용할 수 없습니다.", "This instance is deployed read-only, so category Studio is unavailable.")} />;
  }
  if (session.state !== "authenticated") {
    return <CategoryAccessGate detail={text("카테고리를 확인하려면 먼저 인증해 주세요.", "Authenticate before managing categories.")} login />;
  }
  if (!session.blog) {
    return <CategoryAccessGate detail={text("카테고리를 만들기 전에 블로그를 먼저 만들어 주세요.", "Create a blog before creating categories.")} onboarding />;
  }

  async function createCategory(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!owner || creating) return;
    setCreating(true);
    setNotice(undefined);
    const input: CreateCategoryInput = {
      slug: slug.trim(),
      title: title.trim(),
      ...optionalText("description", description),
      ...(themePreset ? { themePreset } : {}),
    };
    try {
      const created = await client.createStudioCategory(input);
      setCategories((current) => [...current, created]);
      setSlug("");
      setTitle("");
      setDescription("");
      setThemePreset("");
      setNotice({ kind: "success", text: text(`‘${created.title}’ 카테고리를 만들었습니다.`, `Created the “${created.title}” category.`) });
    } catch (reason) {
      setNotice({ kind: "error", text: asMessage(reason) });
    } finally {
      setCreating(false);
    }
  }

  async function updateCategory(categoryId: string, input: UpdateCategoryInput) {
    if (!owner || busyId) return;
    setBusyId(categoryId);
    setNotice(undefined);
    try {
      const updated = await client.updateStudioCategory(categoryId, input);
      replaceCategory(setCategories, updated);
      setNotice({ kind: "success", text: text(`‘${updated.title}’ 카테고리를 저장했습니다.`, `Saved the “${updated.title}” category.`) });
    } catch (reason) {
      setNotice({ kind: "error", text: asMessage(reason) });
      throw reason;
    } finally {
      setBusyId(undefined);
    }
  }

  async function archiveCategory(categoryId: string) {
    if (!owner || busyId) return;
    setBusyId(categoryId);
    setNotice(undefined);
    try {
      const archived = await client.archiveStudioCategory(categoryId);
      replaceCategory(setCategories, archived);
      setNotice({ kind: "success", text: text(`‘${archived.title}’ 카테고리를 보관했습니다.`, `Archived the “${archived.title}” category.`) });
    } catch (reason) {
      setNotice({ kind: "error", text: asMessage(reason) });
      throw reason;
    } finally {
      setBusyId(undefined);
    }
  }

  const activeCount = categories.filter((category) => category.status === "active").length;

  return (
    <div className="studio-settings-page">
      <header className="settings-heading">
        <div>
          <p className="eyebrow">Information architecture</p>
          <h1>{text("카테고리", "Categories")}</h1>
          <p>{text("글쓰기와 분리된 공간에서 주제별 공개 주소와 테마를 관리합니다.", "Manage topic-specific public addresses and themes separately from writing.")}</p>
        </div>
        <AppLink className="button button-ghost" href="/studio">{text("Studio로 돌아가기", "Back to Studio")}</AppLink>
      </header>

      {!owner ? (
        <section className="settings-feature-notice" aria-labelledby="category-readonly-title">
          <span aria-hidden="true">i</span>
          <div>
            <h2 id="category-readonly-title">{text("보기 전용 권한입니다", "You have view-only access")}</h2>
            <p>{text("작성자와 편집자는 카테고리를 확인하고 글에 사용할 수 있습니다. 생성·수정·보관은 블로그 소유자만 할 수 있습니다.", "Authors and editors can view and use categories. Only the blog owner can create, edit, or archive them.")}</p>
          </div>
        </section>
      ) : (
        <section className="settings-panel" aria-labelledby="new-category-title">
          <div className="settings-panel-heading">
            <div><span className="settings-step">01</span><div><h2 id="new-category-title">{text("새 카테고리", "New category")}</h2><p>{text("주소는 생성 후 바뀌지 않습니다. 짧고 오래 쓸 영문 주소를 선택하세요.", "The address cannot be changed after creation. Choose a short, durable URL slug.")}</p></div></div>
          </div>
          <form className="onboarding-form" onSubmit={(event) => void createCategory(event)}>
            <div className="field-grid">
              <label>
                {text("공개 주소", "Public address")}
                <span className="input-prefix"><span>/</span><input
                  autoCapitalize="none"
                  autoComplete="off"
                  inputMode="url"
                  maxLength={40}
                  onChange={(event) => setSlug(event.target.value.toLowerCase())}
                  pattern="[a-z0-9]+(?:-[a-z0-9]+)*"
                  placeholder="yangja"
                  required
                  value={slug}
                /></span>
              </label>
              <label>
                {text("표시 이름", "Display name")}
                <input maxLength={200} onChange={(event) => setTitle(event.target.value)} placeholder={text("양자", "Quantum")} required value={title} />
              </label>
            </div>
            <label>
              {text("설명", "Description")} <span className="field-hint">{text("선택", "optional")}</span>
              <textarea maxLength={2000} onChange={(event) => setDescription(event.target.value)} rows={3} value={description} />
            </label>
            <ThemePresetSelect onChange={setThemePreset} value={themePreset} />
            <div className="settings-save-row">
              <p>{slug ? <>{text("예상 공개 주소:", "Public address:")} <code>{categoryHref(session.blog.handle, slug, primary)}</code></> : text("영문 소문자, 숫자, 중간 하이픈만 사용할 수 있습니다.", "Use lowercase letters, numbers, and hyphens between words.")}</p>
              <button className="button button-primary" disabled={creating} type="submit">{creating ? text("만드는 중…", "Creating…") : text("카테고리 만들기", "Create category")}</button>
            </div>
          </form>
        </section>
      )}

      <section className="settings-panel" aria-labelledby="category-list-title" aria-busy={loading}>
        <div className="settings-panel-heading">
          <div><span className="settings-step">02</span><div><h2 id="category-list-title">{text("카테고리 목록", "Category list")}</h2><p>{text(`활성 ${activeCount}개 · 전체 ${categories.length}개`, `${activeCount} active · ${categories.length} total`)}</p></div></div>
        </div>
        {loading ? <div className="settings-loading" role="status">{text("카테고리를 불러오는 중…", "Loading categories…")}</div> : null}
        {loadError ? <p className="settings-message is-error" role="alert">{loadError}</p> : null}
        {!loading && !loadError && categories.length === 0 ? (
          <div className="dashboard-empty"><span aria-hidden="true">◇</span><h3>{text("아직 카테고리가 없습니다", "No categories yet")}</h3><p>{owner ? text("첫 카테고리를 만들어 보세요.", "Create your first category.") : text("소유자가 카테고리를 만들면 여기에 나타납니다.", "Categories created by the owner will appear here.")}</p></div>
        ) : null}
        {categories.length ? (
          <div className="document-cards">
            {categories.map((category) => (
              <StudioCategoryCard
                busy={busyId === category.id}
                category={category}
                handle={session.blog?.handle ?? ""}
                key={category.id}
                onArchive={archiveCategory}
                onUpdate={updateCategory}
                owner={owner}
                primary={primary}
              />
            ))}
          </div>
        ) : null}
      </section>

      {notice ? <p className={`settings-message is-${notice.kind}`} role={notice.kind === "error" ? "alert" : "status"}>{notice.text}</p> : null}
    </div>
  );
}

function StudioCategoryCard({
  busy,
  category,
  handle,
  onArchive,
  onUpdate,
  owner,
  primary,
}: {
  busy: boolean;
  category: CategorySummary;
  handle: string;
  onArchive: (categoryId: string) => Promise<void>;
  onUpdate: (categoryId: string, input: UpdateCategoryInput) => Promise<void>;
  owner: boolean;
  primary: boolean;
}) {
  const titleId = useId();
  const descriptionId = useId();
  const themeId = useId();
  const [title, setTitle] = useState(category.title);
  const [description, setDescription] = useState(category.description ?? "");
  const [themePreset, setThemePreset] = useState<"" | ThemePresetId>(category.themePreset ?? "");
  const [confirmArchive, setConfirmArchive] = useState(false);

  useEffect(() => {
    setTitle(category.title);
    setDescription(category.description ?? "");
    setThemePreset(category.themePreset ?? "");
    setConfirmArchive(false);
  }, [category]);

  const publicHref = categoryHref(handle, category.slug, primary);
  const archived = category.status === "archived";

  if (!owner || archived) {
    return (
      <article className="document-card">
        <div className="document-status-row">
          <span className={`status-badge status-${archived ? "archived" : "published"}`}>{archived ? text("보관됨", "Archived") : text("활성", "Active")}</span>
          <code>/{category.slug}</code>
        </div>
        <h3>{category.title}</h3>
        <p>{category.description || text("설명이 없습니다.", "No description.")}</p>
        <div className="document-card-footer">
          <span>{themeLabel(category.themePreset)}</span>
          {!archived ? <AppLink href={publicHref}>{text("공개 페이지", "Public page")} <span aria-hidden="true">↗</span></AppLink> : <span>{text("새 글 지정 불가", "Cannot assign new posts")}</span>}
        </div>
      </article>
    );
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    try {
      await onUpdate(category.id, {
        title: title.trim(),
        ...optionalText("description", description),
        ...(themePreset ? { themePreset } : {}),
      });
    } catch {
      // The parent owns the shared, announced error state.
    }
  }

  async function archive() {
    try {
      await onArchive(category.id);
    } catch {
      // The parent owns the shared, announced error state.
    }
  }

  return (
    <article className="document-card" aria-busy={busy}>
      <div className="document-status-row">
        <span className="status-badge status-published">{text("활성", "Active")}</span>
        <AppLink href={publicHref}>/{category.slug} <span aria-hidden="true">↗</span></AppLink>
      </div>
      <form className="auth-form" onSubmit={(event) => void submit(event)}>
        <label htmlFor={titleId}>{text("표시 이름", "Display name")}</label>
        <input id={titleId} maxLength={200} onChange={(event) => setTitle(event.target.value)} required value={title} />
        <label htmlFor={descriptionId}>{text("설명", "Description")}</label>
        <textarea id={descriptionId} maxLength={2000} onChange={(event) => setDescription(event.target.value)} rows={3} value={description} />
        <label htmlFor={themeId}>{text("테마", "Theme")}</label>
        <select id={themeId} onChange={(event) => setThemePreset(event.target.value as "" | ThemePresetId)} value={themePreset}>
          <option value="">{text("블로그 기본 테마 상속", "Inherit blog theme")}</option>
          {THEME_PRESETS.map((preset) => <option key={preset.id} value={preset.id}>{preset.name}</option>)}
        </select>
        <div className="document-card-footer">
          <button className="button button-primary" disabled={busy} type="submit">{busy ? text("처리 중…", "Working…") : text("변경 저장", "Save changes")}</button>
          <button className="button button-ghost collaborator-remove" disabled={busy} onClick={() => setConfirmArchive(true)} type="button">{text("보관", "Archive")}</button>
        </div>
        {confirmArchive ? (
          <div className="collaborator-remove-confirm" role="group" aria-label={text(`${category.title} 카테고리 보관 확인`, `Confirm archiving ${category.title}`)}>
            <p>{text("보관하면 기존 공개 글은 유지되지만 새 글을 이 카테고리에 지정할 수 없습니다.", "Archiving preserves existing public posts but prevents assigning new posts to this category.")}</p>
            <div>
              <button className="button button-ghost" disabled={busy} onClick={() => setConfirmArchive(false)} type="button">{text("취소", "Cancel")}</button>
              <button className="button button-danger" disabled={busy} onClick={() => void archive()} type="button">{text("보관 확인", "Archive category")}</button>
            </div>
          </div>
        ) : null}
      </form>
    </article>
  );
}

function ThemePresetSelect({
  onChange,
  value,
}: {
  onChange: (value: "" | ThemePresetId) => void;
  value: "" | ThemePresetId;
}) {
  const id = useId();
  return (
    <label htmlFor={id}>
      {text("카테고리 테마", "Category theme")} <span className="field-hint">{text("선택", "optional")}</span>
      <select id={id} onChange={(event) => onChange(event.target.value as "" | ThemePresetId)} value={value}>
        <option value="">{text("블로그 기본 테마 상속", "Inherit blog theme")}</option>
        {THEME_PRESETS.map((preset) => <option key={preset.id} value={preset.id}>{preset.name} — {preset.description}</option>)}
      </select>
    </label>
  );
}

function CategoryAccessGate({
  detail,
  login = false,
  onboarding = false,
  retry,
}: {
  detail: string;
  login?: boolean;
  onboarding?: boolean;
  retry?: () => void;
}) {
  const { capabilities, setSession } = useSession();
  const accessKeyMethod = login && capabilities
    ? adminAuthChoices(capabilities).accessKeyMethods[0]
    : undefined;
  const localAccountLogin = Boolean(
    capabilities && studioAccessFor(capabilities) === "members",
  );
  return (
    <section className="empty-state studio-access-gate" role="alert">
      <span className="empty-symbol" aria-hidden="true">◇</span>
      <h1>{text("카테고리 Studio", "Category Studio")}</h1>
      <p>{detail}</p>
      {accessKeyMethod ? (
        <div className="studio-inline-admin-access">
          <AdminAccessKeyForm method={accessKeyMethod} onAuthenticated={setSession} />
          {localAccountLogin ? <AppLink className="button button-ghost" href="/login">{text("계정 로그인", "Account login")}</AppLink> : null}
        </div>
      ) : login ? <AppLink className="button button-primary" href="/login">{text("로그인", "Log in")}</AppLink> : null}
      {onboarding ? <AppLink className="button button-primary" href="/onboarding">{text("블로그 만들기", "Create blog")}</AppLink> : null}
      {retry ? <button className="button button-primary" onClick={retry} type="button">{text("다시 시도", "Try again")}</button> : null}
    </section>
  );
}

function optionalText<Key extends string>(key: Key, value: string): { [Property in Key]?: string } {
  const normalized = value.trim();
  return (normalized ? { [key]: normalized } : {}) as { [Property in Key]?: string };
}

function replaceCategory(
  setCategories: (update: (current: CategorySummary[]) => CategorySummary[]) => void,
  updated: CategorySummary,
) {
  setCategories((current) => current.map((category) => (
    category.id === updated.id ? updated : category
  )));
}

function themeLabel(themePreset: ThemePresetId | undefined): string {
  if (!themePreset) return text("블로그 기본 테마", "Blog default theme");
  return THEME_PRESETS.find((preset) => preset.id === themePreset)?.name ?? themePreset;
}
