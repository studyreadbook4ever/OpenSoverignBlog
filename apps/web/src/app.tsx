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
import type { Capabilities, Session } from "@opensoverignblog/sdk";
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
    window.setTimeout(() => mainRef.current?.focus(), 0);
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
  if (pathname === "/") return <FeedPage />;
  if (pathname === "/login") return <LoginPage />;
  if (pathname === "/onboarding") return <OnboardingPage />;
  if (pathname === "/studio" || pathname === "/studio/") {
    return <Suspense fallback={<RouteLoading label="Studio를 불러오는 중" />}><StudioDashboard capabilities={capabilities} /></Suspense>;
  }
  if (pathname === "/studio/settings" || pathname === "/studio/settings/") {
    return <Suspense fallback={<RouteLoading label="블로그 설정을 불러오는 중" />}><StudioSettingsPage capabilities={capabilities} /></Suspense>;
  }
  const article = pathname.match(/^\/@([^/]+)\/([^/]+)\/?$/);
  if (article) {
    return (
      <ArticlePage
        capabilities={capabilities}
        handle={decodePathSegment(article[1] ?? "")}
        slug={decodePathSegment(article[2] ?? "")}
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
  return <NotFoundPage />;
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
  return (
    <footer className="site-footer">
      <div>
        <strong>OpenSoverignBlog</strong>
        <p>당신의 Markdown, 당신의 서버, 당신의 기록.</p>
      </div>
      <div className="footer-links">
        <a href={publicPath("/.well-known/open-soverign-blog.json")}>Discovery</a>
        <a href={publicPath("/openapi/openapi.yaml")}>OpenAPI</a>
        <a href={publicPath("/AI2AI.md")}>AI2AI</a>
      </div>
    </footer>
  );
}
