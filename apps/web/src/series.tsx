import {
  useEffect,
  useMemo,
  useState,
  type FormEvent,
} from "react";
import type {
  Capabilities,
  CategorySummary,
  CreateSeriesInput,
  DocumentSnapshot,
  SeriesSummary,
  ThemePresetId,
} from "@opensoverignblog/sdk";
import { adminAuthChoices, studioAccessFor } from "./auth-policy";
import { useSession } from "./app";
import {
  AppLink,
  asMessage,
  client,
  navigate,
  publicPath,
  slugify,
  text,
  usePageTitle,
} from "./lib";

type Notice = { kind: "success" | "error"; text: string };

export function StudioCreatePage({
  capabilities,
}: {
  capabilities: Capabilities | undefined;
}) {
  const { session, capabilitiesError, refreshCapabilities } = useSession();
  const [choice, setChoice] = useState<"post" | "series">();
  usePageTitle(text("새 콘텐츠", "New content"));

  if (!capabilities && capabilitiesError) {
    return (
      <SeriesGate
        detail={`${text("서버 기능을 확인하지 못했습니다", "Could not load server capabilities")}: ${capabilitiesError}`}
        retry={() => void refreshCapabilities()}
      />
    );
  }
  if (!capabilities || !session) {
    return <div className="dashboard-loading" role="status">{text("Studio 접근 권한을 확인하는 중…", "Checking Studio access…")}</div>;
  }
  if (studioAccessFor(capabilities) === "disabled") {
    return <SeriesGate detail={text("이 인스턴스는 읽기 전용입니다.", "This instance is read-only.")} />;
  }
  if (session.state !== "authenticated") {
    return (
      <SeriesGate
        detail={text("콘텐츠를 만들려면 관리자 인증이 필요합니다.", "Administrator authentication is required to create content.")}
        login
        loginLabel={loginActionLabel(capabilities)}
      />
    );
  }
  if (!session.blog) {
    return <SeriesGate detail={text("콘텐츠를 만들기 전에 블로그를 먼저 만들어 주세요.", "Create a blog before adding content.")} onboarding />;
  }
  const owner = !session.membershipRole || session.membershipRole === "owner";

  return (
    <div className="osb-site-frame studio-create-page">
      <header className="settings-hero">
        <div>
          <p className="eyebrow">{text("콘텐츠 만들기", "Create content")}</p>
          <h1>{text("무엇을 만들까요?", "What would you like to create?")}</h1>
          <p>{text("독립된 포스트를 쓰거나, 순서가 있는 시리즈를 시작할 수 있습니다.", "Write a standalone post or begin an ordered series.")}</p>
        </div>
        <AppLink className="button button-ghost" href="/studio">{text("Studio로 돌아가기", "Back to Studio")}</AppLink>
      </header>

      <div className="content-kind-grid" role="list">
        <button
          aria-pressed={choice === "post"}
          className={`content-kind-card${choice === "post" ? " is-selected" : ""}`}
          onClick={() => setChoice("post")}
          role="listitem"
          type="button"
        >
          <span className="content-kind-icon" aria-hidden="true">¶</span>
          <strong>Post</strong>
          <span>{text("한 편의 독립된 글을 작성합니다.", "Write one standalone article.")}</span>
        </button>
        <button
          aria-pressed={choice === "series"}
          className={`content-kind-card${choice === "series" ? " is-selected" : ""}`}
          disabled={!owner}
          onClick={() => setChoice("series")}
          role="listitem"
          type="button"
        >
          <span className="content-kind-icon" aria-hidden="true">☷</span>
          <strong>Series</strong>
          <span>
            {owner
              ? text("여러 글을 읽는 순서대로 묶습니다.", "Group multiple posts in a reading order.")
              : text("시리즈 생성은 블로그 소유자만 할 수 있습니다.", "Only the blog owner can create a series.")}
          </span>
        </button>
      </div>

      {choice === "post" ? (
        <section className="settings-panel content-kind-next" aria-live="polite">
          <h2>{text("새 포스트", "New post")}</h2>
          <p>{text("편집기에서 제목과 본문을 작성하고, 필요하면 기존 시리즈를 선택하세요.", "Write the title and body in the editor, then optionally choose an existing series.")}</p>
          <AppLink className="button button-primary" href="/studio/write">{text("포스트 편집기 열기", "Open post editor")}</AppLink>
        </section>
      ) : null}
      {choice === "series" ? (
        <CreateSeriesForm
          onCreated={(series) => navigate(
            `/studio/write?series=${encodeURIComponent(series.categoryId)}`,
          )}
        />
      ) : null}
    </div>
  );
}

function CreateSeriesForm({ onCreated }: { onCreated: (series: SeriesSummary) => void }) {
  const [title, setTitle] = useState("");
  const [slug, setSlug] = useState("");
  const [slugTouched, setSlugTouched] = useState(false);
  const [description, setDescription] = useState("");
  const [themePreset, setThemePreset] = useState<"" | ThemePresetId>("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setError(undefined);
    const input: CreateSeriesInput = {
      title,
      slug,
      ...(description.trim() ? { description: description.trim() } : {}),
      ...(themePreset ? { themePreset } : {}),
    };
    try {
      onCreated(await client.createStudioSeries(input));
    } catch (reason) {
      setError(asMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="settings-panel content-kind-next" aria-labelledby="create-series-title">
      <h2 id="create-series-title">{text("새 시리즈", "New series")}</h2>
      <p>{text("공개 주소는 생성 후 바뀌지 않습니다. 발행된 글은 시리즈 끝에 자동으로 추가됩니다.", "The public slug is immutable. Published posts are appended to the series automatically.")}</p>
      <form className="onboarding-form" onSubmit={(event) => void submit(event)}>
        <div className="field-grid">
          <label>
            {text("표시 이름", "Title")}
            <input
              maxLength={200}
              onChange={(event) => {
                const value = event.target.value;
                setTitle(value);
                if (!slugTouched) setSlug(slugify(value));
              }}
              placeholder={text("양자 컴퓨팅", "Quantum computing")}
              required
              value={title}
            />
          </label>
          <label>
            {text("공개 주소", "Public slug")}
            <span className="input-prefix"><span>/</span><input
              autoCapitalize="none"
              autoComplete="off"
              inputMode="url"
              maxLength={40}
              onChange={(event) => {
                setSlugTouched(true);
                setSlug(event.target.value.toLowerCase());
              }}
              pattern="[a-z0-9]+(?:-[a-z0-9]+)*"
              placeholder="yangja"
              required
              value={slug}
            /></span>
          </label>
        </div>
        <label>
          {text("설명", "Description")} <span className="field-hint">{text("선택", "Optional")}</span>
          <textarea maxLength={2000} onChange={(event) => setDescription(event.target.value)} rows={3} value={description} />
        </label>
        <label>
          {text("테마", "Theme")}
          <select onChange={(event) => setThemePreset(event.target.value as "" | ThemePresetId)} value={themePreset}>
            <option value="">{text("블로그 테마 사용", "Use blog theme")}</option>
            <option value="paper">Paper</option>
            <option value="ink">Ink</option>
            <option value="forest">Forest</option>
            <option value="terminal">Terminal</option>
          </select>
        </label>
        {error ? <p className="settings-message is-error" role="alert">{error}</p> : null}
        <div className="settings-save-row">
          <p>{slug ? <><code>/{slug}</code></> : text("주소를 입력해 주세요.", "Enter a public slug.")}</p>
          <button className="button button-primary" disabled={busy} type="submit">
            {busy ? text("만드는 중…", "Creating…") : text("시리즈 만들기", "Create series")}
          </button>
        </div>
      </form>
    </section>
  );
}

export function StudioSeriesPage({
  capabilities,
}: {
  capabilities: Capabilities | undefined;
}) {
  const { session, capabilitiesError, refreshCapabilities } = useSession();
  const [series, setSeries] = useState<SeriesSummary[]>([]);
  const [categories, setCategories] = useState<CategorySummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [notice, setNotice] = useState<Notice>();
  const [busyId, setBusyId] = useState<string>();
  usePageTitle(text("시리즈 관리", "Manage series"));

  const authenticated = session?.state === "authenticated" && Boolean(session.blog);
  const owner = authenticated && (!session.membershipRole || session.membershipRole === "owner");
  const canLoad = Boolean(capabilities && studioAccessFor(capabilities) !== "disabled" && authenticated);

  useEffect(() => {
    if (!canLoad) {
      setLoading(false);
      return;
    }
    const controller = new AbortController();
    setLoading(true);
    Promise.all([
      client.listStudioSeries(controller.signal),
      client.listStudioCategories(controller.signal),
    ])
      .then(([seriesResponse, categoryResponse]) => {
        if (!controller.signal.aborted) {
          setSeries(seriesResponse.items);
          setCategories(categoryResponse.items);
        }
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted) setNotice({ kind: "error", text: asMessage(reason) });
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [canLoad]);

  const promotable = useMemo(() => {
    const backing = new Set(series.map((item) => item.categoryId));
    return categories.filter((category) => category.status === "active" && !backing.has(category.id));
  }, [categories, series]);

  if (!capabilities && capabilitiesError) {
    return <SeriesGate detail={capabilitiesError} retry={() => void refreshCapabilities()} />;
  }
  if (!capabilities || !session) {
    return <div className="dashboard-loading" role="status">{text("Studio 접근 권한을 확인하는 중…", "Checking Studio access…")}</div>;
  }
  if (studioAccessFor(capabilities) === "disabled") {
    return <SeriesGate detail={text("이 인스턴스는 읽기 전용입니다.", "This instance is read-only.")} />;
  }
  if (session.state !== "authenticated") {
    return (
      <SeriesGate
        detail={text("시리즈를 관리하려면 먼저 인증해 주세요.", "Authenticate to manage series.")}
        login
        loginLabel={loginActionLabel(capabilities)}
      />
    );
  }
  if (!session.blog) {
    return <SeriesGate detail={text("블로그를 먼저 만들어 주세요.", "Create a blog first.")} onboarding />;
  }

  async function promote(category: CategorySummary) {
    setBusyId(category.id);
    setNotice(undefined);
    try {
      const created = await client.promoteStudioCategoryToSeries(category.id);
      setSeries((current) => [...current, created].sort((left, right) => left.homePosition - right.homePosition));
      setNotice({ kind: "success", text: text(`${category.title}을(를) 시리즈로 전환했습니다.`, `${category.title} is now a series.`) });
    } catch (reason) {
      setNotice({ kind: "error", text: asMessage(reason) });
    } finally {
      setBusyId(undefined);
    }
  }

  async function archive(item: SeriesSummary) {
    if (!window.confirm(text(`"${item.title}" 시리즈를 보관할까요?`, `Archive the series “${item.title}”?`))) return;
    setBusyId(item.id);
    try {
      const updated = await client.archiveStudioSeries(item.id);
      setSeries((current) => current.map((value) => value.id === updated.id ? updated : value));
      setNotice({ kind: "success", text: text("시리즈를 보관했습니다.", "Series archived.") });
    } catch (reason) {
      setNotice({ kind: "error", text: asMessage(reason) });
    } finally {
      setBusyId(undefined);
    }
  }

  return (
    <div className="osb-site-frame settings-page series-page">
      <header className="settings-hero">
        <div>
          <p className="eyebrow">Series Studio</p>
          <h1>{text("시리즈 관리", "Manage series")}</h1>
          <p>{text("글 묶음의 공개 정보와 읽는 순서를 관리합니다.", "Manage collection metadata and reading order.")}</p>
        </div>
        <div className="settings-hero-actions">
          <AppLink className="button button-primary" href="/studio/new">{text("새 콘텐츠", "New content")}</AppLink>
          <AppLink className="button button-ghost" href="/studio">{text("Studio로 돌아가기", "Back to Studio")}</AppLink>
        </div>
      </header>

      {notice ? <p className={`settings-message is-${notice.kind}`} role={notice.kind === "error" ? "alert" : "status"}>{notice.text}</p> : null}
      {loading ? <div className="settings-loading" role="status">{text("시리즈를 불러오는 중…", "Loading series…")}</div> : null}

      {!loading && owner && promotable.length ? (
        <section className="settings-panel" aria-labelledby="promote-series-title">
          <h2 id="promote-series-title">{text("기존 카테고리를 시리즈로 전환", "Promote an existing category")}</h2>
          <p>{text("주소와 발행 글은 그대로 두고, 현재 글을 오래된 순서부터 시리즈에 넣습니다.", "Keep the route and posts, then order current posts from oldest to newest.")}</p>
          <div className="series-promote-list">
            {promotable.map((category) => (
              <div key={category.id}>
                <span><strong>{category.title}</strong><code>/{category.slug}</code></span>
                <button className="button button-ghost" disabled={busyId === category.id} onClick={() => void promote(category)} type="button">
                  {busyId === category.id ? text("전환 중…", "Promoting…") : text("시리즈로 전환", "Promote")}
                </button>
              </div>
            ))}
          </div>
        </section>
      ) : null}

      {!loading && series.length === 0 ? (
        <div className="dashboard-empty">
          <span aria-hidden="true">☷</span>
          <h2>{text("아직 시리즈가 없습니다", "No series yet")}</h2>
          <p>{text("새 시리즈를 만들거나 기존 카테고리를 전환하세요.", "Create a series or promote an existing category.")}</p>
          <AppLink className="button button-primary" href="/studio/new">{text("시리즈 만들기", "Create a series")}</AppLink>
        </div>
      ) : null}

      <div className="series-manager-list">
        {series.map((item) => (
          <SeriesManagerCard
            busy={busyId === item.id}
            item={item}
            key={item.id}
            onUpdated={(updated) => setSeries((current) => current.map((value) => value.id === updated.id ? updated : value))}
            owner={Boolean(owner)}
            publicHref={session.blog?.isPrimary ? `/${item.slug}` : `/@${session.blog?.handle}/${item.slug}`}
            {...(owner ? { onArchive: archive } : {})}
          />
        ))}
      </div>
    </div>
  );
}

function SeriesManagerCard({
  busy,
  item,
  onArchive,
  onUpdated,
  owner,
  publicHref,
}: {
  busy: boolean;
  item: SeriesSummary;
  onArchive?: (item: SeriesSummary) => Promise<void>;
  onUpdated: (item: SeriesSummary) => void;
  owner: boolean;
  publicHref: string;
}) {
  const [items, setItems] = useState<DocumentSnapshot[]>();
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();

  async function loadItems(open: boolean) {
    if (!open || items) return;
    setLoading(true);
    try {
      setItems(await client.listStudioSeriesItems(item.id));
    } catch (reason) {
      setError(asMessage(reason));
    } finally {
      setLoading(false);
    }
  }

  function move(index: number, direction: -1 | 1) {
    setItems((current) => {
      if (!current) return current;
      const target = index + direction;
      if (target < 0 || target >= current.length) return current;
      const next = [...current];
      [next[index], next[target]] = [next[target]!, next[index]!];
      return next;
    });
  }

  async function saveOrder() {
    if (!items) return;
    setSaving(true);
    setError(undefined);
    try {
      setItems(await client.replaceStudioSeriesOrder(item.id, items.map((document) => document.id)));
    } catch (reason) {
      setError(asMessage(reason));
    } finally {
      setSaving(false);
    }
  }

  async function rename(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    setSaving(true);
    try {
      onUpdated(await client.updateStudioSeries(item.id, {
        title: String(data.get("title") ?? ""),
        ...(String(data.get("description") ?? "").trim()
          ? { description: String(data.get("description")).trim() }
          : {}),
        ...(String(data.get("themePreset") ?? "") ? { themePreset: String(data.get("themePreset")) as ThemePresetId } : {}),
      }));
    } catch (reason) {
      setError(asMessage(reason));
    } finally {
      setSaving(false);
    }
  }

  return (
    <details className="settings-panel series-manager-card" onToggle={(event) => void loadItems(event.currentTarget.open)}>
      <summary>
        <span><span className="mode-pill">Series</span><strong>{item.title}</strong><code>/{item.slug}</code></span>
        <span>{item.status === "active" ? text("활성", "Active") : text("보관됨", "Archived")}</span>
      </summary>
      <div className="series-manager-body">
        <div className="series-card-actions">
          <a href={publicPath(publicHref)}>{text("공개 페이지", "Public page")} ↗</a>
          {item.status === "active" ? (
            <AppLink href={`/studio/write?series=${encodeURIComponent(item.categoryId)}`}>
              {text("이 시리즈에 글 추가", "Add a post to this series")}
            </AppLink>
          ) : null}
          {onArchive && item.status === "active" ? (
            <button className="button button-danger" disabled={busy} onClick={() => void onArchive(item)} type="button">{text("보관", "Archive")}</button>
          ) : null}
        </div>
        {owner && item.status === "active" ? (
          <form className="series-inline-form" onSubmit={(event) => void rename(event)}>
            <label>{text("제목", "Title")}<input defaultValue={item.title} maxLength={200} name="title" required /></label>
            <label>{text("설명", "Description")}<textarea defaultValue={item.description ?? ""} maxLength={2000} name="description" rows={2} /></label>
            <label>{text("테마", "Theme")}<select defaultValue={item.themePreset ?? ""} name="themePreset">
              <option value="">{text("블로그 테마", "Blog theme")}</option>
              <option value="paper">Paper</option><option value="ink">Ink</option><option value="forest">Forest</option><option value="terminal">Terminal</option>
            </select></label>
            <button className="button button-ghost" disabled={saving} type="submit">{text("정보 저장", "Save details")}</button>
          </form>
        ) : null}
        <section aria-label={text("시리즈 글 순서", "Series post order")}>
          <h3>{text("읽는 순서", "Reading order")}</h3>
          {loading ? <p role="status">{text("글을 불러오는 중…", "Loading posts…")}</p> : null}
          {!loading && items?.length === 0 ? <p>{text("발행된 글이 아직 없습니다.", "No published posts yet.")}</p> : null}
          {items?.length ? (
            <ol className="series-order-list">
              {items.map((document, index) => (
                <li key={document.id}>
                  <span><b>{index + 1}</b><span>{document.revision.title}</span></span>
                  {owner ? <span>
                    <button aria-label={text(`${document.revision.title} 위로`, `Move ${document.revision.title} up`)} disabled={index === 0} onClick={() => move(index, -1)} type="button">↑</button>
                    <button aria-label={text(`${document.revision.title} 아래로`, `Move ${document.revision.title} down`)} disabled={index === items.length - 1} onClick={() => move(index, 1)} type="button">↓</button>
                  </span> : null}
                </li>
              ))}
            </ol>
          ) : null}
          {owner && items?.length ? <button className="button button-primary" disabled={saving} onClick={() => void saveOrder()} type="button">{saving ? text("저장 중…", "Saving…") : text("순서 저장", "Save order")}</button> : null}
        </section>
        {error ? <p className="settings-message is-error" role="alert">{error}</p> : null}
      </div>
    </details>
  );
}

function SeriesGate({
  detail,
  login = false,
  loginLabel,
  onboarding = false,
  retry,
}: {
  detail: string;
  login?: boolean;
  loginLabel?: string;
  onboarding?: boolean;
  retry?: () => void;
}) {
  return (
    <div className="osb-site-frame dashboard-shell">
      <section className="dashboard-empty">
        <span aria-hidden="true">☷</span>
        <h1>{text("시리즈 Studio", "Series Studio")}</h1>
        <p>{detail}</p>
        {login ? <AppLink className="button button-primary" href="/login">{loginLabel ?? text("로그인", "Log in")}</AppLink> : null}
        {onboarding ? <AppLink className="button button-primary" href="/onboarding">{text("블로그 만들기", "Create blog")}</AppLink> : null}
        {retry ? <button className="button button-primary" onClick={retry} type="button">{text("다시 시도", "Try again")}</button> : null}
      </section>
    </div>
  );
}

function loginActionLabel(capabilities: Capabilities): string {
  return adminAuthChoices(capabilities).accessKeyMethods.length
    ? text("관리자 키 입력", "Enter administrator key")
    : text("로그인", "Log in");
}
