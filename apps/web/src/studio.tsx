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
  AiSummary,
  AiSummaryProvider,
  Capabilities,
  CategorySummary,
  Collaborator,
  CollaboratorRole,
  CreatePostInput,
  DocumentSnapshot,
  EmbedReference,
  HomePinTarget,
  OntologySidecar,
  PublishArtifact,
  SeriesSummary,
  StudioSettings,
  ThemePresetId,
} from "@opensoverignblog/sdk";
import { AdminAccessKeyForm } from "./admin-access";
import { useSession } from "./app";
import { adminAuthChoices, studioAccessFor } from "./auth-policy";
import { socialEmbedFromUrl } from "./social-embeds";
import {
  acceptedEditorState,
  aiSummarySourceHash,
  editorFingerprint,
  homeCurationCandidates,
  homeCurationRows,
  homePinTargetKey,
  homePinTargets,
  publishedSeriesMembership,
  type HomeCurationCandidate,
  isAiSummarySourceCurrent,
  normalizeSavePayload,
  normalizedEditorTitle,
  payloadFingerprint,
  revisionSavePayload,
  reviewAiSummaryCandidate,
} from "./studio-state";
import {
  AppLink,
  THEME_PRESETS,
  asMessage,
  client,
  formatDate,
  isNotFound,
  navigate,
  slugify,
  text,
  uiLanguage,
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

type HomePinLoadState = "unavailable" | "loading" | "ready" | "error";

export function StudioDashboard({ capabilities }: { capabilities: Capabilities | undefined }) {
  const { session, capabilitiesError, refreshCapabilities } = useSession();
  const [documents, setDocuments] = useState<DocumentSnapshot[]>([]);
  const [categories, setCategories] = useState<CategorySummary[]>([]);
  const [series, setSeries] = useState<SeriesSummary[]>([]);
  const [homePins, setHomePins] = useState<HomePinTarget[]>([]);
  const [savedHomePins, setSavedHomePins] = useState<HomePinTarget[]>([]);
  const [homePinState, setHomePinState] = useState<HomePinLoadState>("unavailable");
  const [homePinNotice, setHomePinNotice] = useState<{ kind: "success" | "error"; text: string }>();
  const [savingHomePins, setSavingHomePins] = useState(false);
  const [loading, setLoading] = useState(true);
  const [status, setStatus] = useState<string>();
  usePageTitle("Studio");

  const studioAccess = capabilities ? studioAccessFor(capabilities) : undefined;
  const canLoad = Boolean(
    studioAccess !== "disabled"
    && session?.state === "authenticated"
    && session.blog,
  );
  const canCurateHome = Boolean(
    session?.state === "authenticated"
    && session.instanceAdministrator
    && capabilities?.features.includes("home_curation"),
  );

  async function load() {
    setLoading(true);
    setStatus(text("문서를 불러오는 중…", "Loading documents…"));
    try {
      const [values, categoryResponse, seriesResponse] = await Promise.all([
        client.listStudioDocuments(),
        client.listStudioCategories(),
        canCurateHome
          ? client.listStudioSeries()
          : Promise.resolve({ items: [] as SeriesSummary[] }),
      ]);
      setDocuments(values);
      setCategories(categoryResponse.items);
      setSeries(seriesResponse.items);
      setStatus(values.length ? text(`${values.length}개의 문서를 불러왔습니다.`, `Loaded ${values.length} documents.`) : undefined);
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      setLoading(false);
    }
  }

  async function loadHomePins() {
    if (!canCurateHome) {
      setHomePinState("unavailable");
      setHomePins([]);
      setSavedHomePins([]);
      return;
    }
    setHomePinState("loading");
    setHomePinNotice(undefined);
    try {
      const pins = await client.getHomePins();
      const targets = homePinTargets(pins);
      setHomePins(targets);
      setSavedHomePins(targets);
      setHomePinState("ready");
    } catch (reason) {
      if (isNotFound(reason)) setHomePinState("unavailable");
      else {
        setHomePinState("error");
        setHomePinNotice({ kind: "error", text: asMessage(reason) });
      }
    }
  }

  useEffect(() => {
    if (!canLoad) return;
    void load();
    void loadHomePins();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canLoad, canCurateHome]);

  const published = documents.filter((document) => Boolean(document.publishedRevisionId)).length;
  const drafts = documents.filter((document) => document.publishedRevisionId !== document.currentRevisionId).length;
  const displayName = session?.state === "authenticated" ? session.user.displayName : "Owner";
  const collaboratorSession = studioAccess === "members" && session?.state === "authenticated"
    && Boolean(session.membershipRole && session.membershipRole !== "owner");
  const categoryById = new Map(categories.map((category) => [category.id, category]));
  const seriesMembership = publishedSeriesMembership(documents, series);
  const homePinCandidates = homeCurationCandidates({
    pinnedItems: [],
    recentItems: [],
  }, {
    studioDocuments: documents,
    studioSeries: series,
    language: uiLanguage,
  });
  const homePinCandidateByKey = new Map(
    homePinCandidates.map((candidate) => [homePinTargetKey(candidate.target), candidate]),
  );
  const selectedHomePinKeys = new Set(homePins.map(homePinTargetKey));
  const homePinsChanged = homePins.map(homePinTargetKey).join(":")
    !== savedHomePins.map(homePinTargetKey).join(":");

  function toggleDashboardHomePin(target: HomePinTarget) {
    setHomePinNotice(undefined);
    const key = homePinTargetKey(target);
    if (selectedHomePinKeys.has(key)) {
      setHomePins(homePins.filter((item) => homePinTargetKey(item) !== key));
      return;
    }
    if (homePins.length >= 3) {
      setHomePinNotice({ kind: "error", text: text("홈에는 시리즈와 일반 글을 합쳐 최대 3개까지 고정할 수 있습니다.", "You can pin up to three series and standalone posts combined.") });
      return;
    }
    setHomePins([...homePins, target]);
  }

  async function saveDashboardHomePins() {
    if (!homePinsChanged || savingHomePins) return;
    setSavingHomePins(true);
    setHomePinNotice(undefined);
    try {
      const saved = homePinTargets(await client.replaceHomePinTargets(homePins));
      setHomePins(saved);
      setSavedHomePins(saved);
      setHomePinNotice({ kind: "success", text: text("홈 항목의 고정 순서를 저장했습니다.", "Saved the pinned home-unit order.") });
    } catch (reason) {
      setHomePinNotice({ kind: "error", text: asMessage(reason) });
    } finally {
      setSavingHomePins(false);
    }
  }

  if (!capabilities && capabilitiesError) {
    return <StudioAccessGate detail={text(`서버 기능을 확인하지 못했습니다: ${capabilitiesError}`, `Could not check server capabilities: ${capabilitiesError}`)} onRetry={() => void refreshCapabilities()} />;
  }
  if (!capabilities || !session) {
    return <div className="dashboard-loading" role="status">{text("Studio 접근 권한을 확인하는 중…", "Checking Studio access…")}</div>;
  }
  if (studioAccess === "disabled") {
    return <StudioAccessGate detail={text("이 인스턴스는 공개 읽기 전용으로 배포되어 Studio가 비활성화되어 있습니다.", "Studio is disabled because this instance is deployed public read-only.")} />;
  }
  if (session.state !== "authenticated") {
    return <StudioAccessGate detail={text("블로그의 글을 쓰고 관리하려면 먼저 인증해 주세요.", "Authenticate before writing and managing blog posts.")} login />;
  }
  const blog = session.blog;
  if (!blog) {
    return <StudioAccessGate detail={text("글을 쓰기 전에 블로그 이름과 첫 테마를 선택해 주세요.", "Choose a blog name and first theme before writing.")} onboarding />;
  }
  if (!canLoad) {
    return <StudioAccessGate detail={text("이 서버가 지원하는 글쓰기 프로필을 확인할 수 없습니다.", "Could not determine the writing profile supported by this server.")} />;
  }
  const blogHandle = blog.handle;

  return (
    <div className="studio-dashboard">
      <header className="studio-heading">
        <div>
          <p className="eyebrow">{text("글쓰기 공간", "Writing space")}</p>
          <h1>{text(`${displayName}님의 Studio`, `${displayName}'s Studio`)}</h1>
          <p>{collaboratorSession
            ? text("초안을 편하게 쓰고 안전하게 저장하세요. 공개 발행과 블로그 설정은 소유자가 맡습니다.", "Write and save drafts safely. The owner handles public publishing and blog settings.")
            : text("편하게 쓰고 안전하게 저장한 뒤, 준비된 글만 블로그에 공개하세요.", "Write comfortably, save safely, and publish only when a post is ready.")}</p>
        </div>
        <div className="studio-heading-actions">
          {session.state === "authenticated" && (!session.membershipRole || session.membershipRole === "owner") ? (
            <>
              <AppLink className="button button-ghost" href="/studio/categories"><span aria-hidden="true">▦</span> {text("카테고리", "Categories")}</AppLink>
              <AppLink className="button button-ghost" href="/studio/settings"><span aria-hidden="true">⚙</span> {text("블로그 설정", "Blog settings")}</AppLink>
            </>
          ) : null}
          {session.instanceAdministrator ? (
            <AppLink className="button button-ghost" href="/studio/system"><span aria-hidden="true">⌘</span> {text("시스템 구조", "System structure")}</AppLink>
          ) : null}
          <AppLink className="button button-primary" href="/studio/new">{text("새 콘텐츠", "New content")} <span aria-hidden="true">＋</span></AppLink>
        </div>
      </header>

      <section className="studio-metrics" aria-label={text("문서 현황", "Document status")}>
        <div><span>{text("전체 문서", "All documents")}</span><strong>{documents.length}</strong></div>
        <div><span>{text("초안", "Drafts")}</span><strong>{drafts}</strong></div>
        <div><span>{text("발행됨", "Published")}</span><strong>{published}</strong></div>
        <div><span>{text("운영 상태", "Operating mode")}</span><strong className="metric-mode">{capabilityModeLabel(capabilities)}</strong></div>
      </section>

      {canCurateHome ? (
        <section className="studio-home-pin-panel" aria-labelledby="studio-home-pin-title">
          <div>
            <p className="eyebrow">Home curation</p>
            <h2 id="studio-home-pin-title">{text("홈 순서 고정", "Pin home units")}</h2>
            <p>{text("시리즈와 시리즈에 속하지 않은 일반 글을 합쳐 최대 3개까지 선택하세요. 고정한 항목은 공개 홈의 앞쪽에 선택 순서대로 놓입니다.", "Choose up to three series and standalone posts combined. Pinned units move to the front of the public home in your selected order.")}</p>
          </div>
          <span className="studio-home-pin-count">{homePins.length} / 3</span>
          {homePinState === "loading" ? <p className="studio-home-pin-message" role="status">{text("현재 고정 항목을 불러오는 중…", "Loading pinned units…")}</p> : null}
          {homePinState === "error" ? <p className="studio-home-pin-message is-error" role="alert">{homePinNotice?.text}</p> : null}
          {homePinState === "ready" ? (
            <>
              <ol className="studio-home-pin-list" aria-label={text("현재 홈 고정 순서", "Current home-unit pin order")}>
                {homePins.map((target, index) => {
                  const key = homePinTargetKey(target);
                  const candidate = homePinCandidateByKey.get(key);
                  const fallback = `${target.kind === "series" ? "Series" : "Post"} ${target.id.slice(0, 8)}`;
                  return (
                    <li key={key}>
                      <span aria-hidden="true">{index + 1}</span>
                      <span className={`home-pin-kind home-pin-kind-${target.kind}`}>{target.kind === "series" ? "Series" : "Post"}</span>
                      <strong>{candidate?.title || fallback}</strong>
                      <button aria-label={text(`${candidate?.title || fallback} 고정 해제`, `Unpin ${candidate?.title || fallback}`)} onClick={() => toggleDashboardHomePin(target)} type="button">{text("해제", "Unpin")}</button>
                    </li>
                  );
                })}
              </ol>
              {!homePins.length ? <p className="studio-home-pin-empty">{text("아직 고정한 홈 항목이 없습니다.", "No home units are pinned yet.")}</p> : null}
              {homePinCandidates.length ? (
                <ul className="studio-home-pin-candidates" aria-label={text("고정할 수 있는 시리즈와 일반 글", "Series and standalone posts available to pin")}>
                  {homePinCandidates.map((candidate) => {
                    const key = homePinTargetKey(candidate.target);
                    const selected = selectedHomePinKeys.has(key);
                    return (
                      <li key={key}>
                        <button
                          aria-pressed={selected}
                          disabled={!selected && homePins.length >= 3}
                          onClick={() => toggleDashboardHomePin(candidate.target)}
                          type="button"
                        >
                          <span className={`home-pin-kind home-pin-kind-${candidate.kind}`}>{candidate.kind === "series" ? "Series" : "Post"}</span>
                          <span><strong>{candidate.title}</strong><small>{candidate.locationLabel}</small></span>
                          <span>{selected ? text("고정 해제", "Unpin") : text("고정", "Pin")}</span>
                        </button>
                      </li>
                    );
                  })}
                </ul>
              ) : <p className="studio-home-pin-empty">{text("고정할 수 있는 발행 시리즈나 일반 글이 없습니다.", "There are no published series or standalone posts available to pin.")}</p>}
              <div className="studio-home-pin-actions">
                <AppLink className="button button-ghost" href="/studio/settings">{text("순서 자세히 관리", "Manage order")}</AppLink>
                <button className="button button-primary" disabled={!homePinsChanged || savingHomePins} onClick={() => void saveDashboardHomePins()} type="button">
                  {savingHomePins ? text("저장하는 중…", "Saving…") : text("홈 순서 저장", "Save home order")}
                </button>
              </div>
              {homePinNotice ? <p className={`studio-home-pin-message is-${homePinNotice.kind}`} role={homePinNotice.kind === "error" ? "alert" : "status"}>{homePinNotice.text}</p> : null}
            </>
          ) : null}
        </section>
      ) : null}

      <section className="document-section" aria-labelledby="documents-title">
        <div className="section-heading">
          <div><p className="eyebrow">Documents</p><h2 id="documents-title">{text("블로그 문서", "Blog documents")}</h2></div>
          <button className="button button-ghost" onClick={() => { void load(); void loadHomePins(); }} type="button">{text("새로고침", "Refresh")}</button>
        </div>
        {loading ? <div className="dashboard-loading" role="status">{text("문서 목록을 불러오는 중…", "Loading documents…")}</div> : null}
        {!loading && documents.length === 0 ? (
          <div className="dashboard-empty"><span aria-hidden="true">✎</span><h3>{text("아직 문서가 없습니다", "No documents yet")}</h3><p>{text("첫 포스트나 시리즈를 시작해 보세요.", "Start your first post or series.")}</p><AppLink className="button button-primary" href="/studio/new">{text("첫 콘텐츠 만들기", "Create first content")}</AppLink></div>
        ) : null}
        {documents.length ? (
          <div className="document-cards">
            {documents.map((document) => {
              const target: HomePinTarget = { kind: "post", id: document.id };
              const targetKey = homePinTargetKey(target);
              const isSeriesMember = seriesMembership.documentIds.has(document.id);
              const selected = selectedHomePinKeys.has(targetKey);
              return (
              <article className="document-card" key={document.id}>
                <div className="document-status-row">
                  <span className={`status-badge status-${document.status}`}>{document.status === "archived" ? text("보관됨", "Archived") : document.publishedRevisionId === document.currentRevisionId ? text("발행됨", "Published") : document.publishedRevisionId ? text("발행 대기 변경", "Changes pending publication") : text("초안", "Draft")}</span>
                  <time dateTime={document.updatedAt}>{formatDate(document.updatedAt)}</time>
                </div>
                <h3><AppLink href={`/studio/write/${document.id}`}>{document.revision.title || text("제목 없는 글", "Untitled post")}</AppLink></h3>
                <p className="document-slug">{document.categoryId && categoryById.get(document.categoryId)
                  ? `${session.instanceAdministrator ? "" : `/@${blogHandle}`}/${categoryById.get(document.categoryId)!.slug}/${document.revision.slug || "untitled"}`
                  : `/@${session?.state === "authenticated" && session.blog ? session.blog.handle : "blog"}/${document.revision.slug || "untitled"}`}</p>
                <div className="document-card-footer">
                  <span>{text(`저장 버전 ${document.revision.revisionNumber}`, `Saved revision ${document.revision.revisionNumber}`)}</span>
                  <div className="document-card-actions">
                    {canCurateHome && homePinState === "ready" && document.publishedRevisionId && document.status !== "archived" && !isSeriesMember ? (
                      <button
                        aria-pressed={selected}
                        className="document-home-pin"
                        disabled={!selected && homePins.length >= 3}
                        onClick={() => toggleDashboardHomePin(target)}
                        type="button"
                      >
                        {selected ? text("홈 고정 해제", "Unpin from home") : text("홈에 고정", "Pin to home")}
                      </button>
                    ) : null}
                    <AppLink href={`/studio/write/${document.id}`}>{text("계속 쓰기", "Continue writing")} <span aria-hidden="true">→</span></AppLink>
                  </div>
                </div>
              </article>
              );
            })}
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
  const [curationCandidates, setCurationCandidates] = useState<HomeCurationCandidate[]>([]);
  const [homePins, setHomePins] = useState<HomePinTarget[]>([]);
  const [savedHomePins, setSavedHomePins] = useState<HomePinTarget[]>([]);
  const [curationState, setCurationState] = useState<SettingsLoadState>("unavailable");
  const [curationNotice, setCurationNotice] = useState<{ kind: "success" | "error"; text: string }>();
  const [savingCuration, setSavingCuration] = useState(false);
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
  usePageTitle(text("블로그 설정", "Blog settings"));

  const studioAccess = capabilities ? studioAccessFor(capabilities) : undefined;
  const ownerSession = session?.state === "authenticated" && Boolean(session.blog) && (
    !session.membershipRole || session.membershipRole === "owner"
  );
  const canLoad = Boolean(studioAccess !== "disabled" && ownerSession);
  const collaborationAvailable = Boolean(capabilities?.features.includes("rbac"));
  const homeCurationAvailable = Boolean(
    session?.state === "authenticated"
    && session.instanceAdministrator
    && capabilities?.features.includes("home_curation"),
  );

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

  useEffect(() => {
    if (!homeCurationAvailable) {
      setCurationState("unavailable");
      setCurationCandidates([]);
      setHomePins([]);
      setSavedHomePins([]);
      return;
    }
    const controller = new AbortController();
    setCurationState("loading");
    setCurationNotice(undefined);
    void Promise.all([
      client.home(controller.signal),
      client.getHomePins(controller.signal),
      client.listStudioDocuments(controller.signal),
      client.listStudioSeries(controller.signal),
    ]).then(([home, pins, documents, seriesResponse]) => {
      if (controller.signal.aborted) return;
      setCurationCandidates(homeCurationCandidates(home, {
        studioDocuments: documents,
        studioSeries: seriesResponse.items,
        language: uiLanguage,
      }));
      const targets = homePinTargets(pins);
      setHomePins(targets);
      setSavedHomePins(targets);
      setCurationState("ready");
    }).catch((reason: unknown) => {
      if (controller.signal.aborted) return;
      if (isNotFound(reason)) setCurationState("unavailable");
      else {
        setCurationState("error");
        setCurationNotice({ kind: "error", text: asMessage(reason) });
      }
    });
    return () => controller.abort();
  }, [homeCurationAvailable, loadAttempt]);

  const cssValidationMessage = customCssProblem(customCss);
  const settingsChanged = Boolean(settings) && (
    themePreset !== settings?.themePreset || (settings.customCssEnabled && customCss !== (settings.customCss ?? ""))
  );
  const homePinsChanged = homePins.map(homePinTargetKey).join(":")
    !== savedHomePins.map(homePinTargetKey).join(":");
  const curationRows = homeCurationRows(curationCandidates, homePins, uiLanguage);

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
        text: text(`저장했습니다. 테마 버전 ${updated.themeRevision}이 지금부터 공개 블로그에 적용됩니다.`, `Saved. Theme revision ${updated.themeRevision} is now live on the public blog.`),
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
      ].sort((left, right) => left.displayName.localeCompare(
        right.displayName,
        uiLanguage === "en" ? "en-US" : "ko-KR",
      )));
      setCollaboratorEmail("");
      setCollaborationNotice({
        kind: "success",
        text: text(`${added.displayName}님을 ${collaboratorRoleLabel(added.role)}로 추가했습니다.`, `Added ${added.displayName} as ${collaboratorRoleLabel(added.role)}.`),
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

  function toggleHomePin(target: HomePinTarget) {
    setCurationNotice(undefined);
    setHomePins((current) => {
      const key = homePinTargetKey(target);
      if (current.some((item) => homePinTargetKey(item) === key)) {
        return current.filter((item) => homePinTargetKey(item) !== key);
      }
      return current.length < 3 ? [...current, target] : current;
    });
  }

  function moveHomePin(target: HomePinTarget, offset: -1 | 1) {
    setHomePins((current) => {
      const key = homePinTargetKey(target);
      const index = current.findIndex((item) => homePinTargetKey(item) === key);
      const targetIndex = index + offset;
      if (index < 0 || targetIndex < 0 || targetIndex >= current.length) return current;
      const next = [...current];
      [next[index], next[targetIndex]] = [next[targetIndex]!, next[index]!];
      return next;
    });
  }

  async function saveHomeCuration() {
    if (savingCuration) return;
    setSavingCuration(true);
    setCurationNotice(undefined);
    try {
      const saved = homePinTargets(await client.replaceHomePinTargets(homePins));
      setHomePins(saved);
      setSavedHomePins(saved);
      setCurationNotice({ kind: "success", text: text("홈 항목의 고정 순서를 저장했습니다. 공개 홈 캐시도 새로 고쳐집니다.", "Saved the pinned home-unit order. The public home cache will also refresh.") });
    } catch (reason) {
      setCurationNotice({ kind: "error", text: asMessage(reason) });
    } finally {
      setSavingCuration(false);
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
      setCollaborationNotice({ kind: "success", text: text(`${collaborator.displayName}님의 공동 작업 권한을 제거했습니다.`, `Removed collaboration access for ${collaborator.displayName}.`) });
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
    return <StudioAccessGate detail={text(`서버 기능을 확인하지 못했습니다: ${capabilitiesError}`, `Could not check server capabilities: ${capabilitiesError}`)} onRetry={() => void refreshCapabilities()} />;
  }
  if (!capabilities || !session) {
    return <div className="dashboard-loading" role="status">{text("블로그 설정 접근 권한을 확인하는 중…", "Checking blog settings access…")}</div>;
  }
  if (studioAccess === "disabled") {
    return <StudioAccessGate detail={text("이 인스턴스는 공개 읽기 전용으로 배포되어 블로그 설정을 바꿀 수 없습니다.", "Blog settings cannot be changed because this instance is deployed public read-only.")} />;
  }
  if (session.state !== "authenticated") {
    return <StudioAccessGate detail={text("블로그 설정을 열려면 먼저 로그인해 주세요.", "Log in before opening blog settings.")} login />;
  }
  if (!session.blog) {
    return <StudioAccessGate detail={text("설정할 블로그를 먼저 만들어 주세요.", "Create a blog before changing its settings.")} onboarding />;
  }
  if (session.membershipRole && session.membershipRole !== "owner") {
    return <StudioAccessGate detail={text("테마와 공동 작업자 설정은 블로그 소유자만 바꿀 수 있습니다.", "Only the blog owner can change themes and collaborator settings.")} />;
  }

  return (
    <div className="studio-settings-page">
      <header className="settings-heading">
        <div>
          <p className="eyebrow">Blog settings</p>
          <h1>{text("내 블로그 꾸미기", "Customize my blog")}</h1>
          <p>{text("코드를 몰라도 테마를 고를 수 있고, 필요한 경우에만 고급 CSS와 공동 작업자를 관리할 수 있습니다.", "Choose a theme without coding, and manage advanced CSS or collaborators only when needed.")}</p>
        </div>
        <AppLink className="button button-ghost" href="/studio"><span aria-hidden="true">←</span> {text("Studio로", "Back to Studio")}</AppLink>
      </header>

      {settingsState === "loading" ? <div className="settings-loading" role="status">{text("현재 블로그 설정을 불러오는 중…", "Loading current blog settings…")}</div> : null}
      {settingsState === "unavailable" ? (
        <section className="settings-feature-notice" aria-labelledby="settings-off-title">
          <span aria-hidden="true">○</span>
          <div><h2 id="settings-off-title">{text("블로그 설정 기능이 서버에서 꺼져 있습니다", "Blog settings are disabled on the server")}</h2><p>{text("화면 오류가 아닙니다. 서버 운영자가 Studio 설정 API를 켜면 이곳에서 테마를 관리할 수 있습니다.", "This is not a display error. You can manage themes here once the server operator enables the Studio settings API.")}</p></div>
        </section>
      ) : null}
      {settingsState === "error" ? (
        <section className="settings-feature-notice is-error" role="alert">
          <span aria-hidden="true">!</span>
          <div><h2>{text("설정을 불러오지 못했습니다", "Could not load settings")}</h2><p>{settingsError}</p><button className="button button-ghost" onClick={() => setLoadAttempt((value) => value + 1)} type="button">{text("다시 시도", "Try again")}</button></div>
        </section>
      ) : null}

      {settingsState === "ready" && settings ? (
        <>
          <section className="settings-panel" aria-labelledby="appearance-title">
            <div className="settings-panel-heading">
              <div><span className="settings-step" aria-hidden="true">01</span><div><h2 id="appearance-title">{text("읽기 테마", "Reading theme")}</h2><p>{text("카드를 선택하면 바로 미리 볼 수 있습니다. 저장하기 전에는 독자 화면이 바뀌지 않습니다.", "Select a card for an immediate preview. Readers will not see a change until you save.")}</p></div></div>
              <span className="settings-revision">{text(`현재 버전 ${settings.themeRevision}`, `Current revision ${settings.themeRevision}`)}</span>
            </div>
            <fieldset className="settings-theme-grid">
              <legend className="sr-only">{text("블로그 읽기 테마 선택", "Choose blog reading theme")}</legend>
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
                <summary><span><strong>{text("고급: 직접 CSS 쓰기", "Advanced: write custom CSS")}</strong><small>{text("필요할 때만 열어 주세요", "Open only when needed")}</small></span><span aria-hidden="true">＋</span></summary>
                <div className="settings-advanced-body">
                  <p className="css-safety-note"><strong>{text("이 CSS는 내 블로그 안에서만 적용됩니다.", "This CSS applies only inside your blog.")}</strong> {text("방문자를 보호하기 위해", "To protect visitors,")} <code>@import</code>{text("를 포함한 모든 @규칙과", " and all other @-rules, plus external resource calls such as")} <code>url()</code>{text(" 같은 외부 리소스 호출은 저장할 수 없습니다.", ", cannot be saved.")}</p>
                  <label htmlFor="blog-custom-css">{text("블로그 CSS", "Blog CSS")}</label>
                  <textarea aria-describedby="custom-css-help custom-css-count" id="blog-custom-css" onChange={(event) => setCustomCss(event.target.value)} placeholder={".article-content h2 {\n  color: #315c46;\n}"} spellCheck={false} value={customCss} />
                  <div className="css-editor-meta"><span id="custom-css-help">{text("HTML, 역슬래시, 외부 URL도 차단됩니다.", "HTML, backslashes, and external URLs are also blocked.")}</span><span id="custom-css-count">{new TextEncoder().encode(customCss).length.toLocaleString(uiLanguage === "en" ? "en-US" : "ko-KR")} / 65,536 bytes</span></div>
                  {cssValidationMessage ? <p className="settings-message is-error" role="alert">{cssValidationMessage}</p> : null}
                </div>
              </details>
            ) : null}

            <div className="settings-save-row">
              <p>{settingsChanged ? text("저장하지 않은 변경이 있습니다.", "You have unsaved changes.") : text("공개 블로그와 설정이 같습니다.", "Settings match the public blog.")}</p>
              <button className="button button-primary" disabled={!settingsChanged || saving || Boolean(cssValidationMessage)} onClick={() => void saveSettings()} type="button">{saving ? text("저장하는 중…", "Saving…") : text("테마 설정 저장", "Save theme settings")}</button>
            </div>
            {settingsNotice ? <p className={`settings-message is-${settingsNotice.kind}`} role={settingsNotice.kind === "error" ? "alert" : "status"}>{settingsNotice.text}</p> : null}
          </section>

          {homeCurationAvailable ? (
            <section className="settings-panel home-curation-panel" aria-labelledby="home-curation-title">
              <div className="settings-panel-heading">
                <div><span className="settings-step" aria-hidden="true">02</span><div><h2 id="home-curation-title">{text("홈 항목 고정", "Pinned home units")}</h2><p>{text("시리즈와 시리즈에 속하지 않은 일반 글을 합쳐 최대 3개까지 고정합니다. 선택한 순서가 공개 홈의 앞쪽 순서가 됩니다.", "Pin up to three series and standalone posts combined. Your selected order becomes their order at the front of the public home.")}</p></div></div>
                <span className="settings-revision">{homePins.length} / 3</span>
              </div>
              {curationState === "loading" ? <div className="collaboration-loading" role="status">{text("홈 구성을 불러오는 중…", "Loading home curation…")}</div> : null}
              {curationState === "error" ? <div className="collaboration-off is-error" role="alert"><strong>{text("홈 구성을 불러오지 못했습니다.", "Could not load home curation.")}</strong><p>{curationNotice?.text}</p></div> : null}
              {curationState === "unavailable" ? <div className="collaboration-off"><strong>{text("홈 큐레이션 DLC가 활성화되지 않았습니다.", "Home curation is not enabled.")}</strong><p>{text("공개 피드는 계속 최신순으로 동작합니다.", "The public feed continues to use most-recent-first order.")}</p></div> : null}
              {curationState === "ready" ? (
                <>
                  {curationRows.length ? (
                    <ol className="home-curation-list">
                      {curationRows.map((candidate) => {
                        const key = homePinTargetKey(candidate.target);
                        const position = homePins.findIndex((target) => homePinTargetKey(target) === key);
                        const selected = position >= 0;
                        return (
                          <li className={selected ? "is-selected" : ""} key={key}>
                            <button
                              aria-pressed={selected}
                              className="home-curation-select"
                              disabled={!selected && homePins.length >= 3}
                              onClick={() => toggleHomePin(candidate.target)}
                              type="button"
                            >
                              <span aria-hidden="true">{selected ? position + 1 : "＋"}</span>
                              <span>
                                <span className={`home-pin-kind home-pin-kind-${candidate.kind}`}>{candidate.kind === "series" ? "Series" : "Post"}</span>
                                <strong>{candidate.title}</strong>
                                <small>{candidate.locationLabel}</small>
                              </span>
                              <span>{selected ? text("고정 해제", "Unpin") : text("고정", "Pin")}</span>
                            </button>
                            {selected ? (
                              <div className="home-curation-order" aria-label={text(`${candidate.title} 순서 변경`, `Change order for ${candidate.title}`)}>
                                <button disabled={position === 0} onClick={() => moveHomePin(candidate.target, -1)} type="button">{text("위", "Up")}</button>
                                <button disabled={position === homePins.length - 1} onClick={() => moveHomePin(candidate.target, 1)} type="button">{text("아래", "Down")}</button>
                              </div>
                            ) : null}
                          </li>
                        );
                      })}
                    </ol>
                  ) : <div className="collaborator-empty"><p>{text("먼저 시리즈에 글을 발행하거나 일반 글을 하나 발행하면 여기에서 고를 수 있습니다.", "Publish a series entry or a standalone post first, then choose it here.")}</p></div>}
                  <div className="settings-save-row">
                    <p>{text("고정은 별도 구역이나 배지를 만들지 않고 홈 항목의 순서만 앞당깁니다.", "Pinning changes only the home-unit order; it does not add a separate section or badge.")}</p>
                    <button className="button button-primary" disabled={!homePinsChanged || savingCuration} onClick={() => void saveHomeCuration()} type="button">{savingCuration ? text("저장하는 중…", "Saving…") : text("홈 구성 저장", "Save home curation")}</button>
                  </div>
                  {curationNotice ? <p className={`settings-message is-${curationNotice.kind}`} role={curationNotice.kind === "error" ? "alert" : "status"}>{curationNotice.text}</p> : null}
                </>
              ) : null}
            </section>
          ) : null}

          <section className="settings-panel" aria-labelledby="collaboration-title">
            <div className="settings-panel-heading">
              <div><span className="settings-step" aria-hidden="true">{homeCurationAvailable ? "03" : "02"}</span><div><h2 id="collaboration-title">{text("함께 쓰는 사람", "Collaborators")}</h2><p>{text("소유권은 그대로 둔 채 글을 쓰거나 편집할 사람만 추가합니다.", "Add people who can write or edit while retaining ownership.")}</p></div></div>
            </div>
            {collaborationState === "off" || collaborationState === "unavailable" ? (
              <div className="collaboration-off" role="status"><strong>{text("공동 작업 기능이 서버에서 꺼져 있습니다.", "Collaboration is disabled on the server.")}</strong><p>{text("개인 블로그에는 이 상태가 가장 단순합니다. 필요할 때 운영 설정에서 collaboration을 켤 수 있습니다.", "This is the simplest setup for a personal blog. Enable collaboration in server settings when needed.")}</p></div>
            ) : null}
            {collaborationState === "loading" ? <div className="collaboration-loading" role="status">{text("공동 작업자 목록을 불러오는 중…", "Loading collaborators…")}</div> : null}
            {collaborationState === "error" ? (
              <div className="collaboration-off is-error" role="alert"><strong>{text("공동 작업자 목록을 불러오지 못했습니다.", "Could not load collaborators.")}</strong><p>{collaborationError}</p><button className="button button-ghost" onClick={() => setLoadAttempt((value) => value + 1)} type="button">{text("다시 시도", "Try again")}</button></div>
            ) : null}
            {collaborationState === "ready" ? (
              <>
                <form className="collaborator-invite" onSubmit={(event) => void inviteCollaborator(event)}>
                  <div><label htmlFor="collaborator-email">{text("계정 이메일", "Account email")}</label><input autoComplete="email" id="collaborator-email" onChange={(event) => setCollaboratorEmail(event.target.value)} placeholder="writer@example.com" required type="email" value={collaboratorEmail} /></div>
                  <div><label htmlFor="collaborator-role">{text("할 수 있는 일", "Role")}</label><select id="collaborator-role" onChange={(event) => setCollaboratorRole(event.target.value as CollaboratorRole)} value={collaboratorRole}><option value="writer">{text("Writer · 초안 작성·편집", "Writer · create and edit drafts")}</option><option value="editor">{text("Editor · 초안 작성·편집", "Editor · create and edit drafts")}</option></select></div>
                  <button className="button button-primary" disabled={inviting} type="submit">{inviting ? text("추가하는 중…", "Adding…") : text("공동 작업자로 초대", "Invite collaborator")}</button>
                  <p>{text("이미 이 서버에 가입한 계정의 이메일을 입력하세요. 현재 Writer와 Editor 모두 초안을 만들고 편집할 수 있으며, 공개 발행과 설정은 소유자만 할 수 있습니다. 블로그 소유자는 이 화면에서 제거할 수 없습니다.", "Enter the email of an account already registered on this server. Writers and editors can create and edit drafts; only the owner can publish and change settings. The blog owner cannot be removed here.")}</p>
                </form>
                {collaborators.length === 0 ? <div className="collaborator-empty"><span aria-hidden="true">☰</span><p>{text("아직 공동 작업자가 없습니다. 혼자 운영 중입니다.", "There are no collaborators yet. You are working solo.")}</p></div> : (
                  <ul className="collaborator-list" aria-label={text("현재 공동 작업자", "Current collaborators")}>
                    {collaborators.map((collaborator) => (
                      <li key={collaborator.userId}>
                        <div className="collaborator-identity"><span className="avatar avatar-small" aria-hidden="true">{collaborator.displayName.slice(0, 2).toLocaleUpperCase()}</span><div><strong>{collaborator.displayName}</strong><span>{collaborator.email} · @{collaborator.handle}</span></div></div>
                        <span className={`collaborator-role role-${collaborator.role}`}>{collaboratorRoleLabel(collaborator.role)}</span>
                        {confirmRemovalUserId === collaborator.userId ? (
                          <div className="collaborator-remove-confirm" role="alert"><p><strong>{text(`${collaborator.displayName}님을 제거할까요?`, `Remove ${collaborator.displayName}?`)}</strong> {text("이 블로그의 Studio 접근 권한을 잃습니다.", "They will lose Studio access to this blog.")}</p><div><button className="button button-ghost" disabled={removingUserId === collaborator.userId} onClick={() => setConfirmRemovalUserId(undefined)} type="button">{text("취소", "Cancel")}</button><button className="button button-danger" disabled={removingUserId === collaborator.userId} onClick={() => void removeCollaborator(collaborator)} type="button">{removingUserId === collaborator.userId ? text("제거 중…", "Removing…") : text("권한 제거", "Remove access")}</button></div></div>
                        ) : <button className="button button-ghost collaborator-remove" onClick={() => setConfirmRemovalUserId(collaborator.userId)} type="button">{text("제거", "Remove")}</button>}
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
  const canEdit = Boolean(
    studioAccess !== "disabled"
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
  const [requestedSeriesCategoryId] = useState(() => (
    documentId
      ? undefined
      : new URLSearchParams(window.location.search).get("series") ?? undefined
  ));
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
  const [categories, setCategories] = useState<CategorySummary[]>([]);
  const [series, setSeries] = useState<SeriesSummary[]>([]);
  const [categoriesLoading, setCategoriesLoading] = useState(false);
  const [categoriesError, setCategoriesError] = useState<string>();
  const [currentAiSummarySourceHash, setCurrentAiSummarySourceHash] = useState<string | null>();
  const titleInputRef = useRef<HTMLTextAreaElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  usePageTitle(draft.title || text("새 글", "New post"));

  const aiSummaryEnabled = capabilities?.features.includes("ai_summary") ?? false;
  const aiSummaryVisible = aiSummaryEnabled || Boolean(draft.aiSummary);

  useEffect(() => {
    if (!canEdit) return;
    const controller = new AbortController();
    setCategoriesLoading(true);
    setCategoriesError(undefined);
    void Promise.all([
      client.listStudioCategories(controller.signal),
      client.listStudioSeries(controller.signal),
    ])
      .then(([categoryResponse, seriesResponse]) => {
        if (!controller.signal.aborted) {
          setCategories(categoryResponse.items);
          setSeries(seriesResponse.items);
          if (
            !initial.restored
            && requestedSeriesCategoryId
            && seriesResponse.items.some(
              (item) => item.status === "active"
                && item.categoryId === requestedSeriesCategoryId,
            )
          ) {
            setDraft((current) => (
              current.categoryId === undefined
                ? { ...current, categoryId: requestedSeriesCategoryId }
                : current
            ));
          }
        }
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setCategoriesError(asMessage(reason));
      })
      .finally(() => {
        if (!controller.signal.aborted) setCategoriesLoading(false);
      });
    return () => controller.abort();
  }, [canEdit, initial.restored, requestedSeriesCategoryId]);

  useEffect(() => {
    if (!aiSummaryVisible) {
      setCurrentAiSummarySourceHash(undefined);
      return;
    }
    let active = true;
    setCurrentAiSummarySourceHash(undefined);
    void aiSummarySourceHash(normalizedEditorTitle(draft.title), draft.sourceMarkdown)
      .then((sourceHash) => {
        if (active) setCurrentAiSummarySourceHash(sourceHash);
      })
      .catch(() => {
        if (active) setCurrentAiSummarySourceHash(null);
      });
    return () => {
      active = false;
    };
  }, [aiSummaryVisible, draft.sourceMarkdown, draft.title]);

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
    void loadDocument(documentId, controller.signal)
      .then((document) => {
        const restoreLocal = Boolean(
          initial.restored && initial.value.editing?.documentId === document.id,
        );
        applyDocument(document, restoreLocal);
        setStatus(restoreLocal
          ? text(`이 브라우저에 남은 초안을 복구했습니다 · 서버 기준 ${initial.value.editing?.baseRevisionId.slice(0, 8)}`, `Recovered a draft left in this browser · server base ${initial.value.editing?.baseRevisionId.slice(0, 8)}`)
          : text(`서버 저장본 ${document.currentRevisionId.slice(0, 8)}을 불러왔습니다.`, `Loaded server revision ${document.currentRevisionId.slice(0, 8)}.`));
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
  }, [documentId, canEdit, loadAttempt, accepted?.id]);

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
  }, [canEdit, loadingDocument, loadError, draft.title, draft.slug, draft.sourceMarkdown, draft.intent, draft.authorship, draft.aiSummary, currentAiSummarySourceHash, embedText]);

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
      authorship: revision.authorship,
      categoryId: document.categoryId ?? null,
      ...(revision.aiSummary ? { aiSummary: revision.aiSummary } : {}),
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
        ...(memoryScopes.core && draft.authorship ? { authorship: draft.authorship } : {}),
        ...(memoryScopes.core && draft.aiSummary ? { aiSummary: draft.aiSummary } : {}),
        ...(memoryScopes.core && draft.categoryId !== undefined ? { categoryId: draft.categoryId } : {}),
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
      setStatus(text(`${field} 붙여넣기를 현재 초안 정책이 차단했습니다.`, `The current draft policy blocked pasting into ${field}.`));
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
    if (
      draft.aiSummary
      && !isAiSummarySourceCurrent(draft.aiSummary, currentAiSummarySourceHash)
    ) {
      setStatus(text("AI 요약을 만든 뒤 제목이나 본문이 바뀌었습니다. 요약을 다시 만들거나 제거해 주세요.", "The title or body changed after the AI summary was created. Regenerate or remove the summary."));
      return;
    }
    let embeds: EmbedReference[] = [];
    let ontology: OntologySidecar | undefined;
    try {
      if (embedText.trim()) {
        const value = JSON.parse(embedText) as EmbedReference[];
        if (!Array.isArray(value)) throw new Error("array");
        embeds = value;
      }
    } catch {
      setStatus(text("외부 콘텐츠 연결 정보는 JSON 배열 형식이어야 합니다.", "External content references must be a JSON array."));
      return;
    }
    try {
      if (ontologyText.trim()) ontology = JSON.parse(ontologyText) as OntologySidecar;
    } catch {
      setStatus(text("AI 지식 연결 정보의 JSON 형식을 확인해 주세요.", "Check the JSON format of the AI knowledge connection data."));
      return;
    }
    return normalizeSavePayload({
      title: normalizedEditorTitle(draft.title),
      slug: draft.slug.trim(),
      sourceMarkdown: draft.sourceMarkdown,
      embeds,
      ...(draft.authorship ? { authorship: draft.authorship } : {}),
      ...(draft.aiSummary ? { aiSummary: draft.aiSummary } : {}),
      ...(draft.categoryId !== undefined ? { categoryId: draft.categoryId } : {}),
      ...(draft.intent ? { intent: draft.intent } : {}),
      ...(ontology ? { ontology } : {}),
    });
  }

  function previewPayload(): CreatePostInput {
    let embeds: EmbedReference[] = [];
    try {
      const parsed = embedText.trim() ? JSON.parse(embedText) as EmbedReference[] : [];
      if (Array.isArray(parsed)) embeds = parsed;
    } catch {
      // Keep the writing preview responsive; save surfaces malformed JSON.
    }
    return {
      title: normalizedEditorTitle(draft.title) || text("제목 없는 글", "Untitled post"),
      slug: draft.slug || "untitled",
      sourceMarkdown: draft.sourceMarkdown,
      embeds,
      ...(draft.authorship ? { authorship: draft.authorship } : {}),
      ...(isAiSummarySourceCurrent(draft.aiSummary, currentAiSummarySourceHash)
        ? { aiSummary: draft.aiSummary }
        : {}),
      ...(draft.categoryId !== undefined ? { categoryId: draft.categoryId } : {}),
      ...(draft.intent ? { intent: draft.intent } : {}),
    };
  }

  async function saveRevision() {
    if (!draft.title.trim() || !draft.slug.trim() || !draft.sourceMarkdown.trim()) {
      setStatus(text("제목과 본문을 입력해 주세요. 글 주소는 제목에서 자동으로 만들어집니다.", "Enter a title and body. The post address is generated automatically from the title."));
      return;
    }
    const payload = parsePayload();
    if (!payload) return;
    setSaving(true);
    setStatus(editing ? text("현재 내용을 새 버전으로 저장하는 중…", "Saving current content as a new revision…") : text("첫 초안을 서버에 저장하는 중…", "Saving the first draft to the server…"));
    try {
      const document = editing
        ? await appendStudioRevision(
          editing,
          revisionSavePayload(payload, accepted?.categoryId),
        )
        : await client.createStudioDocument(payload);
      const acceptedState = acceptedEditorState(payload);
      setAccepted(document);
      setDraft(acceptedState.draft);
      setAcceptedFingerprint(acceptedState.fingerprint);
      setEditing({ documentId: document.id, baseRevisionId: document.currentRevisionId });
      setStatus(canPublish
        ? text(`서버 저장 완료 · 버전 ${document.currentRevisionId.slice(0, 8)} · 아직 공개되지 않았습니다.`, `Saved to server · revision ${document.currentRevisionId.slice(0, 8)} · not yet public.`)
        : text(`서버 저장 완료 · 버전 ${document.currentRevisionId.slice(0, 8)} · 소유자만 공개할 수 있습니다.`, `Saved to server · revision ${document.currentRevisionId.slice(0, 8)} · only the owner can publish.`));
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
      setStatus(text("초안은 저장됐습니다. 공개 발행은 블로그 소유자만 할 수 있습니다.", "The draft is saved. Only the blog owner can publish."));
      return;
    }
    if (!accepted) {
      setStatus(text("먼저 현재 내용을 저장해 주세요.", "Save the current content first."));
      return;
    }
    setPublishing(true);
    setStatus(text("저장된 글을 블로그에 공개하는 중…", "Publishing the saved post to the blog…"));
    try {
      const published = await client.publishStudioDocument(
        accepted.id,
        accepted.currentRevisionId,
      );
      setAccepted(published);
      setEditing({ documentId: published.id, baseRevisionId: published.currentRevisionId });
      setStatus(text("글이 블로그에 공개되었습니다.", "The post is now public."));
      setPublishOpen(false);
    } catch (reason) {
      setStatus(asMessage(reason));
    } finally {
      setPublishing(false);
    }
  }

  function applyFormat(before: string, after = before, placeholder = text("텍스트", "text")) {
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

  function prefixLines(prefix: string, placeholder = text("내용", "content")) {
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
    setStatus(text(`${file.name} 업로드 중…`, `Uploading ${file.name}…`));
    try {
      const uploaded = await client.uploadStudioAsset(file, file.name);
      const markdown = `![${uploaded.record.originalFilename}](${uploaded.url})`;
      const textarea = textareaRef.current;
      if (textarea) {
        const index = textarea.selectionStart;
        update("sourceMarkdown", `${draft.sourceMarkdown.slice(0, index)}\n${markdown}\n${draft.sourceMarkdown.slice(index)}`);
      } else {
        update("sourceMarkdown", `${draft.sourceMarkdown.replace(/\s*$/, "")}\n\n${markdown}\n`);
      }
      setStatus(text("이미지를 저장하고 본문에 넣었습니다.", "Saved the image and inserted it into the body."));
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
    ? text("브라우저에 자동 저장 중…", "Autosaving in browser…")
    : localDraftState === "saved"
      ? text(`브라우저 자동 저장됨${localSavedAt ? ` · ${formatSavedTime(localSavedAt)}` : ""}`, `Autosaved in browser${localSavedAt ? ` · ${formatSavedTime(localSavedAt)}` : ""}`)
      : localDraftState === "off"
        ? text("브라우저 자동 저장 꺼짐", "Browser autosave off")
        : text("브라우저 자동 저장을 사용할 수 없음", "Browser autosave unavailable");
  const settledSaveLabel = revisionMatchesDraft
    ? currentRevisionPublished
      ? text("공개된 내용과 일치함", "Matches published content")
      : canPublish ? text("서버 저장 완료 · 공개 전", "Saved to server · not public") : text("서버 저장 완료 · 소유자만 공개", "Saved to server · owner can publish")
    : localDraftLabel;
  const editorSaveLabel = saving
    ? text("서버에 저장 중…", "Saving to server…")
    : publishing
      ? text("블로그에 공개 중…", "Publishing to blog…")
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

  if (!capabilities || !session) return <div className="editor-loading" role="status">{text("Studio 접근 권한을 확인하는 중…", "Checking Studio access…")}</div>;
  if (studioAccess === "disabled") {
    return <StudioAccessGate detail={text("이 인스턴스는 공개 읽기 전용으로 배포되어 편집 기능이 없습니다.", "Editing is unavailable because this instance is deployed public read-only.")} />;
  }
  if (session.state !== "authenticated") {
    return <StudioAccessGate detail={text("글을 쓰려면 먼저 인증해 주세요.", "Authenticate before writing.")} login />;
  }
  if (session.state === "authenticated" && !session.blog) {
    return <StudioAccessGate detail={text("글을 쓰기 전에 블로그 이름과 첫 테마를 선택해 주세요.", "Choose a blog name and first theme before writing.")} onboarding />;
  }
  if (!canEdit) {
    return <StudioAccessGate detail={text("이 서버가 지원하는 글쓰기 프로필을 확인할 수 없습니다.", "Could not determine the writing profile supported by this server.")} />;
  }
  if (loadingDocument) return <div className="editor-loading" role="status">{text("문서를 불러오는 중…", "Loading document…")}</div>;
  if (loadError) {
    return (
      <section className="empty-state studio-access-gate" role="alert">
        <span className="empty-symbol" aria-hidden="true">!</span>
        <h1>{text("문서를 열지 못했습니다", "Could not open document")}</h1>
        <p>{loadError}</p>
        <button className="button button-primary" onClick={() => setLoadAttempt((value) => value + 1)} type="button">{text("다시 시도", "Try again")}</button>
        <AppLink className="button button-ghost" href="/studio">{text("Studio로 돌아가기", "Back to Studio")}</AppLink>
      </section>
    );
  }
  return (
    <div className="studio-editor-shell">
      <header className="editor-topbar">
        <AppLink className="editor-exit" href="/studio" aria-label={text("Studio 대시보드로 돌아가기", "Back to Studio dashboard")}>← <span>Studio</span></AppLink>
        <div className="editor-save-state" role="status" title={status || localDraftLabel}>
          <span className={saving || publishing || localDraftState === "saving" ? "saving-dot active" : revisionMatchesDraft ? "saving-dot complete" : "saving-dot"} aria-hidden="true" />
          {editorSaveLabel}
        </div>
        <div className="editor-top-actions">
          <span className="mode-pill">{editing ? text(`저장본 ${accepted?.revision.revisionNumber ?? "—"}`, `Revision ${accepted?.revision.revisionNumber ?? "—"}`) : text("새 글", "New post")}</span>
          <button
            aria-keyshortcuts="Control+S Meta+S"
            className="button button-save-draft"
            disabled={saving || publishing || revisionMatchesDraft}
            onClick={() => void saveRevision()}
            title={text("현재 내용 저장 (Ctrl/⌘ + S)", "Save current content (Ctrl/⌘ + S)")}
            type="button"
          >
            {saving ? text("저장 중…", "Saving…") : revisionMatchesDraft ? text("저장됨", "Saved") : text("저장", "Save")}
            <span className="shortcut-hint" aria-hidden="true">Ctrl/⌘ S</span>
          </button>
          <button
            aria-keyshortcuts="Control+Enter Meta+Enter"
            className="button button-publish"
            disabled={publishing || !canPublish}
            onClick={() => setPublishOpen(true)}
            title={canPublish ? text("출간 화면 열기 (Ctrl/⌘ + Enter)", "Open publish panel (Ctrl/⌘ + Enter)") : text("공개 발행은 블로그 소유자만 할 수 있습니다", "Only the blog owner can publish")}
            type="button"
          >
            {canPublish ? text("출간하기", "Publish") : text("소유자만 출간", "Owner publishes")}
          </button>
        </div>
      </header>

      <div className="mobile-editor-tabs" role="group" aria-label={text("편집 화면", "Editor view")}>
        <button aria-pressed={activeTab === "write"} onClick={() => setActiveTab("write")} type="button">{text("쓰기", "Write")}</button>
        <button aria-pressed={activeTab === "preview"} onClick={() => setActiveTab("preview")} type="button">{text("미리보기", "Preview")}</button>
      </div>

      <div className="editor-workspace">
        <section className={`write-pane ${activeTab !== "write" ? "mobile-hidden" : ""}`} aria-label={text("Markdown 작성", "Write Markdown")}>
          <div className="editor-scroll">
            <label className="sr-only" htmlFor="post-title">{text("글 제목", "Post title")}</label>
            <textarea
              className="title-editor"
              id="post-title"
              maxLength={300}
              onChange={(event) => updateTitle(event.target.value)}
              onPaste={(event) => handlePaste("title", event)}
              placeholder={text("제목을 입력하세요", "Enter a title")}
              ref={titleInputRef}
              rows={1}
              value={draft.title}
            />
            <span className="title-rule" aria-hidden="true" />
            <CategorySelector
              categories={categories}
              error={categoriesError}
              loading={categoriesLoading}
              onChange={(categoryId) => update("categoryId", categoryId)}
              series={series}
              value={draft.categoryId}
            />
            {aiSummaryVisible ? (
              <AiSummaryEditor
                currentSourceHash={currentAiSummarySourceHash}
                draft={draft}
                generationEnabled={aiSummaryEnabled}
                setDraft={setDraft}
              />
            ) : null}
            <MarkdownToolbar
              onCommand={(command) => {
                if (command === "heading") prefixLines("## ", text("제목", "Heading"));
                if (command === "bold") applyFormat("**", "**", text("굵은 텍스트", "bold text"));
                if (command === "italic") applyFormat("_", "_", text("기울임 텍스트", "italic text"));
                if (command === "strike") applyFormat("~~", "~~", text("취소선 텍스트", "struck text"));
                if (command === "quote") prefixLines("> ", text("인용문", "Quote"));
                if (command === "link") applyFormat("[", "](https://)", text("링크 텍스트", "link text"));
                if (command === "code") applyFormat("`", "`", "code");
                if (command === "codeblock") applyFormat("```\n", "\n```", "code");
                if (command === "image") fileInputRef.current?.click();
              }}
            />
            {capabilities?.features.includes("social_embeds") ? (
              <SocialEmbedComposer
                embedText={embedText}
                setDraft={setDraft}
                setEmbedText={setEmbedText}
                setStatus={setStatus}
              />
            ) : null}
            <div className="editor-writing-meta">
              <span className="writing-help">{text("서식 버튼으로 본문을 쉽게 꾸밀 수 있어요.", "Use the formatting buttons to style the body easily.")}</span>
              <span className="writing-stats">{text(`공백 포함 ${bodyCharacterCount.toLocaleString("ko-KR")}자 · 예상 ${readingMinutes ? `${readingMinutes}분` : "1분 미만"}`, `${bodyCharacterCount.toLocaleString("en-US")} characters including spaces · about ${readingMinutes ? `${readingMinutes} min` : "under 1 min"}`)}</span>
              <span className={`revision-state ${revisionMatchesDraft ? "is-saved" : "is-dirty"}`}>
                {saving ? text("서버 저장 중", "Saving to server") : revisionMatchesDraft ? currentRevisionPublished ? text("현재 글 공개됨", "Post is public") : canPublish ? text("출간 준비됨", "Ready to publish") : text("소유자 검토 대기", "Awaiting owner review") : accepted ? text("변경 내용 저장 필요", "Changes need saving") : text("첫 저장 전", "Not yet saved")}
              </span>
            </div>
            {status ? <p className="editor-notice" role="status">{status}</p> : null}
            <label className="sr-only" htmlFor="markdown-source">{text("Markdown 본문", "Markdown body")}</label>
            <textarea
              className="markdown-editor"
              id="markdown-source"
              onChange={(event) => update("sourceMarkdown", event.target.value)}
              onPaste={(event) => handlePaste("markdown", event)}
              placeholder={text("이야기를 시작해 보세요.\n\nMarkdown을 몰라도 위의 서식 버튼을 누르면 됩니다.", "Start your story.\n\nYou can use the formatting buttons above even if you do not know Markdown.")}
              ref={textareaRef}
              spellCheck="true"
              value={draft.sourceMarkdown}
            />
            <input
              accept="image/png,image/jpeg,image/gif,image/webp,image/avif"
              aria-label={text("이미지 파일 선택", "Choose image file")}
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

        <section className={`preview-pane ${activeTab !== "preview" ? "mobile-hidden" : ""}`} aria-label={text("발행 미리보기", "Publication preview")}>
          <div className="preview-scroll">
            <div className="preview-label"><span>{text("미리보기", "Preview")}</span><span>{previewState === "loading" ? text("최종 화면 확인 중…", "Checking final view…") : previewState === "ready" ? text("실제 공개 화면 기준", "Matches public view") : text("간단 미리보기", "Basic preview")}</span></div>
            <article className="editor-preview-article">
              <h1>{draft.title || text("제목 없는 글", "Untitled post")}</h1>
              <div className="preview-byline"><span>{session?.state === "authenticated" ? session.user.displayName : text("작성자", "Author")}</span><span>·</span><span>{formatDate(new Date().toISOString())}</span></div>
              {previewArtifact ? (
                <div className="article-content" dangerouslySetInnerHTML={{ __html: sanitizedPreview }} />
              ) : (
                <>
                  {isAiSummarySourceCurrent(draft.aiSummary, currentAiSummarySourceHash) ? (
                    <AiSummaryPreview summary={draft.aiSummary!} />
                  ) : null}
                  <LocalMarkdownPreview markdown={draft.sourceMarkdown} />
                </>
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

function CategorySelector({
  categories,
  error,
  loading,
  onChange,
  series,
  value,
}: {
  categories: CategorySummary[];
  error: string | undefined;
  loading: boolean;
  onChange: (value: string | null) => void;
  series: SeriesSummary[];
  value: string | null | undefined;
}) {
  const current = value ? categories.find((category) => category.id === value) : undefined;
  const seriesCategoryIds = new Set(series.map((item) => item.categoryId));
  const standaloneCategories = categories.filter((category) => !seriesCategoryIds.has(category.id));
  return (
    <section className="editor-category-selector" aria-labelledby="editor-category-title">
      <div>
        <label id="editor-category-title" htmlFor="editor-category">{text("글 위치", "Post location")}</label>
        <p>{text("독립 포스트로 두거나 시리즈·카테고리에 넣습니다. 시리즈 글은 발행할 때 읽는 순서의 끝에 추가됩니다.", "Keep this as a standalone post or place it in a series or category. Series posts are appended to the reading order when published.")}</p>
      </div>
      <div className="editor-category-control">
        <select
          disabled={loading}
          id="editor-category"
          onChange={(event) => onChange(event.target.value || null)}
          value={value ?? ""}
        >
          <option value="">{text("독립 포스트 · 기존 블로그 주소", "Standalone post · regular blog address")}</option>
          {series.length ? (
            <optgroup label="Series">
              {series.map((item) => (
                <option
                  disabled={item.status === "archived" && item.categoryId !== value}
                  key={item.id}
                  value={item.categoryId}
                >
                  {item.title} · /{item.slug}{item.status === "archived" ? text(" · 보관됨", " · archived") : ""}
                </option>
              ))}
            </optgroup>
          ) : null}
          {standaloneCategories.length ? (
            <optgroup label="Category">
              {standaloneCategories.map((category) => (
                <option
                  disabled={category.status === "archived" && category.id !== value}
                  key={category.id}
                  value={category.id}
                >
                  {category.title} · /{category.slug}{category.status === "archived" ? text(" · 보관됨", " · archived") : ""}
                </option>
              ))}
            </optgroup>
          ) : null}
        </select>
        <span className="editor-placement-links">
          <AppLink className="text-button" href="/studio/series">{text("시리즈 관리", "Manage series")}</AppLink>
          <AppLink className="text-button" href="/studio/categories">{text("카테고리 관리", "Manage categories")}</AppLink>
        </span>
      </div>
      {loading ? <p className="field-help" role="status">{text("글 위치를 불러오는 중…", "Loading post locations…")}</p> : null}
      {error ? <p className="field-error" role="alert">{text("글 위치를 불러오지 못했습니다:", "Could not load post locations:")} {error}</p> : null}
      {value && !current && !loading ? (
        <p className="field-error" role="alert">{text("선택했던 카테고리를 찾을 수 없습니다. 저장 전 다른 카테고리를 골라 주세요.", "The selected category could not be found. Choose another category before saving.")}</p>
      ) : null}
    </section>
  );
}

function AiSummaryEditor({
  currentSourceHash,
  draft,
  generationEnabled,
  setDraft,
}: {
  currentSourceHash: string | null | undefined;
  draft: CreatePostInput;
  generationEnabled: boolean;
  setDraft: React.Dispatch<React.SetStateAction<CreatePostInput>>;
}) {
  const [providers, setProviders] = useState<AiSummaryProvider[]>([]);
  const [maximumSourceBytes, setMaximumSourceBytes] = useState<number>();
  const [catalogState, setCatalogState] = useState<"loading" | "ready" | "error">("loading");
  const [providerId, setProviderId] = useState<AiSummaryProvider["id"] | "">("");
  const [model, setModel] = useState("");
  const [oneShotApiKey, setOneShotApiKey] = useState("");
  const [candidate, setCandidate] = useState<AiSummary>();
  const [generating, setGenerating] = useState(false);
  const [notice, setNotice] = useState<string>();
  const generationController = useRef<AbortController | undefined>(undefined);

  useEffect(() => {
    if (!generationEnabled) {
      setCatalogState("ready");
      setProviders([]);
      setMaximumSourceBytes(undefined);
      return;
    }
    const controller = new AbortController();
    setCatalogState("loading");
    void client.aiSummaryProviders(controller.signal)
      .then((response) => {
        if (controller.signal.aborted) return;
        setProviders(response.providers);
        setMaximumSourceBytes(response.maximumSourceBytes);
        const initialProvider = response.providers.find(
          (provider) => provider.id === draft.aiSummary?.provenance.provider,
        ) ?? response.providers[0];
        if (initialProvider) {
          setProviderId(initialProvider.id);
          setModel(
            initialProvider.models.includes(draft.aiSummary?.provenance.model ?? "")
              ? draft.aiSummary!.provenance.model
              : initialProvider.defaultModel,
          );
        }
        setCatalogState("ready");
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted) return;
        setCatalogState("error");
        setNotice(text(`AI 제공자 목록을 불러오지 못했습니다: ${asMessage(reason)}`, `Could not load AI providers: ${asMessage(reason)}`));
      });
    return () => controller.abort();
    // A loaded document only supplies a preferred initial provider/model.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [generationEnabled]);

  useEffect(() => () => {
    generationController.current?.abort();
    setOneShotApiKey("");
  }, []);

  const selectedProvider = providers.find((provider) => provider.id === providerId);
  const appliedFreshness = aiSummaryFreshness(draft.aiSummary, currentSourceHash);
  const candidateFreshness = aiSummaryFreshness(candidate, currentSourceHash);
  const sourceBytes = new TextEncoder().encode(
    `${normalizedEditorTitle(draft.title)}${draft.sourceMarkdown}`,
  ).byteLength;

  function selectProvider(nextProviderId: AiSummaryProvider["id"]) {
    const provider = providers.find((value) => value.id === nextProviderId);
    setProviderId(nextProviderId);
    setModel(provider?.defaultModel ?? "");
    setNotice(undefined);
  }

  async function generateSummary() {
    if (!selectedProvider || !model || !oneShotApiKey) return;
    generationController.current?.abort();
    const controller = new AbortController();
    generationController.current = controller;
    const apiKey = oneShotApiKey;
    setGenerating(true);
    setNotice(text("AI 제공자에게 제목과 Markdown을 보내 요약 초안을 만드는 중…", "Sending the title and Markdown to the AI provider to draft a summary…"));
    try {
      const response = await client.generateAiSummary({
        provider: selectedProvider.id,
        model,
        credentialMode: "one_shot",
        title: normalizedEditorTitle(draft.title),
        sourceMarkdown: draft.sourceMarkdown,
      }, apiKey, controller.signal);
      if (controller.signal.aborted) return;
      setCandidate(response.candidate);
      setNotice(text("요약 초안이 도착했습니다. 내용을 직접 확인한 뒤 사용해 주세요.", "The summary draft is ready. Review it yourself before using it."));
    } catch (reason) {
      if (!controller.signal.aborted) setNotice(asMessage(reason));
    } finally {
      // Clear the credential for success, failure, and cancellation alike.
      setOneShotApiKey("");
      if (generationController.current === controller) {
        setGenerating(false);
        generationController.current = undefined;
      }
    }
  }

  function useCandidate() {
    if (!candidate || candidateFreshness !== "current" || !candidate.text.trim()) return;
    const reviewed = reviewAiSummaryCandidate({ ...candidate, text: candidate.text.trim() });
    setDraft((current) => ({ ...current, aiSummary: reviewed }));
    setNotice(text("검토한 요약을 이 글에 적용했습니다. 저장 버튼을 눌러 새 리비전에 포함하세요.", "Applied the reviewed summary to this post. Save to include it in a new revision."));
  }

  function removeAppliedSummary() {
    setDraft((current) => {
      const { aiSummary: _removed, ...withoutSummary } = current;
      return withoutSummary;
    });
    setNotice(text("이 글에서 AI 요약을 제거했습니다. 서버에 반영하려면 저장해 주세요.", "Removed the AI summary from this post. Save to apply the change on the server."));
  }

  return (
    <section className="ai-summary-editor" aria-labelledby="ai-summary-editor-title">
      <div className="ai-summary-editor-heading">
        <div>
          <p className="eyebrow">{text("선택 기능", "Optional feature")}</p>
          <h2 id="ai-summary-editor-title">{text("글 위에 AI 요약 넣기", "Add an AI summary above the post")}</h2>
          <p>{generationEnabled
            ? text("생성할 때만 제목과 Markdown 본문을 선택한 AI 제공자에게 보냅니다. 요청이 끝나면 입력칸에서 키를 지우며, 글·브라우저 초안·OSB 데이터베이스에는 저장하지 않습니다.", "Only when generating, the title and Markdown body are sent to the selected AI provider. The key is cleared from the input after the request and is never stored in the post, browser draft, or OSB database.")
            : text("이 서버에서는 새 AI 요약 생성을 사용하지 않습니다. 기존 글에 저장된 요약은 확인하거나 제거할 수 있습니다.", "This server does not generate new AI summaries. You can still review or remove a summary stored on an existing post.")}</p>
        </div>
        {draft.aiSummary ? (
          <span className={`ai-summary-state is-${appliedFreshness}`}>
            {appliedFreshness === "current" ? text("요약 적용됨", "Summary applied") : appliedFreshness === "checking" ? text("내용 확인 중", "Checking content") : text("다시 확인 필요", "Needs review")}
          </span>
        ) : <span className="ai-summary-state">{text("사용 안 함", "Not in use")}</span>}
      </div>

      {draft.aiSummary ? (
        <div className={`ai-summary-applied is-${appliedFreshness}`}>
          <div>
            <strong>{text("현재 글에 적용된 요약", "Summary applied to this post")}</strong>
            <span>{aiSummaryProvenanceText(draft.aiSummary)}</span>
          </div>
          <p>{draft.aiSummary.text}</p>
          {appliedFreshness === "stale" ? <p className="ai-summary-warning" role="alert">{text("제목이나 본문이 달라졌습니다. 저장하기 전에 다시 생성하거나 요약을 제거해 주세요.", "The title or body has changed. Regenerate or remove the summary before saving.")}</p> : null}
          <button className="text-button" onClick={removeAppliedSummary} type="button">{text("적용된 요약 제거", "Remove applied summary")}</button>
        </div>
      ) : null}

      {generationEnabled && catalogState === "loading" ? <p className="ai-summary-loading" role="status">{text("사용 가능한 AI 제공자를 확인하는 중…", "Checking available AI providers…")}</p> : null}
      {generationEnabled && catalogState === "ready" && providers.length === 0 ? <p className="ai-summary-warning" role="status">{text("현재 사용할 수 있는 AI 제공자가 없습니다.", "No AI provider is currently available.")}</p> : null}
      {generationEnabled && catalogState === "ready" && selectedProvider ? (
        <div className="ai-summary-controls">
          <label>
            {text("AI 제공자", "AI provider")}
            <select onChange={(event) => selectProvider(event.target.value as AiSummaryProvider["id"])} value={providerId}>
              {providers.map((provider) => <option key={provider.id} value={provider.id}>{provider.label}</option>)}
            </select>
          </label>
          <label>
            {text("모델", "Model")}
            <select onChange={(event) => setModel(event.target.value)} value={model}>
              {selectedProvider.models.map((value) => <option key={value} value={value}>{value}</option>)}
            </select>
          </label>
          <label className="ai-summary-key-field">
            {text("이번 요청에만 쓸 API 키", "API key for this request only")}
            <input
              autoComplete="off"
              onChange={(event) => setOneShotApiKey(event.target.value)}
              placeholder={text(`${selectedProvider.label} API 키`, `${selectedProvider.label} API key`)}
              spellCheck="false"
              type="password"
              value={oneShotApiKey}
            />
          </label>
          <button
            className="button button-ghost ai-summary-generate"
            disabled={
              generating
              || !draft.title.trim()
              || !draft.sourceMarkdown.trim()
              || !oneShotApiKey
              || Boolean(maximumSourceBytes && sourceBytes > maximumSourceBytes)
            }
            onClick={() => void generateSummary()}
            type="button"
          >
            {generating ? text("요약 만드는 중…", "Generating summary…") : candidate || draft.aiSummary ? text("다시 생성", "Regenerate") : text("요약 초안 생성", "Generate summary draft")}
          </button>
          {maximumSourceBytes ? (
            <p className={sourceBytes > maximumSourceBytes ? "ai-summary-warning" : "ai-summary-limit"}>
              {text("제목 + 본문", "Title + body")} {sourceBytes.toLocaleString(uiLanguage === "en" ? "en-US" : "ko-KR")} / {maximumSourceBytes.toLocaleString(uiLanguage === "en" ? "en-US" : "ko-KR")} bytes
            </p>
          ) : null}
        </div>
      ) : null}

      {candidate ? (
        <div className={`ai-summary-candidate is-${candidateFreshness}`}>
          <label htmlFor="ai-summary-candidate">{text("요약 초안 확인·수정", "Review and edit summary draft")}</label>
          <textarea
            id="ai-summary-candidate"
            maxLength={2_000}
            onChange={(event) => setCandidate((current) => current ? { ...current, text: event.target.value } : current)}
            rows={5}
            value={candidate.text}
          />
          <div className="ai-summary-candidate-footer">
            <span>{aiSummaryProvenanceText(candidate)}</span>
            <button
              className="button button-primary"
              disabled={candidateFreshness !== "current" || !candidate.text.trim()}
              onClick={useCandidate}
              type="button"
            >{text("이 요약 사용", "Use this summary")}</button>
          </div>
          {candidateFreshness === "stale" ? <p className="ai-summary-warning" role="alert">{text("생성 뒤 제목이나 본문이 바뀌었습니다. 현재 글 기준으로 다시 생성해 주세요.", "The title or body changed after generation. Regenerate using the current post.")}</p> : null}
        </div>
      ) : null}
      {notice ? <p className="ai-summary-notice" role="status">{notice}</p> : null}
    </section>
  );
}

function aiSummaryFreshness(
  summary: AiSummary | undefined,
  currentSourceHash: string | null | undefined,
): "none" | "checking" | "current" | "stale" {
  if (!summary) return "none";
  if (currentSourceHash === undefined) return "checking";
  return isAiSummarySourceCurrent(summary, currentSourceHash) ? "current" : "stale";
}

function aiSummaryProvenanceText(summary: AiSummary): string {
  const provider = summary.provenance.provider === "openai"
    ? "OpenAI"
    : summary.provenance.provider === "anthropic"
      ? "Anthropic"
      : summary.provenance.provider === "google"
        ? "Google Gemini"
        : summary.provenance.provider;
  return `${provider} · ${summary.provenance.model}`;
}

function AiSummaryPreview({ summary }: { summary: AiSummary }) {
  return (
    <aside className="osb-ai-summary" aria-label={text("AI 요약", "AI summary")}>
      <p className="osb-ai-summary__label">{text("AI 요약 · 사람이 검토함", "AI summary · human reviewed")}</p>
      <p className="osb-ai-summary__text">{summary.text}</p>
    </aside>
  );
}

function SocialEmbedComposer({
  embedText,
  setDraft,
  setEmbedText,
  setStatus,
}: {
  embedText: string;
  setDraft: React.Dispatch<React.SetStateAction<CreatePostInput>>;
  setEmbedText: (value: string) => void;
  setStatus: (value: string | undefined) => void;
}) {
  const [url, setUrl] = useState("");
  const preview = useMemo(() => socialEmbedFromUrl(url, uiLanguage), [url]);

  function insert() {
    if (!preview) {
      setStatus(text("지원하는 YouTube 또는 X 게시물의 https 주소를 확인해 주세요.", "Enter a supported HTTPS URL for a YouTube video or X post."));
      return;
    }
    let current: EmbedReference[] = [];
    try {
      current = embedText.trim() ? JSON.parse(embedText) as EmbedReference[] : [];
      if (!Array.isArray(current)) throw new Error("array");
    } catch {
      setStatus(text("기존 외부 콘텐츠 연결 JSON을 먼저 확인해 주세요.", "Check the existing external-content JSON first."));
      return;
    }
    const next = [...current.filter((embed) => embed.id !== preview.id), preview];
    setEmbedText(JSON.stringify(next, null, 2));
    setDraft((draft) => {
      const directive = `::osb-embed ${preview.id}`;
      if (draft.sourceMarkdown.includes(directive)) return draft;
      const separator = draft.sourceMarkdown.trimEnd() ? "\n\n" : "";
      return { ...draft, sourceMarkdown: `${draft.sourceMarkdown.trimEnd()}${separator}${directive}\n` };
    });
    setUrl("");
    setStatus(text(`${preview.title} 연결과 본문 블록을 추가했습니다.`, `Added the ${preview.title} reference and body block.`));
  }

  return (
    <details className="social-embed-composer">
      <summary>{text("동영상·X 게시물 넣기", "Insert video or X post")} <small>{text("URL만 붙여넣으세요", "Just paste a URL")}</small></summary>
      <div className="social-embed-input-row">
        <label htmlFor="social-embed-url">{text("YouTube 또는 X 주소", "YouTube or X URL")}</label>
        <div>
          <input
            id="social-embed-url"
            inputMode="url"
            onChange={(event) => setUrl(event.target.value)}
            placeholder={text("https://youtu.be/… 또는 https://x.com/…/status/…", "https://youtu.be/… or https://x.com/…/status/…")}
            type="url"
            value={url}
          />
          <button className="button button-primary" disabled={!preview} onClick={insert} type="button">{text("본문에 넣기", "Insert into body")}</button>
        </div>
      </div>
      {url ? (
        preview ? (
          <div className="social-embed-preview" role="status">
            <span>{preview.provider === "youtube" ? "YouTube" : "X"}</span>
            <div><strong>{preview.title}</strong><small>{preview.canonicalUrl}</small></div>
            <span aria-hidden="true">✓</span>
          </div>
        ) : <p className="field-error" role="alert">{text("지원하는 공개 게시물 주소가 아닙니다.", "This is not a supported public-post URL.")}</p>
      ) : null}
    </details>
  );
}

type ToolbarCommand = "heading" | "bold" | "italic" | "strike" | "quote" | "link" | "image" | "code" | "codeblock";

function MarkdownToolbar({ onCommand }: { onCommand: (command: ToolbarCommand) => void }) {
  const tools: Array<{ command: ToolbarCommand; label: string; glyph: ReactNode }> = [
    { command: "heading", label: text("제목 2", "Heading 2"), glyph: "H₂" },
    { command: "bold", label: text("굵게", "Bold"), glyph: <strong>B</strong> },
    { command: "italic", label: text("기울임", "Italic"), glyph: <em>I</em> },
    { command: "strike", label: text("취소선", "Strikethrough"), glyph: <s>S</s> },
    { command: "quote", label: text("인용문", "Quote"), glyph: "❞" },
    { command: "link", label: text("링크", "Link"), glyph: "↗" },
    { command: "image", label: text("이미지 업로드", "Upload image"), glyph: "▧" },
    { command: "code", label: text("인라인 코드", "Inline code"), glyph: "<>" },
    { command: "codeblock", label: text("코드 블록", "Code block"), glyph: "{ }" },
  ];
  return (
    <div className="markdown-toolbar" role="toolbar" aria-label={text("Markdown 서식", "Markdown formatting")}>
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
      <summary><span>{text("AI·고급 연동", "AI and advanced integrations")}</span><small>{text("글 주소 · 초안 보관 · 외부/AI 연결", "Post address · draft storage · external/AI links")}</small></summary>
      <div className="advanced-options-body">
        <label>
          {text("공개 글 주소", "Public post address")}
          <small>{text("제목에서 자동으로 만들어집니다. 꼭 필요한 경우에만 바꾸세요.", "Generated automatically from the title. Change it only when necessary.")}</small>
          <input maxLength={240} onChange={(event) => { setSlugTouched(true); update("slug", event.target.value); }} onPaste={(event) => handlePaste("slug", event)} required value={draft.slug} />
        </label>
        <fieldset><legend>{text("브라우저 초안 보관", "Browser draft storage")}</legend>
          <p>{text("글을 쓰는 동안 서버 저장과 별개로 브라우저에 임시 보관합니다. 민감한 글이라면 범위를 줄이거나 끌 수 있습니다.", "While writing, temporarily keep a copy in the browser separately from server saves. Reduce the scope or turn it off for sensitive writing.")}</p>
          <div className="advanced-grid">
            <label>{text("보관 위치", "Storage location")}<select onChange={(event) => setStorageMode(event.target.value as DraftStorageMode)} value={storageMode}><option value="session">{text("현재 탭을 닫을 때까지", "Until this tab closes")}</option><option value="device">{text("이 기기에 일정 시간", "On this device for a set time")}</option><option value="off">{text("브라우저에 보관하지 않기", "Do not store in browser")}</option></select></label>
            {storageMode === "device" ? <label>{text("자동 삭제", "Auto-delete")}<select onChange={(event) => setRetentionHours(Number(event.target.value))} value={retentionHours}><option value={1}>{text("1시간 뒤", "After 1 hour")}</option><option value={24}>{text("24시간 뒤", "After 24 hours")}</option><option value={168}>{text("7일 뒤", "After 7 days")}</option><option value={720}>{text("30일 뒤", "After 30 days")}</option></select></label> : null}
            <label>{text("붙여넣기", "Pasting")}<select onChange={(event) => setPastePolicy(event.target.value as PastePolicy)} value={pastePolicy}><option value="allow">{text("허용", "Allow")}</option><option value="block">{text("고급 입력란에서는 막기", "Block in advanced fields")}</option></select></label>
          </div>
          <p>{text("브라우저에 보관할 항목", "Items to store in browser")}</p>
          <div className="memory-checks">
            {([
              ["core", text("기본 글(제목·주소·본문)", "Basic post (title, address, body)")],
              ["intent", text("별도 HTML 화면", "Separate HTML view")],
              ["embeds", text("외부 콘텐츠 연결 정보", "External content references")],
              ["ontology", text("AI 지식 연결 정보", "AI knowledge connections")],
              ["pasteReceipts", text("붙여넣기 기록(내용 제외)", "Paste receipts (content excluded)")],
            ] as const).map(([key, label]) => <label key={key}><input checked={memoryScopes[key]} disabled={storageMode === "off"} onChange={(event) => setMemoryScopes((current) => ({ ...current, [key]: event.target.checked }))} type="checkbox" />{label}</label>)}
          </div>
          {pasteReceipts.length ? <p>{text(`붙여넣기 시각·글자 수 기록 ${pasteReceipts.length}개 · 붙여넣은 내용은 기록하지 않습니다.`, `${pasteReceipts.length} paste time/character-count receipts · pasted content is not recorded.`)}</p> : null}
          <button className="text-button" onClick={() => { localStorage.removeItem(draftKey); sessionStorage.removeItem(draftKey); setPasteReceipts([]); setStorageMode("off"); setStatus(text("브라우저에 보관된 초안을 지웠습니다.", "Cleared the draft stored in this browser.")); }} type="button">{text("브라우저 초안 지우기", "Clear browser draft")}</button>
        </fieldset>
        <fieldset><legend>{text("별도 HTML 화면 (선택)", "Separate HTML view (optional)")}</legend>
          <p>{text("HTML을 직접 다루는 사용자만 켜세요. 안전 검사를 거친 뒤 기본 Markdown 화면과 함께 보관됩니다.", "Enable this only if you work directly with HTML. It is safety-checked and stored alongside the default Markdown view.")}</p>
          <label className="checkbox-label"><input checked={intentEnabled} onChange={(event) => { const enabled = event.target.checked; setIntentEnabled(enabled); update("intent", enabled ? { format: "enhanced-html-v1", sourceHtml: draft.intent?.sourceHtml ?? "" } : undefined); }} type="checkbox" />{text("직접 만든 HTML 화면도 사용하기", "Also use a custom HTML view")}</label>
          {intentEnabled ? <label>{text("HTML 코드", "HTML code")}<textarea onChange={(event) => update("intent", { format: "enhanced-html-v1", sourceHtml: event.target.value })} onPaste={(event) => handlePaste("intent", event)} value={draft.intent?.sourceHtml ?? ""} /></label> : null}
        </fieldset>
        <details><summary>{text("외부 콘텐츠 연결 (개발자용)", "External content references (developers)")}</summary><p>{text("동영상 같은 외부 콘텐츠를 안전한 참조 정보로 연결합니다. 본문에는", "Link external content such as video through safe reference data. Add")} <code>::osb-embed id</code>{text("를 한 줄로 넣으세요.", " on its own line in the body.")}</p><label>{text("연결 정보(JSON 배열)", "Reference data (JSON array)")}<textarea onChange={(event) => { setStatus(undefined); setEmbedText(event.target.value); }} onPaste={(event) => handlePaste("embeds", event)} value={embedText} /></label></details>
        <details><summary>{text("AI 지식 연결 (AI2AI·개발자용)", "AI knowledge connections (AI2AI/developers)")}</summary><p>{text("AI나 다른 도구가 글의 의미를 읽을 수 있도록 별도 구조화 정보를 붙입니다. 일반 글쓰기에는 필요하지 않습니다.", "Attach separate structured data so AI and other tools can interpret the post. It is not needed for ordinary writing.")}</p><label>{text("지식 연결 정보(JSON)", "Knowledge connection data (JSON)")}<textarea onChange={(event) => { setStatus(undefined); setOntologyText(event.target.value); }} onPaste={(event) => handlePaste("ontology", event)} value={ontologyText} /></label></details>
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
      <div className="publish-panel-heading"><div><p className="eyebrow">{text("공개 전 확인", "Before publishing")}</p><h2 id="publish-dialog-title">{text("글을 블로그에 공개할까요?", "Publish this post to the blog?")}</h2></div><button aria-label={text("출간 패널 닫기", "Close publish panel")} className="dialog-close" onClick={onClose} type="button">×</button></div>
      <div className="publish-summary"><span className="publish-cover" aria-hidden="true">{draft.title.slice(0, 1) || "✦"}</span><div><strong>{draft.title || text("제목 없는 글", "Untitled post")}</strong><code>/{draft.slug || "untitled"}</code></div></div>
      <div className="revision-flow" aria-label={text("출간 단계", "Publishing steps")}><div className="flow-step"><span>1</span><div><strong>{text("현재 글 저장", "Save current post")}</strong><p>{text("지금 화면의 제목과 본문을 안전한 새 버전으로 보관합니다.", "Store the title and body currently on screen as a safe new revision.")}</p></div>{exactRevisionReady ? <b aria-label={text("완료", "Complete")}>✓</b> : null}</div><div className="flow-step"><span>2</span><div><strong>{text("블로그에 공개", "Publish to blog")}</strong><p>{text("저장된 버전만 독자에게 보입니다. 작성 중인 변경은 실수로 공개되지 않습니다.", "Readers see only saved revisions. Work-in-progress changes cannot be published accidentally.")}</p></div></div></div>
      {accepted && exactRevisionReady ? <p className="revision-proof">{text("현재 내용이 저장되어 출간할 준비가 됐습니다.", "The current content is saved and ready to publish.")} <code>{accepted.currentRevisionId.slice(0, 8)}</code></p> : <p className="revision-proof warning">{accepted ? text("저장 뒤 바뀐 내용이 있습니다. 현재 내용을 한 번 더 저장해 주세요.", "Content changed after the last save. Save the current content once more.") : text("아직 서버에 저장되지 않았습니다. 먼저 현재 내용을 저장해 주세요.", "This content has not been saved to the server yet. Save it first.")}</p>}
      {status ? <p className="inline-status" role="status">{status}</p> : null}
      <div className="publish-actions"><button className="button button-ghost" disabled={saving || publishing || exactRevisionReady} onClick={onSave} type="button">{saving ? text("저장 중…", "Saving…") : exactRevisionReady ? text("현재 내용 저장됨", "Current content saved") : text("현재 내용 저장", "Save current content")}</button><button className="button button-primary" disabled={!exactRevisionReady || saving || publishing || currentRevisionPublished} onClick={onPublish} type="button">{publishing ? text("공개 중…", "Publishing…") : currentRevisionPublished ? text("이미 공개된 글", "Already published") : text("블로그에 공개", "Publish to blog")}</button></div>
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
  const { capabilities, setSession } = useSession();
  const accessKeyMethod = login && capabilities
    ? adminAuthChoices(capabilities).accessKeyMethods[0]
    : undefined;
  const localAccountLogin = Boolean(
    capabilities && studioAccessFor(capabilities) === "members",
  );
  return (
    <section
      aria-labelledby="studio-access-title"
      className="empty-state studio-access-gate"
    >
      <span className="empty-symbol" aria-hidden="true">✦</span>
      <h1 id="studio-access-title">
        {accessKeyMethod
          ? text(
            "관리자 Access Key로 Studio 열기",
            "Open Studio with an administrator access key",
          )
          : text("Studio를 열 수 없습니다", "Cannot open Studio")}
      </h1>
      <p>{detail}</p>
      {onRetry ? <button className="button button-primary" onClick={onRetry} type="button">{text("다시 시도", "Try again")}</button> : null}
      {accessKeyMethod ? (
        <div className="studio-inline-admin-access">
          <AdminAccessKeyForm
            autoFocus
            method={accessKeyMethod}
            onAuthenticated={(next) => {
              setSession(next);
              if (next.state === "authenticated" && !next.blog) navigate("/onboarding");
            }}
            submitLabel={text(
              "관리자 키로 Studio 열기",
              "Open Studio with administrator key",
            )}
          />
          {localAccountLogin ? <AppLink className="button button-ghost" href="/login">{text("계정 로그인", "Account login")}</AppLink> : null}
        </div>
      ) : login ? <AppLink className="button button-primary" href="/login">{text("로그인", "Log in")}</AppLink> : null}
      {onboarding ? <AppLink className="button button-primary" href="/onboarding">{text("블로그 만들기", "Create blog")}</AppLink> : null}
      {!login && !onboarding ? <AppLink className="button button-ghost" href="/">{text("공개 피드로 돌아가기", "Back to public feed")}</AppLink> : null}
    </section>
  );
}

function collaboratorRoleLabel(role: CollaboratorRole): string {
  return text(`${role === "editor" ? "Editor" : "Writer"} · 초안 작성·편집`, `${role === "editor" ? "Editor" : "Writer"} · create and edit drafts`);
}

function customCssProblem(value: string): string | undefined {
  if (new TextEncoder().encode(value).length > 65_536) {
    return text("CSS는 65,536 bytes 이하로 줄여 주세요.", "Reduce CSS to 65,536 bytes or less.");
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
    return text("외부 요청이나 페이지 탈출로 이어질 수 있는 CSS가 있습니다. @규칙, url()/image-set()/src(), URL, HTML, 역슬래시를 제거해 주세요.", "The CSS may make external requests or escape the page. Remove @-rules, url()/image-set()/src(), URLs, HTML, and backslashes.");
  }
  return undefined;
}

function LocalMarkdownPreview({ markdown }: { markdown: string }) {
  if (!markdown.trim()) return <p className="preview-placeholder">{text("왼쪽에 본문을 입력하면 이곳에서 읽기 흐름을 확인할 수 있습니다.", "Enter a body on the left to preview the reading flow here.")}</p>;
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

async function loadDocument(
  documentId: string,
  signal: AbortSignal,
): Promise<DocumentSnapshot> {
  return client.getStudioDocument(documentId, signal);
}

async function appendStudioRevision(
  editing: EditingTarget,
  input: CreatePostInput,
): Promise<DocumentSnapshot> {
  const payload = { ...input, baseRevisionId: editing.baseRevisionId, idempotencyKey: crypto.randomUUID() };
  return client.createStudioRevision(editing.documentId, payload);
}

function capabilityModeLabel(capabilities: Capabilities): string {
  const access = studioAccessFor(capabilities);
  if (access === "disabled") return text("읽기 전용", "Read only");
  if (access === "admin_only") return text("관리자 전용", "Administrator only");
  return text("계정별 블로그", "Per-account blogs");
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
  return date.toLocaleTimeString(uiLanguage === "en" ? "en-US" : "ko-KR", {
    hour: "2-digit",
    minute: "2-digit",
  });
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
