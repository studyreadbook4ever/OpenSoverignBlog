import {
  createContext,
  lazy,
  Suspense,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import type { Capabilities, Session, VersionInfo } from "@opensoverignblog/sdk";
import { studioAccessFor } from "./auth-policy";
import { AppLink, asMessage, client, initials, isNotFound, navigate, publicPath, usePathname } from "./lib";
import {
  ArticlePage,
  BlogPage,
  FeedPage,
  LoginPage,
  NotFoundPage,
  OnboardingPage,
} from "./public-pages";

const StudioDashboard = lazy(async () => ({
  default: (await import("./studio")).StudioDashboard,
}));
const StudioEditor = lazy(async () => ({
  default: (await import("./studio")).StudioEditor,
}));
const StudioSettingsPage = lazy(async () => ({
  default: (await import("./studio")).StudioSettingsPage,
}));
const AdminTreePage = lazy(async () => ({
  default: (await import("./admin-tree")).AdminTreePage,
}));
const CategoryPage = lazy(async () => ({
  default: (await import("./categories")).CategoryPage,
}));
const StudioCategoriesPage = lazy(async () => ({
  default: (await import("./categories")).StudioCategoriesPage,
}));

interface SessionContextValue {
  session: Session | undefined;
  capabilities: Capabilities | undefined;
  capabilitiesError?: string;
  sessionError?: string;
  setSession: (session: Session) => void;
  refreshCapabilities: () => Promise<void>;
  refreshSession: () => Promise<void>;
}

const SessionContext = createContext<SessionContextValue | undefined>(undefined);

export function useSession(): SessionContextValue {
  const value = useContext(SessionContext);
  if (!value) throw new Error("SessionContext is unavailable");
  return value;
}

export function App() {
  const pathname = usePathname();
  const [session, setSession] = useState<Session>();
  const [capabilities, setCapabilities] = useState<Capabilities>();
  const [capabilitiesError, setCapabilitiesError] = useState<string>();
  const [sessionError, setSessionError] = useState<string>();
  const mainRef = useRef<HTMLElement>(null);

  async function refreshSession() {
    try {
      const value = await client.session();
      setSession(value);
      setSessionError(undefined);
    } catch (reason) {
      if (isNotFound(reason)) {
        setSession({ state: "anonymous", registrationOpen: false });
        return;
      }
      setSession({ state: "anonymous", registrationOpen: false });
      setSessionError(asMessage(reason));
    }
  }

  async function refreshCapabilities() {
    try {
      setCapabilities(await client.capabilities());
      setCapabilitiesError(undefined);
    } catch (reason) {
      setCapabilities(undefined);
      setCapabilitiesError(asMessage(reason));
    }
  }

  useEffect(() => {
    const controller = new AbortController();
    void client.capabilities(controller.signal)
      .then((value) => {
        setCapabilities(value);
        setCapabilitiesError(undefined);
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setCapabilitiesError(asMessage(reason));
      });
    void client
      .session(controller.signal)
      .then((value) => {
        setSession(value);
        setSessionError(undefined);
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted) return;
        if (!isNotFound(reason)) setSessionError(asMessage(reason));
        setSession({
          state: "anonymous",
          registrationOpen: false,
        });
      });
    return () => controller.abort();
  }, []);

  useEffect(() => {
    window.setTimeout(() => mainRef.current?.focus({ preventScroll: true }), 0);
  }, [pathname]);

  const context = useMemo<SessionContextValue>(
    () => ({
      session,
      capabilities,
      ...(capabilitiesError ? { capabilitiesError } : {}),
      ...(sessionError ? { sessionError } : {}),
      setSession,
      refreshCapabilities,
      refreshSession,
    }),
    [session, capabilities, capabilitiesError, sessionError],
  );

  const editorMatch = pathname.match(/^\/studio\/write(?:\/([^/]+))?\/?$/);
  const editorDocumentId = editorMatch?.[1] ? decodePathSegment(editorMatch[1]) : undefined;
  const page = resolvePage(pathname, capabilities);

  return (
    <SessionContext.Provider value={context}>
      <a className="skip-link" href="#main-content">
        본문으로 건너뛰기
      </a>
      {editorMatch ? (
        <main className="editor-route" id="main-content" ref={mainRef} tabIndex={-1}>
          <Suspense fallback={<RouteLoading label="Studio 편집기를 불러오는 중" />}>
            {session && capabilities ? (
              <StudioEditor
                capabilities={capabilities}
                documentId={editorDocumentId}
                key={`${session.state === "authenticated" ? session.user.id : "legacy"}:${editorDocumentId ?? "new"}`}
              />
            ) : capabilitiesError ? (
              <CapabilityError detail={capabilitiesError} onRetry={() => void refreshCapabilities()} />
            ) : <RouteLoading label="Studio 접근 권한을 확인하는 중" />}
          </Suspense>
        </main>
      ) : (
        <>
          <SiteHeader />
          <main className="route-main" id="main-content" ref={mainRef} tabIndex={-1}>
            {page}
          </main>
          <SiteFooter />
        </>
      )}
    </SessionContext.Provider>
  );
}

function resolvePage(pathname: string, capabilities: Capabilities | undefined): ReactNode {
  if (pathname === "/" || pathname === "/index.html") return <FeedPage />;
  if (pathname === "/login") return <LoginPage />;
  if (pathname === "/onboarding") return <OnboardingPage />;
  if (pathname === "/studio" || pathname === "/studio/") {
    return <Suspense fallback={<RouteLoading label="Studio를 불러오는 중" />}><StudioDashboard capabilities={capabilities} /></Suspense>;
  }
  if (pathname === "/studio/settings" || pathname === "/studio/settings/") {
    return <Suspense fallback={<RouteLoading label="블로그 설정을 불러오는 중" />}><StudioSettingsPage capabilities={capabilities} /></Suspense>;
  }
  if (pathname === "/studio/categories" || pathname === "/studio/categories/") {
    return <StudioCategoriesRoute capabilities={capabilities} />;
  }
  if (pathname === "/studio/system" || pathname === "/studio/system/") {
    return <Suspense fallback={<RouteLoading label="프로그램 트리를 불러오는 중" />}><AdminTreePage /></Suspense>;
  }
  if (pathname.startsWith("/studio/")) return <NotFoundPage />;
  const memberCategoryArticle = pathname.match(/^\/@([^/]+)\/([^/]+)\/([^/]+)\/?$/);
  if (memberCategoryArticle) {
    return (
      <ArticlePage
        capabilities={capabilities}
        categorySlug={decodePathSegment(memberCategoryArticle[2] ?? "")}
        handle={decodePathSegment(memberCategoryArticle[1] ?? "")}
        slug={decodePathSegment(memberCategoryArticle[3] ?? "")}
      />
    );
  }
  const memberArticleOrCategory = pathname.match(/^\/@([^/]+)\/([^/]+)\/?$/);
  if (memberArticleOrCategory) {
    return (
      <MemberCategoryOrArticlePage
        capabilities={capabilities}
        handle={decodePathSegment(memberArticleOrCategory[1] ?? "")}
        segment={decodePathSegment(memberArticleOrCategory[2] ?? "")}
      />
    );
  }
  const blog = pathname.match(/^\/@([^/]+)\/?$/);
  if (blog) return <BlogPage handle={decodePathSegment(blog[1] ?? "")} />;
  const legacyArticle = pathname.match(/^\/blog\/([^/]+)\/?$/);
  if (legacyArticle) {
    return (
      <ArticlePage
        capabilities={capabilities}
        handle="open-soverign"
        legacy
        slug={decodePathSegment(legacyArticle[1] ?? "")}
      />
    );
  }
  const primaryCategoryArticle = pathname.match(/^\/([^/@][^/]*)\/([^/]+)\/?$/);
  if (primaryCategoryArticle) {
    return (
      <ArticlePage
        capabilities={capabilities}
        categorySlug={decodePathSegment(primaryCategoryArticle[1] ?? "")}
        handle=""
        primary
        slug={decodePathSegment(primaryCategoryArticle[2] ?? "")}
      />
    );
  }
  const primaryCategory = pathname.match(/^\/([^/@][^/]*)\/?$/);
  if (primaryCategory) {
    return (
      <Suspense fallback={<RouteLoading label="카테고리를 불러오는 중" />}>
        <CategoryPage categorySlug={decodePathSegment(primaryCategory[1] ?? "")} handle="" primary />
      </Suspense>
    );
  }
  return <NotFoundPage />;
}

function StudioCategoriesRoute({ capabilities }: { capabilities: Capabilities | undefined }) {
  const { session } = useSession();
  const primary = session?.state === "authenticated" && Boolean(session.blog?.isPrimary);
  return (
    <Suspense fallback={<RouteLoading label="카테고리를 불러오는 중" />}>
      <StudioCategoriesPage capabilities={capabilities} primary={primary} />
    </Suspense>
  );
}

function MemberCategoryOrArticlePage({
  capabilities,
  handle,
  segment,
}: {
  capabilities: Capabilities | undefined;
  handle: string;
  segment: string;
}) {
  const [resolution, setResolution] = useState<"loading" | "category" | "article" | "error">("loading");
  const [detail, setDetail] = useState<string>();

  useEffect(() => {
    const controller = new AbortController();
    setResolution("loading");
    setDetail(undefined);
    void client.getBlogCategory(handle, segment, controller.signal)
      .then(() => {
        if (!controller.signal.aborted) setResolution("category");
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted) return;
        if (isNotFound(reason)) {
          setResolution("article");
          return;
        }
        setDetail(asMessage(reason));
        setResolution("error");
      });
    return () => controller.abort();
  }, [handle, segment]);

  if (resolution === "loading") return <RouteLoading label="공개 주소를 확인하는 중" />;
  if (resolution === "error") {
    return <CapabilityError detail={detail ?? "공개 주소를 확인하지 못했습니다."} onRetry={() => window.location.reload()} />;
  }
  if (resolution === "category") {
    return (
      <Suspense fallback={<RouteLoading label="카테고리를 불러오는 중" />}>
        <CategoryPage categorySlug={segment} handle={handle} />
      </Suspense>
    );
  }
  return <ArticlePage capabilities={capabilities} handle={handle} slug={segment} />;
}

function decodePathSegment(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

function RouteLoading({ label }: { label: string }) {
  return <div className="page-loading" role="status"><span aria-hidden="true" /><p>{label}…</p></div>;
}

function CapabilityError({ detail, onRetry }: { detail: string; onRetry: () => void }) {
  return (
    <section className="empty-state studio-access-gate" role="alert">
      <span className="empty-symbol" aria-hidden="true">!</span>
      <h1>서버 기능을 확인하지 못했습니다</h1>
      <p>{detail}</p>
      <button className="button button-primary" onClick={onRetry} type="button">다시 시도</button>
    </section>
  );
}

function SiteHeader() {
  const { session, capabilities, capabilitiesError, refreshCapabilities, setSession } = useSession();
  const [busy, setBusy] = useState(false);
  const studioAccess = capabilities ? studioAccessFor(capabilities) : undefined;
  const deliveryOnly = studioAccess === "disabled";

  async function logout() {
    setBusy(true);
    try {
      setSession(await client.logout());
      navigate("/");
    } catch (reason) {
      if (isNotFound(reason)) {
        setSession({ state: "anonymous", registrationOpen: false });
        navigate("/");
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <header className="site-header">
      <div className="site-header-inner">
        <AppLink className="brand" href="/" aria-label="OpenSoverignBlog 홈">
          <span className="brand-mark" aria-hidden="true">
            OS
          </span>
          <span>OpenSoverignBlog</span>
        </AppLink>
        <nav className="primary-nav" aria-label="주요 메뉴">
          <AppLink href="/">피드</AppLink>
          {session?.state === "authenticated" && session.blog ? (
            <AppLink href={`/@${session.blog.handle}`}>
              {!session.membershipRole || session.membershipRole === "owner" ? "내 블로그" : "참여 블로그"}
            </AppLink>
          ) : null}
          {session?.state === "authenticated" && session.blog ? (
            <AppLink href="/studio">Studio</AppLink>
          ) : null}
          <a href={publicPath("/AI2AI.md")}>AI2AI</a>
        </nav>
        <div className="header-actions">
          {!capabilities ? (
            <button className="button button-ghost" onClick={() => void refreshCapabilities()} title={capabilitiesError} type="button">
              {capabilitiesError ? "기능 확인 재시도" : "서버 확인 중"}
            </button>
          ) : deliveryOnly ? (
            <span className="mode-pill">읽기 전용</span>
          ) : session?.state === "authenticated" ? (
            <>
              {session.blog && (!session.membershipRole || session.membershipRole === "owner") ? (
                <AppLink className="button button-ghost header-settings" href="/studio/settings" aria-label="블로그 설정">
                  <span aria-hidden="true">⚙</span><span className="header-settings-label">설정</span>
                </AppLink>
              ) : null}
              <AppLink className="button button-primary header-write" href="/studio/write">
                새 글 쓰기
              </AppLink>
              <div className="session-chip">
                <span className="avatar avatar-small" aria-hidden="true">
                  {initials(session.user.displayName)}
                </span>
                <span className="session-name">{session.user.displayName}</span>
                <button disabled={busy} onClick={() => void logout()} type="button">
                  로그아웃
                </button>
              </div>
            </>
          ) : (
            <AppLink className="button button-primary" href="/login">
              {studioAccess === "admin_only" ? "관리자 접근" : "로그인"}
            </AppLink>
          )}
        </div>
      </div>
    </header>
  );
}

function SiteFooter() {
  const [version, setVersion] = useState<VersionInfo>();
  useEffect(() => {
    const controller = new AbortController();
    void client.version(controller.signal).then(setVersion).catch(() => undefined);
    return () => controller.abort();
  }, []);

  return (
    <footer className="site-footer">
      <div className="footer-project">
        <strong>OpenSoverignBlog</strong>
        <p>당신의 Markdown, 당신의 서버, 당신의 기록.</p>
        <p className="footer-version">
          <span>현재 버전 v{version?.currentVersion ?? "0.1.0"}</span>
          <span>출시일 {version?.currentReleaseDate ?? "미출시"}</span>
          {version?.latestVersion ? <span>최신 버전 v{version.latestVersion}{version.latestReleaseDate ? ` (${version.latestReleaseDate})` : ""}{version.updateAvailable ? " · 업데이트 가능" : ""}</span> : null}
        </p>
      </div>
      <div className="footer-links">
        <a href={publicPath("/.well-known/open-soverign-blog.json")}>Discovery</a>
        <a href={publicPath("/openapi/openapi.yaml")}>OpenAPI</a>
        <a href={publicPath("/AI2AI.md")}>AI2AI</a>
        <a href={publicPath(version?.licenseHref ?? "/UNLICENSE")}>Unlicense</a>
        <a className="footer-github" href={version?.repositoryUrl ?? "https://github.com/studyreadbook4ever/OpenSoverignBlog"} rel="noreferrer" target="_blank">
          <svg aria-hidden="true" viewBox="0 0 24 24"><path d="M12 .7a11.5 11.5 0 0 0-3.64 22.4c.58.1.79-.25.79-.56v-2.23c-3.23.7-3.91-1.37-3.91-1.37-.53-1.34-1.29-1.7-1.29-1.7-1.05-.72.08-.7.08-.7 1.17.08 1.78 1.2 1.78 1.2 1.04 1.77 2.72 1.26 3.39.96.1-.75.4-1.26.74-1.55-2.58-.29-5.29-1.29-5.29-5.7 0-1.26.45-2.29 1.2-3.1-.12-.3-.52-1.48.11-3.07 0 0 .98-.31 3.16 1.18a10.9 10.9 0 0 1 5.76 0c2.2-1.5 3.17-1.18 3.17-1.18.63 1.6.23 2.78.11 3.07.75.81 1.2 1.84 1.2 3.1 0 4.43-2.72 5.4-5.3 5.7.42.36.78 1.07.78 2.16v3.2c0 .31.21.67.8.56A11.5 11.5 0 0 0 12 .7Z" /></svg>
          GitHub
        </a>
        <a aria-label="개발자 홈페이지 eff0rtchung.kr" className="footer-home-link" href={version?.developerUrl ?? "https://eff0rtchung.kr"} rel="noreferrer" target="_blank">
          <svg aria-hidden="true" viewBox="0 0 24 24"><path d="M3 10.8 12 3l9 7.8v9.7a.5.5 0 0 1-.5.5H15v-6H9v6H3.5a.5.5 0 0 1-.5-.5v-9.7Z" /></svg>
          eff0rtchung.kr
        </a>
      </div>
    </footer>
  );
}
