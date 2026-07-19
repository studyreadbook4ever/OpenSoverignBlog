import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type ClipboardEvent,
  type FormEvent,
  type ReactNode,
} from "react";
import DOMPurify from "dompurify";
import type {
  Capabilities,
  Collaborator,
  CollaboratorRole,
  CreatePostInput,
  DocumentSnapshot,
  EmbedReference,
  OntologySidecar,
  PublishArtifact,
  StudioSettings,
  ThemePresetId,
} from "@opensoverignblog/sdk";
import { useSession } from "./app";
import { isLegacyOwnerBearerMode, studioAccessFor } from "./auth-policy";
import {
  AppLink,
  THEME_PRESETS,
  asMessage,
  client,
  formatDate,
  isNotFound,
  navigate,
  slugify,
  usePageTitle,
} from "./lib";

const DRAFT_KEY_PREFIX = "osb.studio.draft.v3";

type DraftStorageMode = "session" | "device" | "off";
type PastePolicy = "allow" | "block";
type EditorTab = "write" | "preview";
type LocalDraftState = "saving" | "saved" | "off" | "error";

interface DraftMemoryScopes {
  core: boolean;
  intent: boolean;
  embeds: boolean;
  ontology: boolean;
  pasteReceipts: boolean;
}

interface PasteReceipt {
  occurredAt: string;
  field: "title" | "slug" | "markdown" | "intent" | "embeds" | "ontology";
  characters: number;
  mediaTypes: string[];
}

interface EditingTarget {
  documentId: string;
  baseRevisionId: string;
}

interface StoredStudioDraft {
  version: 3;
  post: CreatePostInput;
  embedText: string;
  ontologyText: string;
  editing?: EditingTarget;
  memoryScopes: DraftMemoryScopes;
  pastePolicy: PastePolicy;
  pasteReceipts: PasteReceipt[];
  retentionHours?: number;
  savedAt: string;
  expiresAt?: string;
}

export function StudioDashboard({ capabilities }: { capabilities: Capabilities | undefined }) {
  const { session, capabilitiesError, refreshCapabilities } = useSession();
  const [documents, setDocuments] = useState<DocumentSnapshot[]>([]);
  const [loading, setLoading] = useState(true);
  const [status, setStatus] = useState<string>();
  usePageTitle("Studio");

  const studioAccess = capabilities ? studioAccessFor(capabilities) : undefined;
  const legacyOwnerMode = capabilities ? isLegacyOwnerBearerMode(capabilities) : false;
  const canLoad = Boolean(
    studioAccess !== "disabled"
    && !legacyOwnerMode
    && session?.state === "authenticated"
    && session.blog,
  );

  async function load() {
    setLoading(true);
    setStatus("문서를 불러오는 중…");
    try {
      const values = await listDocumentsCompat(legacyOwnerMode);
      setDocuments(values);
      setStatus(values.length ? `${values.length}개의 문서를 불러왔습니다.` : undefined);
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    if (!canLoad) return;
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canLoad]);

  const published = documents.filter((document) => Boolean(document.publishedRevisionId)).length;
  const drafts = documents.filter((document) => document.publishedRevisionId !== document.currentRevisionId).length;
  const displayName = session?.state === "authenticated" ? session.user.displayName : "Owner";
  const collaboratorSession = studioAccess === "members" && session?.state === "authenticated"
    && Boolean(session.membershipRole && session.membershipRole !== "owner");

  if (!capabilities && capabilitiesError) {
    return <StudioAccessGate detail={`서버 기능을 확인하지 못했습니다: ${capabilitiesError}`} onRetry={() => void refreshCapabilities()} />;
  }
  if (!capabilities || !session) {
    return <div className="dashboard-loading" role="status">Studio 접근 권한을 확인하는 중…</div>;
  }
  if (studioAccess === "disabled") {
    return <StudioAccessGate detail="이 인스턴스는 공개 읽기 전용으로 배포되어 Studio가 비활성화되어 있습니다." />;
  }
  if (legacyOwnerMode) {
    return <StudioAccessGate detail="이 서버는 브라우저에 관리자 토큰을 보관하는 이전 방식을 사용합니다. 세션 기반 관리자 인증을 설정해 주세요." login />;
  }
  if (session.state !== "authenticated") {
    return <StudioAccessGate detail="블로그의 글을 쓰고 관리하려면 먼저 인증해 주세요." login />;
  }
  if (session.state === "authenticated" && !session.blog) {
    return <StudioAccessGate detail="글을 쓰기 전에 블로그 이름과 첫 테마를 선택해 주세요." onboarding />;
  }
  if (!canLoad) {
    return <StudioAccessGate detail="이 서버가 지원하는 글쓰기 프로필을 확인할 수 없습니다." />;
  }

  return (
    <div className="studio-dashboard">
      <header className="studio-heading">
        <div>
          <p className="eyebrow">글쓰기 공간</p>
          <h1>{displayName}님의 Studio</h1>
          <p>{collaboratorSession
            ? "초안을 편하게 쓰고 안전하게 저장하세요. 공개 발행과 블로그 설정은 소유자가 맡습니다."
            : "편하게 쓰고 안전하게 저장한 뒤, 준비된 글만 블로그에 공개하세요."}</p>
        </div>
        <div className="studio-heading-actions">
          {session.state === "authenticated" && (!session.membershipRole || session.membershipRole === "owner") ? (
            <AppLink className="button button-ghost" href="/studio/settings"><span aria-hidden="true">⚙</span> 블로그 설정</AppLink>
          ) : null}
          <AppLink className="button button-primary" href="/studio/write">새 글 쓰기 <span aria-hidden="true">＋</span></AppLink>
        </div>
      </header>

      <section className="studio-metrics" aria-label="문서 현황">
        <div><span>전체 문서</span><strong>{documents.length}</strong></div>
        <div><span>초안</span><strong>{drafts}</strong></div>
        <div><span>발행됨</span><strong>{published}</strong></div>
        <div><span>운영 상태</span><strong className="metric-mode">{capabilityModeLabel(capabilities)}</strong></div>
      </section>

      <section className="document-section" aria-labelledby="documents-title">
        <div className="section-heading">
          <div><p className="eyebrow">Documents</p><h2 id="documents-title">블로그 문서</h2></div>
          <button className="button button-ghost" onClick={() => void load()} type="button">새로고침</button>
        </div>
        {loading ? <div className="dashboard-loading" role="status">문서 목록을 불러오는 중…</div> : null}
        {!loading && documents.length === 0 ? (
          <div className="dashboard-empty"><span aria-hidden="true">✎</span><h3>아직 문서가 없습니다</h3><p>첫 글 초안을 시작해 보세요.</p><AppLink className="button button-primary" href="/studio/write">첫 글 쓰기</AppLink></div>
        ) : null}
        {documents.length ? (
          <div className="document-cards">
            {documents.map((document) => (
              <article className="document-card" key={document.id}>
                <div className="document-status-row">
                  <span className={`status-badge status-${document.status}`}>{document.status === "archived" ? "보관됨" : document.publishedRevisionId === document.currentRevisionId ? "발행됨" : document.publishedRevisionId ? "발행 대기 변경" : "초안"}</span>
                  <time dateTime={document.updatedAt}>{formatDate(document.updatedAt)}</time>
                </div>
                <h3><AppLink href={`/studio/write/${document.id}`}>{document.revision.title || "제목 없는 글"}</AppLink></h3>
                <p className="document-slug">/@{session?.state === "authenticated" && session.blog ? session.blog.handle : "blog"}/{document.revision.slug || "untitled"}</p>
                <div className="document-card-footer"><span>저장 버전 {document.revision.revisionNumber}</span><AppLink href={`/studio/write/${document.id}`}>계속 쓰기 <span aria-hidden="true">→</span></AppLink></div>
              </article>
            ))}
          </div>
        ) : null}
      </section>

      {status ? <p className="inline-status" role="status">{status}</p> : null}
    </div>
  );
}

type SettingsLoadState = "loading" | "ready" | "unavailable" | "error";
type CollaborationLoadState = "off" | "loading" | "ready" | "unavailable" | "error";

export function StudioSettingsPage({ capabilities }: { capabilities: Capabilities | undefined }) {
  const { session, capabilitiesError, refreshCapabilities } = useSession();
  const [settings, setSettings] = useState<StudioSettings>();
  const [settingsState, setSettingsState] = useState<SettingsLoadState>("loading");
  const [settingsError, setSettingsError] = useState<string>();
  const [themePreset, setThemePreset] = useState<ThemePresetId>("paper");
  const [customCss, setCustomCss] = useState("");
  const [saving, setSaving] = useState(false);
  const [settingsNotice, setSettingsNotice] = useState<{ kind: "success" | "error"; text: string }>();
  const [collaborators, setCollaborators] = useState<Collaborator[]>([]);
  const [collaborationState, setCollaborationState] = useState<CollaborationLoadState>("off");
  const [collaborationError, setCollaborationError] = useState<string>();
  const [collaboratorEmail, setCollaboratorEmail] = useState("");
  const [collaboratorRole, setCollaboratorRole] = useState<CollaboratorRole>("writer");
  const [inviting, setInviting] = useState(false);
  const [removingUserId, setRemovingUserId] = useState<string>();
  const [confirmRemovalUserId, setConfirmRemovalUserId] = useState<string>();
  const [collaborationNotice, setCollaborationNotice] = useState<{ kind: "success" | "error"; text: string }>();
  const [loadAttempt, setLoadAttempt] = useState(0);
  usePageTitle("블로그 설정");

  const studioAccess = capabilities ? studioAccessFor(capabilities) : undefined;
  const legacyOwnerMode = capabilities ? isLegacyOwnerBearerMode(capabilities) : false;
  const ownerSession = session?.state === "authenticated" && Boolean(session.blog) && (
    !session.membershipRole || session.membershipRole === "owner"
  );
  const canLoad = Boolean(studioAccess !== "disabled" && !legacyOwnerMode && ownerSession);
  const collaborationAvailable = Boolean(capabilities?.features.includes("rbac"));

  useEffect(() => {
    if (!canLoad) return;
    const controller = new AbortController();
    setSettingsState("loading");
    setSettingsError(undefined);
    setSettingsNotice(undefined);
    void client.getStudioSettings(controller.signal)
      .then((value) => {
        if (controller.signal.aborted) return;
        setSettings(value);
        setThemePreset(value.themePreset);
        setCustomCss(value.customCss ?? "");
        setSettingsState("ready");
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted) return;
        if (isNotFound(reason)) {
          setSettingsState("unavailable");
          return;
        }
        setSettingsError(asMessage(reason));
        setSettingsState("error");
      });

    if (collaborationAvailable) {
      setCollaborationState("loading");
      setCollaborationError(undefined);
      void client.listStudioCollaborators(controller.signal)
        .then((value) => {
          if (controller.signal.aborted) return;
          setCollaborators(value.items);
          setCollaborationState("ready");
        })
        .catch((reason: unknown) => {
          if (controller.signal.aborted) return;
          if (isNotFound(reason)) {
            setCollaborationState("unavailable");
            return;
          }
          setCollaborationError(asMessage(reason));
          setCollaborationState("error");
        });
    } else {
      setCollaborationState("off");
      setCollaborators([]);
    }
    return () => controller.abort();
  }, [canLoad, collaborationAvailable, loadAttempt]);

  const cssValidationMessage = customCssProblem(customCss);
  const settingsChanged = Boolean(settings) && (
    themePreset !== settings?.themePreset || (settings.customCssEnabled && customCss !== (settings.customCss ?? ""))
  );

  async function saveSettings() {
    if (!settings || saving) return;
    if (cssValidationMessage) {
      setSettingsNotice({ kind: "error", text: cssValidationMessage });
      return;
    }
    setSaving(true);
    setSettingsNotice(undefined);
    try {
      const updated = await client.updateStudioSettings({
        themePreset,
        ...(settings.customCssEnabled ? { customCss: customCss.trim() ? customCss : null } : {}),
      });
      setSettings(updated);
      setThemePreset(updated.themePreset);
      setCustomCss(updated.customCss ?? "");
      setSettingsNotice({
        kind: "success",
        text: `저장했습니다. 테마 버전 ${updated.themeRevision}이 지금부터 공개 블로그에 적용됩니다.`,
      });
    } catch (reason) {
      if (isNotFound(reason)) {
        setSettingsState("unavailable");
      } else {
        setSettingsNotice({ kind: "error", text: asMessage(reason) });
      }
    } finally {
      setSaving(false);
    }
  }

  async function inviteCollaborator(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const email = collaboratorEmail.trim();
    if (!email || inviting) return;
    setInviting(true);
    setCollaborationNotice(undefined);
    try {
      const added = await client.addStudioCollaborator({ email, role: collaboratorRole });
      setCollaborators((current) => [
        ...current.filter((item) => item.userId !== added.userId),
        added,
      ].sort((left, right) => left.displayName.localeCompare(right.displayName, "ko")));
      setCollaboratorEmail("");
      setCollaborationNotice({
        kind: "success",
        text: `${added.displayName}님을 ${collaboratorRoleLabel(added.role)}로 추가했습니다.`,
      });
    } catch (reason) {
      if (isNotFound(reason)) {
        setCollaborationState("unavailable");
      } else {
        setCollaborationNotice({ kind: "error", text: asMessage(reason) });
      }
    } finally {
      setInviting(false);
    }
  }

  async function removeCollaborator(collaborator: Collaborator) {
    const currentUserId = session?.state === "authenticated" ? session.user.id : undefined;
    if (removingUserId || collaborator.userId === currentUserId) return;
    setRemovingUserId(collaborator.userId);
    setCollaborationNotice(undefined);
    try {
      await client.removeStudioCollaborator(collaborator.userId);
      setCollaborators((current) => current.filter((item) => item.userId !== collaborator.userId));
      setConfirmRemovalUserId(undefined);
      setCollaborationNotice({ kind: "success", text: `${collaborator.displayName}님의 공동 작업 권한을 제거했습니다.` });
    } catch (reason) {
      if (isNotFound(reason)) {
        setCollaborationState("unavailable");
      } else {
        setCollaborationNotice({ kind: "error", text: asMessage(reason) });
      }
    } finally {
      setRemovingUserId(undefined);
    }
  }

  if (!capabilities && capabilitiesError) {
    return <StudioAccessGate detail={`서버 기능을 확인하지 못했습니다: ${capabilitiesError}`} onRetry={() => void refreshCapabilities()} />;
  }
  if (!capabilities || !session) {
    return <div className="dashboard-loading" role="status">블로그 설정 접근 권한을 확인하는 중…</div>;
  }
  if (studioAccess === "disabled") {
    return <StudioAccessGate detail="이 인스턴스는 공개 읽기 전용으로 배포되어 블로그 설정을 바꿀 수 없습니다." />;
  }
  if (legacyOwnerMode) {
    return <StudioAccessGate detail="이 서버는 이전 관리자 Bearer 방식을 사용하므로 새 세션 기반 설정 화면을 열 수 없습니다." login />;
  }
  if (session.state !== "authenticated") {
    return <StudioAccessGate detail="블로그 설정을 열려면 먼저 로그인해 주세요." login />;
  }
  if (!session.blog) {
    return <StudioAccessGate detail="설정할 블로그를 먼저 만들어 주세요." onboarding />;
  }
  if (session.membershipRole && session.membershipRole !== "owner") {
    return <StudioAccessGate detail="테마와 공동 작업자 설정은 블로그 소유자만 바꿀 수 있습니다." />;
  }

  return (
    <div className="studio-settings-page">
      <header className="settings-heading">
        <div>
          <p className="eyebrow">Blog settings</p>
          <h1>내 블로그 꾸미기</h1>
          <p>코드를 몰라도 테마를 고를 수 있고, 필요한 경우에만 고급 CSS와 공동 작업자를 관리할 수 있습니다.</p>
        </div>
        <AppLink className="button button-ghost" href="/studio"><span aria-hidden="true">←</span> Studio로</AppLink>
      </header>

      {settingsState === "loading" ? <div className="settings-loading" role="status">현재 블로그 설정을 불러오는 중…</div> : null}
      {settingsState === "unavailable" ? (
        <section className="settings-feature-notice" aria-labelledby="settings-off-title">
          <span aria-hidden="true">○</span>
          <div><h2 id="settings-off-title">블로그 설정 기능이 서버에서 꺼져 있습니다</h2><p>화면 오류가 아닙니다. 서버 운영자가 Studio 설정 API를 켜면 이곳에서 테마를 관리할 수 있습니다.</p></div>
        </section>
      ) : null}
      {settingsState === "error" ? (
        <section className="settings-feature-notice is-error" role="alert">
          <span aria-hidden="true">!</span>
          <div><h2>설정을 불러오지 못했습니다</h2><p>{settingsError}</p><button className="button button-ghost" onClick={() => setLoadAttempt((value) => value + 1)} type="button">다시 시도</button></div>
        </section>
      ) : null}

      {settingsState === "ready" && settings ? (
        <>
          <section className="settings-panel" aria-labelledby="appearance-title">
            <div className="settings-panel-heading">
              <div><span className="settings-step" aria-hidden="true">01</span><div><h2 id="appearance-title">읽기 테마</h2><p>카드를 선택하면 바로 미리 볼 수 있습니다. 저장하기 전에는 독자 화면이 바뀌지 않습니다.</p></div></div>
              <span className="settings-revision">현재 버전 {settings.themeRevision}</span>
            </div>
            <fieldset className="settings-theme-grid">
              <legend className="sr-only">블로그 읽기 테마 선택</legend>
              {THEME_PRESETS.map((preset) => (
                <label className="settings-theme-option" data-theme={preset.id} key={preset.id}>
                  <input checked={themePreset === preset.id} name="settings-theme" onChange={() => setThemePreset(preset.id)} type="radio" value={preset.id} />
                  <span className="settings-theme-preview" aria-hidden="true"><span>OPEN NOTE</span><strong>{preset.sampleTitle}</strong><i /></span>
                  <span className="settings-theme-copy"><strong>{preset.name}</strong><small>{preset.description}</small></span>
                  <span className="settings-theme-selected" aria-hidden="true">✓</span>
                </label>
              ))}
            </fieldset>

            {settings.customCssEnabled ? (
              <details className="settings-advanced">
                <summary><span><strong>고급: 직접 CSS 쓰기</strong><small>필요할 때만 열어 주세요</small></span><span aria-hidden="true">＋</span></summary>
                <div className="settings-advanced-body">
                  <p className="css-safety-note"><strong>이 CSS는 내 블로그 안에서만 적용됩니다.</strong> 방문자를 보호하기 위해 <code>@import</code>를 포함한 모든 @규칙과 <code>url()</code> 같은 외부 리소스 호출은 저장할 수 없습니다.</p>
                  <label htmlFor="blog-custom-css">블로그 CSS</label>
                  <textarea aria-describedby="custom-css-help custom-css-count" id="blog-custom-css" onChange={(event) => setCustomCss(event.target.value)} placeholder={".article-content h2 {\n  color: #315c46;\n}"} spellCheck={false} value={customCss} />
                  <div className="css-editor-meta"><span id="custom-css-help">HTML, 역슬래시, 외부 URL도 차단됩니다.</span><span id="custom-css-count">{new TextEncoder().encode(customCss).length.toLocaleString("ko-KR")} / 65,536 bytes</span></div>
                  {cssValidationMessage ? <p className="settings-message is-error" role="alert">{cssValidationMessage}</p> : null}
                </div>
              </details>
            ) : null}

            <div className="settings-save-row">
              <p>{settingsChanged ? "저장하지 않은 변경이 있습니다." : "공개 블로그와 설정이 같습니다."}</p>
              <button className="button button-primary" disabled={!settingsChanged || saving || Boolean(cssValidationMessage)} onClick={() => void saveSettings()} type="button">{saving ? "저장하는 중…" : "테마 설정 저장"}</button>
            </div>
            {settingsNotice ? <p className={`settings-message is-${settingsNotice.kind}`} role={settingsNotice.kind === "error" ? "alert" : "status"}>{settingsNotice.text}</p> : null}
          </section>

          <section className="settings-panel" aria-labelledby="collaboration-title">
            <div className="settings-panel-heading">
              <div><span className="settings-step" aria-hidden="true">02</span><div><h2 id="collaboration-title">함께 쓰는 사람</h2><p>소유권은 그대로 둔 채 글을 쓰거나 편집할 사람만 추가합니다.</p></div></div>
            </div>
            {collaborationState === "off" || collaborationState === "unavailable" ? (
              <div className="collaboration-off" role="status"><strong>공동 작업 기능이 서버에서 꺼져 있습니다.</strong><p>개인 블로그에는 이 상태가 가장 단순합니다. 필요할 때 운영 설정에서 collaboration을 켤 수 있습니다.</p></div>
            ) : null}
            {collaborationState === "loading" ? <div className="collaboration-loading" role="status">공동 작업자 목록을 불러오는 중…</div> : null}
            {collaborationState === "error" ? (
              <div className="collaboration-off is-error" role="alert"><strong>공동 작업자 목록을 불러오지 못했습니다.</strong><p>{collaborationError}</p><button className="button button-ghost" onClick={() => setLoadAttempt((value) => value + 1)} type="button">다시 시도</button></div>
            ) : null}
            {collaborationState === "ready" ? (
              <>
                <form className="collaborator-invite" onSubmit={(event) => void inviteCollaborator(event)}>
                  <div><label htmlFor="collaborator-email">계정 이메일</label><input autoComplete="email" id="collaborator-email" onChange={(event) => setCollaboratorEmail(event.target.value)} placeholder="writer@example.com" required type="email" value={collaboratorEmail} /></div>
                  <div><label htmlFor="collaborator-role">할 수 있는 일</label><select id="collaborator-role" onChange={(event) => setCollaboratorRole(event.target.value as CollaboratorRole)} value={collaboratorRole}><option value="writer">Writer · 초안 작성·편집</option><option value="editor">Editor · 초안 작성·편집</option></select></div>
                  <button className="button button-primary" disabled={inviting} type="submit">{inviting ? "추가하는 중…" : "공동 작업자로 초대"}</button>
                  <p>이미 이 서버에 가입한 계정의 이메일을 입력하세요. 현재 Writer와 Editor 모두 초안을 만들고 편집할 수 있으며, 공개 발행과 설정은 소유자만 할 수 있습니다. 블로그 소유자는 이 화면에서 제거할 수 없습니다.</p>
                </form>
                {collaborators.length === 0 ? <div className="collaborator-empty"><span aria-hidden="true">☰</span><p>아직 공동 작업자가 없습니다. 혼자 운영 중입니다.</p></div> : (
                  <ul className="collaborator-list" aria-label="현재 공동 작업자">
                    {collaborators.map((collaborator) => (
                      <li key={collaborator.userId}>
                        <div className="collaborator-identity"><span className="avatar avatar-small" aria-hidden="true">{collaborator.displayName.slice(0, 2).toLocaleUpperCase()}</span><div><strong>{collaborator.displayName}</strong><span>{collaborator.email} · @{collaborator.handle}</span></div></div>
                        <span className={`collaborator-role role-${collaborator.role}`}>{collaboratorRoleLabel(collaborator.role)}</span>
                        {confirmRemovalUserId === collaborator.userId ? (
                          <div className="collaborator-remove-confirm" role="alert"><p><strong>{collaborator.displayName}님을 제거할까요?</strong> 이 블로그의 Studio 접근 권한을 잃습니다.</p><div><button className="button button-ghost" disabled={removingUserId === collaborator.userId} onClick={() => setConfirmRemovalUserId(undefined)} type="button">취소</button><button className="button button-danger" disabled={removingUserId === collaborator.userId} onClick={() => void removeCollaborator(collaborator)} type="button">{removingUserId === collaborator.userId ? "제거 중…" : "권한 제거"}</button></div></div>
                        ) : <button className="button button-ghost collaborator-remove" onClick={() => setConfirmRemovalUserId(collaborator.userId)} type="button">제거</button>}
                      </li>
                    ))}
                  </ul>
                )}
                {collaborationNotice ? <p className={`settings-message is-${collaborationNotice.kind}`} role={collaborationNotice.kind === "error" ? "alert" : "status"}>{collaborationNotice.text}</p> : null}
              </>
            ) : null}
          </section>
        </>
      ) : null}
    </div>
  );
}

export function StudioEditor({
  capabilities,
  documentId,
}: {
  capabilities: Capabilities | undefined;
  documentId: string | undefined;
}) {
  const { session } = useSession();
  const studioAccess = capabilities ? studioAccessFor(capabilities) : undefined;
  const legacyOwnerMode = capabilities ? isLegacyOwnerBearerMode(capabilities) : false;
  const canEdit = Boolean(
    studioAccess !== "disabled"
    && !legacyOwnerMode
    && session?.state === "authenticated"
    && session.blog,
  );
  const canPublish = (
    session?.state === "authenticated"
    && (!session.membershipRole || session.membershipRole === "owner")
  );
  const draftOwner = session?.state === "authenticated" ? `user-${session.user.id}` : "anonymous";
  const draftKey = `${DRAFT_KEY_PREFIX}:${draftOwner}:${documentId ?? "new"}`;
  const [initial] = useState(() => loadDraft(draftKey));
  const [draft, setDraft] = useState<CreatePostInput>(initial.value.post);
  const [intentEnabled, setIntentEnabled] = useState(Boolean(initial.value.post.intent));
  const [embedText, setEmbedText] = useState(initial.value.embedText);
  const [ontologyText, setOntologyText] = useState(initial.value.ontologyText);
  const [editing, setEditing] = useState<EditingTarget | undefined>(initial.value.editing);
  const [memoryScopes, setMemoryScopes] = useState(initial.value.memoryScopes);
  const [pastePolicy, setPastePolicy] = useState<PastePolicy>(initial.value.pastePolicy);
  const [pasteReceipts, setPasteReceipts] = useState<PasteReceipt[]>(initial.value.pasteReceipts);
  const [storageMode, setStorageMode] = useState<DraftStorageMode>(initial.mode);
  const [retentionHours, setRetentionHours] = useState(initial.value.retentionHours ?? 24);
  const [localDraftState, setLocalDraftState] = useState<LocalDraftState>(
    initial.mode === "off" ? "off" : initial.restored ? "saved" : "saving",
  );
  const [localSavedAt, setLocalSavedAt] = useState<string | undefined>(
    initial.restored ? initial.value.savedAt : undefined,
  );
  const [accepted, setAccepted] = useState<DocumentSnapshot>();
  const [acceptedFingerprint, setAcceptedFingerprint] = useState<string>();
  const [status, setStatus] = useState<string>();
  const [loadingDocument, setLoadingDocument] = useState(Boolean(documentId));
  const [loadError, setLoadError] = useState<string>();
  const [loadAttempt, setLoadAttempt] = useState(0);
  const [saving, setSaving] = useState(false);
  const [publishing, setPublishing] = useState(false);
  const [publishOpen, setPublishOpen] = useState(false);
  const [activeTab, setActiveTab] = useState<EditorTab>("write");
  const [slugTouched, setSlugTouched] = useState(Boolean(draft.slug));
  const [previewArtifact, setPreviewArtifact] = useState<PublishArtifact>();
  const [previewState, setPreviewState] = useState<"idle" | "loading" | "ready" | "local">("idle");
  const titleInputRef = useRef<HTMLTextAreaElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  usePageTitle(draft.title || "새 글");

  useEffect(() => {
    const resizeTitle = () => {
      const input = titleInputRef.current;
      if (!input) return;
      input.style.height = "auto";
      const configuredMax = Number.parseFloat(window.getComputedStyle(input).maxHeight);
      const maxHeight = Number.isFinite(configuredMax) ? configuredMax : input.scrollHeight;
      const nextHeight = Math.min(input.scrollHeight, maxHeight);
      input.style.height = `${nextHeight}px`;
      input.style.overflowY = input.scrollHeight > nextHeight ? "auto" : "hidden";
    };
    resizeTitle();
    window.addEventListener("resize", resizeTitle);
    return () => window.removeEventListener("resize", resizeTitle);
  }, [activeTab, draft.title]);

  useEffect(() => {
    if (!documentId || !canEdit) return;
    if (accepted?.id === documentId) {
      setLoadingDocument(false);
      setLoadError(undefined);
      return;
    }
    const controller = new AbortController();
    setLoadingDocument(true);
    setLoadError(undefined);
    void loadDocument(documentId, legacyOwnerMode, controller.signal)
      .then((document) => {
        const restoreLocal = Boolean(
          initial.restored && initial.value.editing?.documentId === document.id,
        );
        applyDocument(document, restoreLocal);
        setStatus(restoreLocal
          ? `이 브라우저에 남은 초안을 복구했습니다 · 서버 기준 ${initial.value.editing?.baseRevisionId.slice(0, 8)}`
          : `서버 저장본 ${document.currentRevisionId.slice(0, 8)}을 불러왔습니다.`);
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setLoadError(asMessage(reason));
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoadingDocument(false);
      });
    return () => controller.abort();
    // applyDocument is intentionally scoped to this editor instance.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [documentId, canEdit, legacyOwnerMode, loadAttempt, accepted?.id]);

  useEffect(() => {
    if (!canEdit || loadingDocument || loadError) return;
    setLocalDraftState(storageMode === "off" ? "off" : "saving");
    const handle = window.setTimeout(() => persistDraft(), 350);
    return () => window.clearTimeout(handle);
  }, [canEdit, loadingDocument, loadError, draft, editing, embedText, memoryScopes, ontologyText, pastePolicy, pasteReceipts, retentionHours, storageMode]);

  useEffect(() => {
    if (!canEdit || loadingDocument || loadError) return;
    if (!draft.sourceMarkdown.trim()) {
      setPreviewArtifact(undefined);
      setPreviewState("idle");
      return;
    }
    if (legacyOwnerMode) {
      setPreviewArtifact(undefined);
      setPreviewState("local");
      return;
    }
    const controller = new AbortController();
    const handle = window.setTimeout(() => {
      setPreviewState("loading");
      void client.previewStudio(previewPayload(), controller.signal)
        .then((value) => {
          setPreviewArtifact(value.artifact);
          setPreviewState("ready");
        })
        .catch(() => {
          if (!controller.signal.aborted) {
            setPreviewArtifact(undefined);
            setPreviewState("local");
          }
        });
    }, 500);
    return () => {
      window.clearTimeout(handle);
      controller.abort();
    };
    // Sidecar parsing is intentionally deferred until save; source preview remains responsive.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canEdit, loadingDocument, loadError, legacyOwnerMode, draft.title, draft.slug, draft.sourceMarkdown, draft.intent]);

  function update<K extends keyof CreatePostInput>(key: K, value: CreatePostInput[K]) {
    setStatus(undefined);
    setDraft((current) => ({ ...current, [key]: value }));
  }

  function updateTitle(value: string) {
    setStatus(undefined);
    setDraft((current) => ({
      ...current,
      title: value,
      ...(!slugTouched ? { slug: slugify(value) } : {}),
    }));
  }

  function applyDocument(document: DocumentSnapshot, preserveLocalDraft = false) {
    const revision = document.revision;
    const post: CreatePostInput = {
      title: revision.title,
      slug: revision.slug,
      sourceMarkdown: revision.sourceMarkdown,
      embeds: revision.embeds,
      ...(revision.intent ? { intent: revision.intent } : {}),
      ...(revision.ontology ? { ontology: revision.ontology } : {}),
    };
    if (!preserveLocalDraft) {
      setDraft(post);
      setIntentEnabled(Boolean(revision.intent));
      setEmbedText(revision.embeds.length ? JSON.stringify(revision.embeds, null, 2) : "");
      setOntologyText(revision.ontology ? JSON.stringify(revision.ontology, null, 2) : "");
      setEditing({ documentId: document.id, baseRevisionId: document.currentRevisionId });
      setSlugTouched(true);
    }
    setAccepted(document);
    setAcceptedFingerprint(payloadFingerprint(post));
  }

  function persistDraft() {
    try {
      localStorage.removeItem(draftKey);
      sessionStorage.removeItem(draftKey);
      if (storageMode === "off") {
        setLocalDraftState("off");
        setLocalSavedAt(undefined);
        return;
      }
      const now = new Date();
      const post: CreatePostInput = {
        title: memoryScopes.core ? draft.title : "",
        slug: memoryScopes.core ? draft.slug : "",
        sourceMarkdown: memoryScopes.core ? draft.sourceMarkdown : "",
        ...(memoryScopes.intent && draft.intent ? { intent: draft.intent } : {}),
      };
      const value: StoredStudioDraft = {
        version: 3,
        post,
        embedText: memoryScopes.embeds ? embedText : "",
        ontologyText: memoryScopes.ontology ? ontologyText : "",
        ...(memoryScopes.core && editing ? { editing } : {}),
        memoryScopes,
        pastePolicy,
        pasteReceipts: memoryScopes.pasteReceipts ? pasteReceipts.slice(-20) : [],
        retentionHours,
        savedAt: now.toISOString(),
        ...(storageMode === "device" ? { expiresAt: new Date(now.getTime() + retentionHours * 3_600_000).toISOString() } : {}),
      };
      (storageMode === "device" ? localStorage : sessionStorage).setItem(draftKey, JSON.stringify(value));
      setLocalDraftState("saved");
      setLocalSavedAt(value.savedAt);
    } catch {
      setLocalDraftState("error");
      setLocalSavedAt(undefined);
    }
  }

  function handlePaste(field: PasteReceipt["field"], event: ClipboardEvent<HTMLInputElement | HTMLTextAreaElement>) {
    if (pastePolicy === "block") {
      event.preventDefault();
      setStatus(`${field} 붙여넣기를 현재 초안 정책이 차단했습니다.`);
      return;
    }
    const characters = event.clipboardData.getData("text/plain").length;
    if (memoryScopes.pasteReceipts) {
      setPasteReceipts((current) => [...current.slice(-19), {
        occurredAt: new Date().toISOString(),
        field,
        characters,
        mediaTypes: Array.from(event.clipboardData.types).slice(0, 16),
      }]);
    }
  }

  function parsePayload(): CreatePostInput | undefined {
    let embeds: EmbedReference[] = [];
    let ontology: OntologySidecar | undefined;
    try {
      if (embedText.trim()) {
        const value = JSON.parse(embedText) as EmbedReference[];
        if (!Array.isArray(value)) throw new Error("array");
        embeds = value;
      }
    } catch {
      setStatus("외부 콘텐츠 연결 정보는 JSON 배열 형식이어야 합니다.");
      return;
    }
    try {
      if (ontologyText.trim()) ontology = JSON.parse(ontologyText) as OntologySidecar;
    } catch {
      setStatus("AI 지식 연결 정보의 JSON 형식을 확인해 주세요.");
      return;
    }
    return {
      title: draft.title.trim(),
      slug: draft.slug.trim(),
      sourceMarkdown: draft.sourceMarkdown,
      embeds,
      ...(draft.intent ? { intent: draft.intent } : {}),
      ...(ontology ? { ontology } : {}),
    };
  }

  function previewPayload(): CreatePostInput {
    return {
      title: draft.title || "제목 없는 글",
      slug: draft.slug || "untitled",
      sourceMarkdown: draft.sourceMarkdown,
      ...(draft.intent ? { intent: draft.intent } : {}),
    };
  }

  async function saveRevision() {
    if (!draft.title.trim() || !draft.slug.trim() || !draft.sourceMarkdown.trim()) {
      setStatus("제목과 본문을 입력해 주세요. 글 주소는 제목에서 자동으로 만들어집니다.");
      return;
    }
    const payload = parsePayload();
    if (!payload) return;
    setSaving(true);
    setStatus(editing ? "현재 내용을 새 버전으로 저장하는 중…" : "첫 초안을 서버에 저장하는 중…");
    try {
      const document = editing
        ? await saveRevisionCompat(editing, payload, legacyOwnerMode)
        : await createDocumentCompat(payload, legacyOwnerMode);
      setAccepted(document);
      setAcceptedFingerprint(payloadFingerprint(payload));
      setEditing({ documentId: document.id, baseRevisionId: document.currentRevisionId });
      setStatus(canPublish
        ? `서버 저장 완료 · 버전 ${document.currentRevisionId.slice(0, 8)} · 아직 공개되지 않았습니다.`
        : `서버 저장 완료 · 버전 ${document.currentRevisionId.slice(0, 8)} · 소유자만 공개할 수 있습니다.`);
      localStorage.removeItem(draftKey);
      sessionStorage.removeItem(draftKey);
      if (!documentId) navigate(`/studio/write/${document.id}`, true);
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      setSaving(false);
    }
  }

  async function publishAccepted() {
    if (!canPublish) {
      setStatus("초안은 저장됐습니다. 공개 발행은 블로그 소유자만 할 수 있습니다.");
      return;
    }
    if (!accepted) {
      setStatus("먼저 현재 내용을 저장해 주세요.");
      return;
    }
    setPublishing(true);
    setStatus("저장된 글을 블로그에 공개하는 중…");
    try {
      const published = legacyOwnerMode
        ? await client.publish(accepted.id, accepted.currentRevisionId)
        : await client.publishStudioDocument(accepted.id, accepted.currentRevisionId);
      setAccepted(published);
      setEditing({ documentId: published.id, baseRevisionId: published.currentRevisionId });
      setStatus("글이 블로그에 공개되었습니다.");
      setPublishOpen(false);
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      setPublishing(false);
    }
  }

  function applyFormat(before: string, after = before, placeholder = "텍스트") {
    const textarea = textareaRef.current;
    if (!textarea) return;
    const start = textarea.selectionStart;
    const end = textarea.selectionEnd;
    const selected = draft.sourceMarkdown.slice(start, end) || placeholder;
    const next = `${draft.sourceMarkdown.slice(0, start)}${before}${selected}${after}${draft.sourceMarkdown.slice(end)}`;
    update("sourceMarkdown", next);
    window.requestAnimationFrame(() => {
      textarea.focus();
      textarea.setSelectionRange(start + before.length, start + before.length + selected.length);
    });
  }

  function prefixLines(prefix: string, placeholder = "내용") {
    const textarea = textareaRef.current;
    if (!textarea) return;
    const start = textarea.selectionStart;
    const end = textarea.selectionEnd;
    const selected = draft.sourceMarkdown.slice(start, end) || placeholder;
    const value = selected.split("\n").map((line) => `${prefix}${line}`).join("\n");
    update("sourceMarkdown", `${draft.sourceMarkdown.slice(0, start)}${value}${draft.sourceMarkdown.slice(end)}`);
    window.requestAnimationFrame(() => textarea.focus());
  }

  async function uploadImage(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    if (!file) return;
    setStatus(`${file.name} 업로드 중…`);
    try {
      const uploaded = legacyOwnerMode
        ? await client.uploadAsset(file, file.name)
        : await client.uploadStudioAsset(file, file.name);
      const markdown = `![${uploaded.record.originalFilename}](${uploaded.url})`;
      const textarea = textareaRef.current;
      if (textarea) {
        const index = textarea.selectionStart;
        update("sourceMarkdown", `${draft.sourceMarkdown.slice(0, index)}\n${markdown}\n${draft.sourceMarkdown.slice(index)}`);
      } else {
        update("sourceMarkdown", `${draft.sourceMarkdown.replace(/\s*$/, "")}\n\n${markdown}\n`);
      }
      setStatus("이미지를 저장하고 본문에 넣었습니다.");
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      event.target.value = "";
    }
  }

  const sanitizedPreview = useMemo(
    () => DOMPurify.sanitize(previewArtifact?.html ?? "", {
      USE_PROFILES: { html: true },
      FORBID_TAGS: ["iframe", "object", "embed", "script", "style", "svg"],
      FORBID_ATTR: ["style"],
    }),
    [previewArtifact],
  );
  const currentFingerprint = useMemo(
    () => editorFingerprint(draft, embedText, ontologyText),
    [draft, embedText, ontologyText],
  );
  const bodyCharacterCount = draft.sourceMarkdown.length;
  const readingMinutes = estimateReadingMinutes(draft.sourceMarkdown);
  const revisionMatchesDraft = Boolean(accepted && acceptedFingerprint === currentFingerprint);
  const currentRevisionPublished = Boolean(
    revisionMatchesDraft && accepted?.publishedRevisionId === accepted?.currentRevisionId,
  );
  const localDraftLabel = localDraftState === "saving"
    ? "브라우저에 자동 저장 중…"
    : localDraftState === "saved"
      ? `브라우저 자동 저장됨${localSavedAt ? ` · ${formatSavedTime(localSavedAt)}` : ""}`
      : localDraftState === "off"
        ? "브라우저 자동 저장 꺼짐"
        : "브라우저 자동 저장을 사용할 수 없음";
  const settledSaveLabel = revisionMatchesDraft
    ? currentRevisionPublished
      ? "공개된 내용과 일치함"
      : canPublish ? "서버 저장 완료 · 공개 전" : "서버 저장 완료 · 소유자만 공개"
    : localDraftLabel;
  const editorSaveLabel = saving
    ? "서버에 저장 중…"
    : publishing
      ? "블로그에 공개 중…"
      : status ?? settledSaveLabel;

  useEffect(() => {
    function handleEditorShortcut(event: KeyboardEvent) {
      if (!canEdit || loadingDocument || loadError || event.isComposing || event.repeat) return;
      if (!(event.metaKey || event.ctrlKey) || event.altKey) return;
      if (event.key.toLowerCase() === "s") {
        event.preventDefault();
        if (!saving && !publishing && !revisionMatchesDraft) void saveRevision();
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        if (canPublish && !publishing && !publishOpen) setPublishOpen(true);
      }
    }
    window.addEventListener("keydown", handleEditorShortcut);
    return () => window.removeEventListener("keydown", handleEditorShortcut);
  });

  if (!capabilities || !session) return <div className="editor-loading" role="status">Studio 접근 권한을 확인하는 중…</div>;
  if (studioAccess === "disabled") {
    return <StudioAccessGate detail="이 인스턴스는 공개 읽기 전용으로 배포되어 편집 기능이 없습니다." />;
  }
  if (legacyOwnerMode) {
    return <StudioAccessGate detail="이 서버는 브라우저 Bearer 토큰을 요구하는 이전 방식입니다. 세션 기반 관리자 인증을 설정해 주세요." login />;
  }
  if (session.state !== "authenticated") {
    return <StudioAccessGate detail="글을 쓰려면 먼저 인증해 주세요." login />;
  }
  if (session.state === "authenticated" && !session.blog) {
    return <StudioAccessGate detail="글을 쓰기 전에 블로그 이름과 첫 테마를 선택해 주세요." onboarding />;
  }
  if (!canEdit) {
    return <StudioAccessGate detail="이 서버가 지원하는 글쓰기 프로필을 확인할 수 없습니다." />;
  }
  if (loadingDocument) return <div className="editor-loading" role="status">문서를 불러오는 중…</div>;
  if (loadError) {
    return (
      <section className="empty-state studio-access-gate" role="alert">
        <span className="empty-symbol" aria-hidden="true">!</span>
        <h1>문서를 열지 못했습니다</h1>
        <p>{loadError}</p>
        <button className="button button-primary" onClick={() => setLoadAttempt((value) => value + 1)} type="button">다시 시도</button>
        <AppLink className="button button-ghost" href="/studio">Studio로 돌아가기</AppLink>
      </section>
    );
  }
  return (
    <div className="studio-editor-shell">
      <header className="editor-topbar">
        <AppLink className="editor-exit" href="/studio" aria-label="Studio 대시보드로 돌아가기">← <span>Studio</span></AppLink>
        <div className="editor-save-state" role="status" title={status || localDraftLabel}>
          <span className={saving || publishing || localDraftState === "saving" ? "saving-dot active" : revisionMatchesDraft ? "saving-dot complete" : "saving-dot"} aria-hidden="true" />
          {editorSaveLabel}
        </div>
        <div className="editor-top-actions">
          <span className="mode-pill">{editing ? `저장본 ${accepted?.revision.revisionNumber ?? "—"}` : "새 글"}</span>
          <button
            aria-keyshortcuts="Control+S Meta+S"
            className="button button-save-draft"
            disabled={saving || publishing || revisionMatchesDraft}
            onClick={() => void saveRevision()}
            title="현재 내용 저장 (Ctrl/⌘ + S)"
            type="button"
          >
            {saving ? "저장 중…" : revisionMatchesDraft ? "저장됨" : "저장"}
            <span className="shortcut-hint" aria-hidden="true">Ctrl/⌘ S</span>
          </button>
          <button
            aria-keyshortcuts="Control+Enter Meta+Enter"
            className="button button-publish"
            disabled={publishing || !canPublish}
            onClick={() => setPublishOpen(true)}
            title={canPublish ? "출간 화면 열기 (Ctrl/⌘ + Enter)" : "공개 발행은 블로그 소유자만 할 수 있습니다"}
            type="button"
          >
            {canPublish ? "출간하기" : "소유자만 출간"}
          </button>
        </div>
      </header>

      <div className="mobile-editor-tabs" role="group" aria-label="편집 화면">
        <button aria-pressed={activeTab === "write"} onClick={() => setActiveTab("write")} type="button">쓰기</button>
        <button aria-pressed={activeTab === "preview"} onClick={() => setActiveTab("preview")} type="button">미리보기</button>
      </div>

      <div className="editor-workspace">
        <section className={`write-pane ${activeTab !== "write" ? "mobile-hidden" : ""}`} aria-label="Markdown 작성">
          <div className="editor-scroll">
            <label className="sr-only" htmlFor="post-title">글 제목</label>
            <textarea
              className="title-editor"
              id="post-title"
              maxLength={300}
              onChange={(event) => updateTitle(event.target.value)}
              onPaste={(event) => handlePaste("title", event)}
              placeholder="제목을 입력하세요"
              ref={titleInputRef}
              rows={1}
              value={draft.title}
            />
            <span className="title-rule" aria-hidden="true" />
            <MarkdownToolbar
              onCommand={(command) => {
                if (command === "heading") prefixLines("## ", "제목");
                if (command === "bold") applyFormat("**", "**", "굵은 텍스트");
                if (command === "italic") applyFormat("_", "_", "기울임 텍스트");
                if (command === "strike") applyFormat("~~", "~~", "취소선 텍스트");
                if (command === "quote") prefixLines("> ", "인용문");
                if (command === "link") applyFormat("[", "](https://)", "링크 텍스트");
                if (command === "code") applyFormat("`", "`", "code");
                if (command === "codeblock") applyFormat("```\n", "\n```", "code");
                if (command === "image") fileInputRef.current?.click();
              }}
            />
            <div className="editor-writing-meta">
              <span className="writing-help">서식 버튼으로 본문을 쉽게 꾸밀 수 있어요.</span>
              <span className="writing-stats">공백 포함 {bodyCharacterCount.toLocaleString()}자 · 예상 {readingMinutes ? `${readingMinutes}분` : "1분 미만"}</span>
              <span className={`revision-state ${revisionMatchesDraft ? "is-saved" : "is-dirty"}`}>
                {saving ? "서버 저장 중" : revisionMatchesDraft ? currentRevisionPublished ? "현재 글 공개됨" : canPublish ? "출간 준비됨" : "소유자 검토 대기" : accepted ? "변경 내용 저장 필요" : "첫 저장 전"}
              </span>
            </div>
            {status ? <p className="editor-notice" role="status">{status}</p> : null}
            <label className="sr-only" htmlFor="markdown-source">Markdown 본문</label>
            <textarea
              className="markdown-editor"
              id="markdown-source"
              onChange={(event) => update("sourceMarkdown", event.target.value)}
              onPaste={(event) => handlePaste("markdown", event)}
              placeholder={"이야기를 시작해 보세요.\n\nMarkdown을 몰라도 위의 서식 버튼을 누르면 됩니다."}
              ref={textareaRef}
              spellCheck="true"
              value={draft.sourceMarkdown}
            />
            <input
              accept="image/png,image/jpeg,image/gif,image/webp,image/avif"
              aria-label="이미지 파일 선택"
              className="visually-hidden-input"
              onChange={(event) => void uploadImage(event)}
              ref={fileInputRef}
              tabIndex={-1}
              type="file"
            />
            <AdvancedEditorOptions
              draft={draft}
              draftKey={draftKey}
              embedText={embedText}
              handlePaste={handlePaste}
              intentEnabled={intentEnabled}
              memoryScopes={memoryScopes}
              ontologyText={ontologyText}
              pastePolicy={pastePolicy}
              pasteReceipts={pasteReceipts}
              retentionHours={retentionHours}
              setDraft={setDraft}
              setEmbedText={setEmbedText}
              setIntentEnabled={setIntentEnabled}
              setMemoryScopes={setMemoryScopes}
              setOntologyText={setOntologyText}
              setPastePolicy={setPastePolicy}
              setPasteReceipts={setPasteReceipts}
              setRetentionHours={setRetentionHours}
              setSlugTouched={setSlugTouched}
              setStatus={setStatus}
              setStorageMode={setStorageMode}
              storageMode={storageMode}
            />
          </div>
        </section>

        <section className={`preview-pane ${activeTab !== "preview" ? "mobile-hidden" : ""}`} aria-label="발행 미리보기">
          <div className="preview-scroll">
            <div className="preview-label"><span>미리보기</span><span>{previewState === "loading" ? "최종 화면 확인 중…" : previewState === "ready" ? "실제 공개 화면 기준" : "간단 미리보기"}</span></div>
            <article className="editor-preview-article">
              <h1>{draft.title || "제목 없는 글"}</h1>
              <div className="preview-byline"><span>{session?.state === "authenticated" ? session.user.displayName : "작성자"}</span><span>·</span><span>{formatDate(new Date().toISOString())}</span></div>
              {previewArtifact ? (
                <div className="article-content" dangerouslySetInnerHTML={{ __html: sanitizedPreview }} />
              ) : (
                <LocalMarkdownPreview markdown={draft.sourceMarkdown} />
              )}
            </article>
          </div>
        </section>
      </div>

      {publishOpen && canPublish ? (
        <PublishPanel
          accepted={accepted}
          draft={draft}
          onClose={() => setPublishOpen(false)}
          onPublish={() => void publishAccepted()}
          onSave={() => void saveRevision()}
          publishing={publishing}
          revisionMatchesDraft={revisionMatchesDraft}
          saving={saving}
          status={status}
        />
      ) : null}
    </div>
  );
}

type ToolbarCommand = "heading" | "bold" | "italic" | "strike" | "quote" | "link" | "image" | "code" | "codeblock";

function MarkdownToolbar({ onCommand }: { onCommand: (command: ToolbarCommand) => void }) {
  const tools: Array<{ command: ToolbarCommand; label: string; glyph: ReactNode }> = [
    { command: "heading", label: "제목 2", glyph: "H₂" },
    { command: "bold", label: "굵게", glyph: <strong>B</strong> },
    { command: "italic", label: "기울임", glyph: <em>I</em> },
    { command: "strike", label: "취소선", glyph: <s>S</s> },
    { command: "quote", label: "인용문", glyph: "❞" },
    { command: "link", label: "링크", glyph: "↗" },
    { command: "image", label: "이미지 업로드", glyph: "▧" },
    { command: "code", label: "인라인 코드", glyph: "<>" },
    { command: "codeblock", label: "코드 블록", glyph: "{ }" },
  ];
  return (
    <div className="markdown-toolbar" role="toolbar" aria-label="Markdown 서식">
      {tools.map((tool, index) => (
        <button className={index === 4 || index === 7 ? "toolbar-separator" : ""} key={tool.command} onClick={() => onCommand(tool.command)} title={tool.label} type="button"><span aria-hidden="true">{tool.glyph}</span><span className="sr-only">{tool.label}</span></button>
      ))}
    </div>
  );
}

function AdvancedEditorOptions({
  draft,
  draftKey,
  embedText,
  handlePaste,
  intentEnabled,
  memoryScopes,
  ontologyText,
  pastePolicy,
  pasteReceipts,
  retentionHours,
  setDraft,
  setEmbedText,
  setIntentEnabled,
  setMemoryScopes,
  setOntologyText,
  setPastePolicy,
  setPasteReceipts,
  setRetentionHours,
  setSlugTouched,
  setStatus,
  setStorageMode,
  storageMode,
}: {
  draft: CreatePostInput;
  draftKey: string;
  embedText: string;
  handlePaste: (field: PasteReceipt["field"], event: ClipboardEvent<HTMLInputElement | HTMLTextAreaElement>) => void;
  intentEnabled: boolean;
  memoryScopes: DraftMemoryScopes;
  ontologyText: string;
  pastePolicy: PastePolicy;
  pasteReceipts: PasteReceipt[];
  retentionHours: number;
  setDraft: React.Dispatch<React.SetStateAction<CreatePostInput>>;
  setEmbedText: (value: string) => void;
  setIntentEnabled: (value: boolean) => void;
  setMemoryScopes: React.Dispatch<React.SetStateAction<DraftMemoryScopes>>;
  setOntologyText: (value: string) => void;
  setPastePolicy: (value: PastePolicy) => void;
  setPasteReceipts: (value: PasteReceipt[]) => void;
  setRetentionHours: (value: number) => void;
  setSlugTouched: (value: boolean) => void;
  setStatus: (value: string | undefined) => void;
  setStorageMode: (value: DraftStorageMode) => void;
  storageMode: DraftStorageMode;
}) {
  function update<K extends keyof CreatePostInput>(key: K, value: CreatePostInput[K]) {
    setStatus(undefined);
    setDraft((current) => ({ ...current, [key]: value }));
  }
  return (
    <details className="advanced-editor-options">
      <summary><span>AI·고급 연동</span><small>글 주소 · 초안 보관 · 외부/AI 연결</small></summary>
      <div className="advanced-options-body">
        <label>
          공개 글 주소
          <small>제목에서 자동으로 만들어집니다. 꼭 필요한 경우에만 바꾸세요.</small>
          <input maxLength={240} onChange={(event) => { setSlugTouched(true); update("slug", event.target.value); }} onPaste={(event) => handlePaste("slug", event)} required value={draft.slug} />
        </label>
        <fieldset><legend>브라우저 초안 보관</legend>
          <p>글을 쓰는 동안 서버 저장과 별개로 브라우저에 임시 보관합니다. 민감한 글이라면 범위를 줄이거나 끌 수 있습니다.</p>
          <div className="advanced-grid">
            <label>보관 위치<select onChange={(event) => setStorageMode(event.target.value as DraftStorageMode)} value={storageMode}><option value="session">현재 탭을 닫을 때까지</option><option value="device">이 기기에 일정 시간</option><option value="off">브라우저에 보관하지 않기</option></select></label>
            {storageMode === "device" ? <label>자동 삭제<select onChange={(event) => setRetentionHours(Number(event.target.value))} value={retentionHours}><option value={1}>1시간 뒤</option><option value={24}>24시간 뒤</option><option value={168}>7일 뒤</option><option value={720}>30일 뒤</option></select></label> : null}
            <label>붙여넣기<select onChange={(event) => setPastePolicy(event.target.value as PastePolicy)} value={pastePolicy}><option value="allow">허용</option><option value="block">고급 입력란에서는 막기</option></select></label>
          </div>
          <p>브라우저에 보관할 항목</p>
          <div className="memory-checks">
            {([
              ["core", "기본 글(제목·주소·본문)"],
              ["intent", "별도 HTML 화면"],
              ["embeds", "외부 콘텐츠 연결 정보"],
              ["ontology", "AI 지식 연결 정보"],
              ["pasteReceipts", "붙여넣기 기록(내용 제외)"],
            ] as const).map(([key, label]) => <label key={key}><input checked={memoryScopes[key]} disabled={storageMode === "off"} onChange={(event) => setMemoryScopes((current) => ({ ...current, [key]: event.target.checked }))} type="checkbox" />{label}</label>)}
          </div>
          {pasteReceipts.length ? <p>붙여넣기 시각·글자 수 기록 {pasteReceipts.length}개 · 붙여넣은 내용은 기록하지 않습니다.</p> : null}
          <button className="text-button" onClick={() => { localStorage.removeItem(draftKey); sessionStorage.removeItem(draftKey); setPasteReceipts([]); setStorageMode("off"); setStatus("브라우저에 보관된 초안을 지웠습니다."); }} type="button">브라우저 초안 지우기</button>
        </fieldset>
        <fieldset><legend>별도 HTML 화면 (선택)</legend>
          <p>HTML을 직접 다루는 사용자만 켜세요. 안전 검사를 거친 뒤 기본 Markdown 화면과 함께 보관됩니다.</p>
          <label className="checkbox-label"><input checked={intentEnabled} onChange={(event) => { const enabled = event.target.checked; setIntentEnabled(enabled); update("intent", enabled ? { format: "enhanced-html-v1", sourceHtml: draft.intent?.sourceHtml ?? "" } : undefined); }} type="checkbox" />직접 만든 HTML 화면도 사용하기</label>
          {intentEnabled ? <label>HTML 코드<textarea onChange={(event) => update("intent", { format: "enhanced-html-v1", sourceHtml: event.target.value })} onPaste={(event) => handlePaste("intent", event)} value={draft.intent?.sourceHtml ?? ""} /></label> : null}
        </fieldset>
        <details><summary>외부 콘텐츠 연결 (개발자용)</summary><p>동영상 같은 외부 콘텐츠를 안전한 참조 정보로 연결합니다. 본문에는 <code>::osb-embed id</code>를 한 줄로 넣으세요.</p><label>연결 정보(JSON 배열)<textarea onChange={(event) => { setStatus(undefined); setEmbedText(event.target.value); }} onPaste={(event) => handlePaste("embeds", event)} value={embedText} /></label></details>
        <details><summary>AI 지식 연결 (AI2AI·개발자용)</summary><p>AI나 다른 도구가 글의 의미를 읽을 수 있도록 별도 구조화 정보를 붙입니다. 일반 글쓰기에는 필요하지 않습니다.</p><label>지식 연결 정보(JSON)<textarea onChange={(event) => { setStatus(undefined); setOntologyText(event.target.value); }} onPaste={(event) => handlePaste("ontology", event)} value={ontologyText} /></label></details>
      </div>
    </details>
  );
}

function PublishPanel({
  accepted,
  draft,
  onClose,
  onPublish,
  onSave,
  publishing,
  revisionMatchesDraft,
  saving,
  status,
}: {
  accepted: DocumentSnapshot | undefined;
  draft: CreatePostInput;
  onClose: () => void;
  onPublish: () => void;
  onSave: () => void;
  publishing: boolean;
  revisionMatchesDraft: boolean;
  saving: boolean;
  status: string | undefined;
}) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    dialogRef.current?.showModal();
    return () => dialogRef.current?.close();
  }, []);
  const exactRevisionReady = Boolean(accepted?.currentRevisionId && revisionMatchesDraft);
  const currentRevisionPublished = Boolean(
    revisionMatchesDraft && accepted && accepted.publishedRevisionId === accepted.currentRevisionId,
  );
  return (
    <dialog aria-labelledby="publish-dialog-title" className="publish-dialog" onCancel={(event) => { event.preventDefault(); onClose(); }} ref={dialogRef}>
      <div className="publish-panel-heading"><div><p className="eyebrow">공개 전 확인</p><h2 id="publish-dialog-title">글을 블로그에 공개할까요?</h2></div><button aria-label="출간 패널 닫기" className="dialog-close" onClick={onClose} type="button">×</button></div>
      <div className="publish-summary"><span className="publish-cover" aria-hidden="true">{draft.title.slice(0, 1) || "✦"}</span><div><strong>{draft.title || "제목 없는 글"}</strong><code>/{draft.slug || "untitled"}</code></div></div>
      <div className="revision-flow" aria-label="출간 단계"><div className="flow-step"><span>1</span><div><strong>현재 글 저장</strong><p>지금 화면의 제목과 본문을 안전한 새 버전으로 보관합니다.</p></div>{exactRevisionReady ? <b aria-label="완료">✓</b> : null}</div><div className="flow-step"><span>2</span><div><strong>블로그에 공개</strong><p>저장된 버전만 독자에게 보입니다. 작성 중인 변경은 실수로 공개되지 않습니다.</p></div></div></div>
      {accepted && exactRevisionReady ? <p className="revision-proof">현재 내용이 저장되어 출간할 준비가 됐습니다. <code>{accepted.currentRevisionId.slice(0, 8)}</code></p> : <p className="revision-proof warning">{accepted ? "저장 뒤 바뀐 내용이 있습니다. 현재 내용을 한 번 더 저장해 주세요." : "아직 서버에 저장되지 않았습니다. 먼저 현재 내용을 저장해 주세요."}</p>}
      {status ? <p className="inline-status" role="status">{status}</p> : null}
      <div className="publish-actions"><button className="button button-ghost" disabled={saving || publishing || exactRevisionReady} onClick={onSave} type="button">{saving ? "저장 중…" : exactRevisionReady ? "현재 내용 저장됨" : "현재 내용 저장"}</button><button className="button button-primary" disabled={!exactRevisionReady || saving || publishing || currentRevisionPublished} onClick={onPublish} type="button">{publishing ? "공개 중…" : currentRevisionPublished ? "이미 공개된 글" : "블로그에 공개"}</button></div>
    </dialog>
  );
}

function StudioAccessGate({
  detail,
  login = false,
  onboarding = false,
  onRetry,
}: {
  detail: string;
  login?: boolean;
  onboarding?: boolean;
  onRetry?: () => void;
}) {
  return (
    <section className="empty-state studio-access-gate">
      <span className="empty-symbol" aria-hidden="true">✦</span>
      <h1>Studio를 열 수 없습니다</h1>
      <p>{detail}</p>
      {onRetry ? <button className="button button-primary" onClick={onRetry} type="button">다시 시도</button> : null}
      {login ? <AppLink className="button button-primary" href="/login">로그인</AppLink> : null}
      {onboarding ? <AppLink className="button button-primary" href="/onboarding">블로그 만들기</AppLink> : null}
      {!login && !onboarding ? <AppLink className="button button-ghost" href="/">공개 피드로 돌아가기</AppLink> : null}
    </section>
  );
}

function collaboratorRoleLabel(role: CollaboratorRole): string {
  return `${role === "editor" ? "Editor" : "Writer"} · 초안 작성·편집`;
}

function customCssProblem(value: string): string | undefined {
  if (new TextEncoder().encode(value).length > 65_536) {
    return "CSS는 65,536 bytes 이하로 줄여 주세요.";
  }
  const lower = value.toLocaleLowerCase("en-US");
  if (
    value.includes("<")
    || value.includes(">")
    || value.includes("\\")
    || lower.includes("@")
    || lower.includes("http:")
    || lower.includes("https:")
    || lower.includes("data:")
    || lower.includes("javascript:")
    || lower.includes("//")
    || /(?:^|[^a-z-])(url|image-set|src)\s*\(/i.test(value)
    || lower.includes("expression(")
    || lower.includes("behavior:")
    || lower.includes("-moz-binding")
  ) {
    return "외부 요청이나 페이지 탈출로 이어질 수 있는 CSS가 있습니다. @규칙, url()/image-set()/src(), URL, HTML, 역슬래시를 제거해 주세요.";
  }
  return undefined;
}

function LocalMarkdownPreview({ markdown }: { markdown: string }) {
  if (!markdown.trim()) return <p className="preview-placeholder">왼쪽에 본문을 입력하면 이곳에서 읽기 흐름을 확인할 수 있습니다.</p>;
  const lines = markdown.split("\n");
  const nodes: ReactNode[] = [];
  let inCode = false;
  let code: string[] = [];
  lines.forEach((line, index) => {
    if (line.startsWith("```")) {
      if (inCode) {
        nodes.push(<pre key={`code-${index}`}><code>{code.join("\n")}</code></pre>);
        code = [];
      }
      inCode = !inCode;
      return;
    }
    if (inCode) {
      code.push(line);
      return;
    }
    if (line.startsWith("### ")) nodes.push(<h3 key={index}>{line.slice(4)}</h3>);
    else if (line.startsWith("## ")) nodes.push(<h2 key={index}>{line.slice(3)}</h2>);
    else if (line.startsWith("# ")) nodes.push(<h2 key={index}>{line.slice(2)}</h2>);
    else if (line.startsWith("> ")) nodes.push(<blockquote key={index}>{line.slice(2)}</blockquote>);
    else if (/^[-*] /.test(line)) nodes.push(<p className="local-list-item" key={index}>• {line.slice(2)}</p>);
    else if (line.trim()) nodes.push(<p key={index}>{line}</p>);
    else nodes.push(<br key={index} />);
  });
  if (code.length) nodes.push(<pre key="code-last"><code>{code.join("\n")}</code></pre>);
  return <div className="article-content local-markdown-preview">{nodes}</div>;
}

async function listDocumentsCompat(legacyOwnerMode: boolean, signal?: AbortSignal): Promise<DocumentSnapshot[]> {
  return legacyOwnerMode
    ? client.listAdminDocuments(signal)
    : client.listStudioDocuments(signal);
}

async function loadDocument(
  documentId: string,
  legacyOwnerMode: boolean,
  signal: AbortSignal,
): Promise<DocumentSnapshot> {
  if (legacyOwnerMode) return client.getAdminDocument(documentId, signal);
  return client.getStudioDocument(documentId, signal);
}

async function createDocumentCompat(input: CreatePostInput, legacyOwnerMode: boolean): Promise<DocumentSnapshot> {
  return legacyOwnerMode
    ? client.createPost(input)
    : client.createStudioDocument(input);
}

async function saveRevisionCompat(
  editing: EditingTarget,
  input: CreatePostInput,
  legacyOwnerMode: boolean,
): Promise<DocumentSnapshot> {
  const payload = { ...input, baseRevisionId: editing.baseRevisionId, idempotencyKey: crypto.randomUUID() };
  if (!legacyOwnerMode) return client.createStudioRevision(editing.documentId, payload);
  await client.proposeRevision(editing.documentId, payload);
  return client.getAdminDocument(editing.documentId);
}

function capabilityModeLabel(capabilities: Capabilities): string {
  const access = studioAccessFor(capabilities);
  if (access === "disabled") return "읽기 전용";
  if (access === "admin_only") return "관리자 전용";
  return "계정별 블로그";
}

function estimateReadingMinutes(markdown: string): number {
  const readable = markdown
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/https?:\/\/\S+/g, " ")
    .replace(/[#*_>`~\[\]()-]/g, " ");
  const hangulCharacters = readable.match(/[가-힣]/g)?.length ?? 0;
  const latinWords = readable.match(/[A-Za-z0-9]+/g)?.length ?? 0;
  const otherCharacters = readable
    .replace(/[가-힣A-Za-z0-9\s]/g, "")
    .length;
  const minutes = hangulCharacters / 500 + latinWords / 200 + otherCharacters / 500;
  return minutes > 0 ? Math.max(1, Math.ceil(minutes)) : 0;
}

function formatSavedTime(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleTimeString("ko-KR", { hour: "2-digit", minute: "2-digit" });
}

function loadDraft(draftKey: string): { value: StoredStudioDraft; mode: DraftStorageMode; restored: boolean } {
  const fallback: StoredStudioDraft = {
    version: 3,
    post: { title: "", slug: "", sourceMarkdown: "" },
    embedText: "",
    ontologyText: "",
    memoryScopes: { core: true, intent: true, embeds: true, ontology: false, pasteReceipts: false },
    pastePolicy: "allow",
    pasteReceipts: [],
    savedAt: new Date().toISOString(),
  };
  for (const [storage, mode] of [[sessionStorage, "session"], [localStorage, "device"]] as const) {
    try {
      const raw = storage.getItem(draftKey);
      if (!raw) continue;
      const stored = JSON.parse(raw) as StoredStudioDraft;
      if (stored.version !== 3 || !stored.post || !stored.memoryScopes || !Array.isArray(stored.pasteReceipts) || !["allow", "block"].includes(stored.pastePolicy)) throw new Error("unknown draft version");
      if (stored.retentionHours !== undefined && ![1, 24, 168, 720].includes(stored.retentionHours)) throw new Error("invalid retention period");
      if (stored.expiresAt && Date.parse(stored.expiresAt) <= Date.now()) {
        storage.removeItem(draftKey);
        continue;
      }
      return { value: stored, mode, restored: true };
    } catch {
      storage.removeItem(draftKey);
    }
  }
  return { value: fallback, mode: "session", restored: false };
}

function payloadFingerprint(post: CreatePostInput): string {
  return JSON.stringify({
    title: post.title.trim(),
    slug: post.slug.trim(),
    sourceMarkdown: post.sourceMarkdown,
    embeds: post.embeds ?? [],
    intent: post.intent ?? null,
    ontology: post.ontology ?? null,
  });
}

function editorFingerprint(
  post: CreatePostInput,
  embedText: string,
  ontologyText: string,
): string {
  try {
    const embeds = embedText.trim() ? JSON.parse(embedText) as unknown : [];
    const ontology = ontologyText.trim() ? JSON.parse(ontologyText) as unknown : null;
    return JSON.stringify({
      title: post.title.trim(),
      slug: post.slug.trim(),
      sourceMarkdown: post.sourceMarkdown,
      embeds,
      intent: post.intent ?? null,
      ontology,
    });
  } catch {
    return `invalid-sidecar:${post.title}:${post.slug}:${post.sourceMarkdown}:${embedText}:${ontologyText}`;
  }
}
