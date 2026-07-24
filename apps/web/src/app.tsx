import {
  createContext,
  lazy,
  Suspense,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import type { Capabilities, Session, VersionInfo } from "@opensoverignblog/sdk";
import { adminAuthChoices, studioAccessFor } from "./auth-policy";
import { AppLink, asMessage, client, initials, isNotFound, navigate, publicPath, text, usePathname } from "./lib";
import {
  ArticlePage,
  BlogPage,
  FeedPage,
  LoginPage,
  NotFoundPage,
  OnboardingPage,
  ReferencesPage,
} from "./public-pages";
import {
  AdvertisingSettingsButton,
  KakaoAdFitProvider,
  KakaoAdFitSlot,
} from "./kakao-adfit";
import { isReferencesPath } from "./references";

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
const StudioCreatePage = lazy(async () => ({
  default: (await import("./series")).StudioCreatePage,
}));
const StudioSeriesPage = lazy(async () => ({
  default: (await import("./series")).StudioSeriesPage,
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

export type PublicReaderContentStatus = "pending" | "ready" | "error";

interface PublicReaderContentState {
  pathname: string;
  status: PublicReaderContentStatus;
}

const PublicReaderContentContext = createContext<
  ((status: PublicReaderContentStatus) => void) | undefined
>(undefined);

export function useSession(): SessionContextValue {
  const value = useContext(SessionContext);
  if (!value) throw new Error("SessionContext is unavailable");
  return value;
}

export function usePublicReaderContentStatus(
  status: PublicReaderContentStatus | undefined,
) {
  const report = useContext(PublicReaderContentContext);
  useLayoutEffect(() => {
    if (!report || !status) return undefined;
    report(status);
    return () => report("pending");
  }, [report, status]);
}

export function App() {
  const pathname = usePathname();
  const [session, setSession] = useState<Session>();
  const [capabilities, setCapabilities] = useState<Capabilities>();
  const [capabilitiesError, setCapabilitiesError] = useState<string>();
  const [sessionError, setSessionError] = useState<string>();
  const [readerContent, setReaderContent] = useState<PublicReaderContentState>({
    pathname,
    status: "pending",
  });
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
  const reportReaderContent = useCallback(
    (status: PublicReaderContentStatus) => {
      setReaderContent((current) => (
        current.pathname === pathname && current.status === status
          ? current
          : { pathname, status }
      ));
    },
    [pathname],
  );
  const readerContentReady = readerContent.pathname === pathname
    && readerContent.status === "ready";

  return (
    <SessionContext.Provider value={context}>
      <a className="skip-link" href="#main-content">
        {text("본문으로 건너뛰기", "Skip to content")}
      </a>
      {editorMatch ? (
        <main className="editor-route" id="main-content" ref={mainRef} tabIndex={-1}>
          <Suspense fallback={<RouteLoading label={text("Studio 편집기를 불러오는 중", "Loading Studio editor")} />}>
            {session && capabilities ? (
              <StudioEditor
                capabilities={capabilities}
                documentId={editorDocumentId}
                key={`${session.state === "authenticated" ? session.user.id : "legacy"}:${editorDocumentId ?? "new"}`}
              />
            ) : capabilitiesError ? (
              <CapabilityError detail={capabilitiesError} onRetry={() => void refreshCapabilities()} />
            ) : <RouteLoading label={text("Studio 접근 권한을 확인하는 중", "Checking Studio access")} />}
          </Suspense>
        </main>
      ) : (
        <KakaoAdFitProvider
          advertising={capabilities?.advertising}
          contentReady={readerContentReady}
          pathname={pathname}
        >
          <SiteHeader />
          <KakaoAdFitSlot placement="top" />
          <main className="route-main" id="main-content" ref={mainRef} tabIndex={-1}>
            <PublicReaderContentContext.Provider value={reportReaderContent}>
              {page}
            </PublicReaderContentContext.Provider>
          </main>
          <KakaoAdFitSlot placement="bottom" />
          <SiteFooter />
        </KakaoAdFitProvider>
      )}
    </SessionContext.Provider>
  );
}

function resolvePage(pathname: string, capabilities: Capabilities | undefined): ReactNode {
  if (pathname === "/" || pathname === "/index.html") return <FeedPage />;
  if (pathname === "/login") return <LoginPage />;
  if (pathname === "/onboarding") return <OnboardingPage />;
  if (capabilities?.references && isReferencesPath(pathname)) {
    return <ReferencesPage capabilities={capabilities} />;
  }
  if (pathname === "/studio" || pathname === "/studio/") {
    return <Suspense fallback={<RouteLoading label={text("Studio를 불러오는 중", "Loading Studio")} />}><StudioDashboard capabilities={capabilities} /></Suspense>;
  }
  if (pathname === "/studio/settings" || pathname === "/studio/settings/") {
    return <Suspense fallback={<RouteLoading label={text("블로그 설정을 불러오는 중", "Loading blog settings")} />}><StudioSettingsPage capabilities={capabilities} /></Suspense>;
  }
  if (pathname === "/studio/categories" || pathname === "/studio/categories/") {
    return <StudioCategoriesRoute capabilities={capabilities} />;
  }
  if (pathname === "/studio/new" || pathname === "/studio/new/") {
    return <Suspense fallback={<RouteLoading label={text("콘텐츠 작성 화면을 불러오는 중", "Loading content creation")} />}><StudioCreatePage capabilities={capabilities} /></Suspense>;
  }
  if (pathname === "/studio/series" || pathname === "/studio/series/") {
    return <Suspense fallback={<RouteLoading label={text("시리즈를 불러오는 중", "Loading series")} />}><StudioSeriesPage capabilities={capabilities} /></Suspense>;
  }
  if (pathname === "/studio/system" || pathname === "/studio/system/") {
    return <Suspense fallback={<RouteLoading label={text("프로그램 트리를 불러오는 중", "Loading program tree")} />}><AdminTreePage /></Suspense>;
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
      <Suspense fallback={<RouteLoading label={text("카테고리를 불러오는 중", "Loading category")} />}>
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
    <Suspense fallback={<RouteLoading label={text("카테고리를 불러오는 중", "Loading categories")} />}>
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
  usePublicReaderContentStatus(
    resolution === "error"
      ? "error"
      : resolution === "loading" ? "pending" : undefined,
  );

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

  if (resolution === "loading") return <RouteLoading label={text("공개 주소를 확인하는 중", "Checking public address")} />;
  if (resolution === "error") {
    return <CapabilityError detail={detail ?? text("공개 주소를 확인하지 못했습니다.", "Could not resolve the public address.")} onRetry={() => window.location.reload()} />;
  }
  if (resolution === "category") {
    return (
      <Suspense fallback={<RouteLoading label={text("카테고리를 불러오는 중", "Loading category")} />}>
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
      <h1>{text("서버 기능을 확인하지 못했습니다", "Could not check server capabilities")}</h1>
      <p>{detail}</p>
      <button className="button button-primary" onClick={onRetry} type="button">{text("다시 시도", "Try again")}</button>
    </section>
  );
}

function SiteHeader() {
  const { session, capabilities, capabilitiesError, refreshCapabilities, setSession } = useSession();
  const [busy, setBusy] = useState(false);
  const studioAccess = capabilities ? studioAccessFor(capabilities) : undefined;
  const deliveryOnly = studioAccess === "disabled";
  const accessKeyLogin = Boolean(
    capabilities && adminAuthChoices(capabilities).accessKeyMethods.length,
  );

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
        <AppLink className="brand" href="/" aria-label={text("OpenSoverignBlog 홈", "OpenSoverignBlog home")}>
          <span className="brand-mark" aria-hidden="true">
            OS
          </span>
          <span>OpenSoverignBlog</span>
        </AppLink>
        <nav className="primary-nav" aria-label={text("주요 메뉴", "Primary navigation")}>
          <AppLink href="/">{text("피드", "Feed")}</AppLink>
          {capabilities?.references ? (
            <AppLink href={capabilities.references.href}>{capabilities.references.label}</AppLink>
          ) : null}
          {session?.state === "authenticated" && session.blog ? (
            <AppLink href={`/@${session.blog.handle}`}>
              {!session.membershipRole || session.membershipRole === "owner"
                ? text("내 블로그", "My blog")
                : text("참여 블로그", "Collaborating blog")}
            </AppLink>
          ) : null}
          {session?.state === "authenticated" && session.blog ? (
            <AppLink href="/studio">Studio</AppLink>
          ) : null}
          {session?.state === "authenticated" && session.blog ? (
            <AppLink href="/studio/series">Series</AppLink>
          ) : null}
          <a href={publicPath("/AI2AI.md")}>AI2AI</a>
        </nav>
        <div className="header-actions">
          {!capabilities ? (
            <button className="button button-ghost" onClick={() => void refreshCapabilities()} title={capabilitiesError} type="button">
              {capabilitiesError ? text("기능 확인 재시도", "Retry capability check") : text("서버 확인 중", "Checking server")}
            </button>
          ) : deliveryOnly ? (
            <span className="mode-pill">{text("읽기 전용", "Read only")}</span>
          ) : session?.state === "authenticated" ? (
            <>
              {session.blog && (!session.membershipRole || session.membershipRole === "owner") ? (
                <AppLink className="button button-ghost header-settings" href="/studio/settings" aria-label={text("블로그 설정", "Blog settings")}>
                  <span aria-hidden="true">⚙</span><span className="header-settings-label">{text("설정", "Settings")}</span>
                </AppLink>
              ) : null}
              <AppLink className="button button-primary header-write" href="/studio/new">
                {text("새 콘텐츠", "New content")}
              </AppLink>
              <div className="session-chip">
                <span className="avatar avatar-small" aria-hidden="true">
                  {initials(session.user.displayName)}
                </span>
                <span className="session-name">{session.user.displayName}</span>
                <button disabled={busy} onClick={() => void logout()} type="button">
                  {text("로그아웃", "Log out")}
                </button>
              </div>
            </>
          ) : (
            <AppLink className="button button-primary" href="/login">
              {accessKeyLogin
                ? text("관리자 키 입력", "Enter administrator key")
                : studioAccess === "admin_only"
                  ? text("관리자 접근", "Administrator access")
                  : text("로그인", "Log in")}
            </AppLink>
          )}
        </div>
      </div>
    </header>
  );
}

function SiteFooter() {
  const { capabilities } = useSession();
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
        <p>{text("당신의 Markdown, 당신의 서버, 당신의 기록.", "Your Markdown. Your server. Your record.")}</p>
        <p className="footer-version">
          <span>{text("현재 버전", "Current version")} v{version?.currentVersion ?? "0.1.2"}</span>
          <span>{text("출시일", "Released")} {version?.currentReleaseDate ?? text("미출시", "unreleased")}</span>
          {version?.latestVersion ? <span>{text("최신 버전", "Latest version")} v{version.latestVersion}{version.latestReleaseDate ? ` (${version.latestReleaseDate})` : ""}{version.updateAvailable ? text(" · 업데이트 가능", " · update available") : ""}</span> : null}
        </p>
      </div>
      <div className="footer-links">
        {capabilities?.references ? (
          <AppLink href={capabilities.references.href}>{capabilities.references.label}</AppLink>
        ) : null}
        <a href={publicPath("/.well-known/open-soverign-blog.json")}>Discovery</a>
        <a href={publicPath("/openapi/openapi.yaml")}>OpenAPI</a>
        <a href={publicPath("/AI2AI.md")}>AI2AI</a>
        <a href={publicPath(version?.licenseHref ?? "/UNLICENSE")}>Unlicense</a>
        <AdvertisingSettingsButton />
        <a className="footer-github" href={version?.repositoryUrl ?? "https://github.com/studyreadbook4ever/OpenSoverignBlog"} rel="noreferrer" target="_blank">
          <svg aria-hidden="true" viewBox="0 0 24 24"><path d="M12 .7a11.5 11.5 0 0 0-3.64 22.4c.58.1.79-.25.79-.56v-2.23c-3.23.7-3.91-1.37-3.91-1.37-.53-1.34-1.29-1.7-1.29-1.7-1.05-.72.08-.7.08-.7 1.17.08 1.78 1.2 1.78 1.2 1.04 1.77 2.72 1.26 3.39.96.1-.75.4-1.26.74-1.55-2.58-.29-5.29-1.29-5.29-5.7 0-1.26.45-2.29 1.2-3.1-.12-.3-.52-1.48.11-3.07 0 0 .98-.31 3.16 1.18a10.9 10.9 0 0 1 5.76 0c2.2-1.5 3.17-1.18 3.17-1.18.63 1.6.23 2.78.11 3.07.75.81 1.2 1.84 1.2 3.1 0 4.43-2.72 5.4-5.3 5.7.42.36.78 1.07.78 2.16v3.2c0 .31.21.67.8.56A11.5 11.5 0 0 0 12 .7Z" /></svg>
          GitHub
        </a>
        <a aria-label={text("개발자 홈페이지 eff0rtchung.kr", "Developer homepage eff0rtchung.kr")} className="footer-home-link" href={version?.developerUrl ?? "https://eff0rtchung.kr"} rel="noreferrer" target="_blank">
          <svg aria-hidden="true" viewBox="0 0 24 24"><path d="M3 10.8 12 3l9 7.8v9.7a.5.5 0 0 1-.5.5H15v-6H9v6H3.5a.5.5 0 0 1-.5-.5v-9.7Z" /></svg>
          eff0rtchung.kr
        </a>
      </div>
    </footer>
  );
}
