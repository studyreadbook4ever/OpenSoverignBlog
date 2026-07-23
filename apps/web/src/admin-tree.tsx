import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import type { AdminTreeNode } from "@opensoverignblog/sdk";
import { AdminAccessKeyForm } from "./admin-access";
import { useSession } from "./app";
import { adminAuthChoices } from "./auth-policy";
import { AppLink, asMessage, client, text, uiLanguage, usePageTitle } from "./lib";

const ROOT_NODE_ID = "root";
const PAGE_SIZE = 100;

interface BranchState {
  items: AdminTreeNode[];
  loaded: boolean;
  loading: boolean;
  nextCursor?: string;
  failedCursor?: string;
  error?: string;
}

const EMPTY_BRANCH: BranchState = {
  items: [],
  loaded: false,
  loading: false,
};

export function AdminTreePage() {
  const { session, sessionError } = useSession();
  const [branches, setBranches] = useState<Record<string, BranchState>>({});
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const [activeKey, setActiveKey] = useState<string>();
  const [selectedId, setSelectedId] = useState<string>();
  const [generatedAt, setGeneratedAt] = useState<string>();
  const [announcement, setAnnouncement] = useState("");
  const treeRef = useRef<HTMLUListElement>(null);
  const controllersRef = useRef<Set<AbortController>>(new Set());
  const loadingParentsRef = useRef<Map<string, AbortController>>(new Map());
  const canInspect = session?.state === "authenticated" && session.instanceAdministrator;
  const canInspectRef = useRef(canInspect);
  canInspectRef.current = canInspect;
  usePageTitle(text("프로그램 트리", "Program tree"));

  function abortRequests() {
    for (const controller of controllersRef.current) controller.abort();
    controllersRef.current.clear();
    loadingParentsRef.current.clear();
  }

  async function loadBranch(parentId: string, cursor?: string) {
    if (!canInspectRef.current || loadingParentsRef.current.has(parentId)) return;
    const controller = new AbortController();
    controllersRef.current.add(controller);
    loadingParentsRef.current.set(parentId, controller);
    setBranches((current) => {
      const previous = current[parentId] ?? EMPTY_BRANCH;
      const loadingBranch: BranchState = { ...previous, loading: true };
      delete loadingBranch.error;
      delete loadingBranch.failedCursor;
      return {
        ...current,
        [parentId]: loadingBranch,
      };
    });
    setAnnouncement(cursor
      ? text("다음 트리 항목을 불러오는 중입니다.", "Loading more tree items.")
      : text("트리 항목을 불러오는 중입니다.", "Loading tree items."));

    try {
      const page = await client.adminTree(
        { parent: parentId, ...(cursor ? { cursor } : {}), limit: PAGE_SIZE },
        controller.signal,
      );
      if (controller.signal.aborted || !canInspectRef.current) return;
      setBranches((current) => {
        const previousItems = cursor ? (current[parentId]?.items ?? []) : [];
        const knownIds = new Set(previousItems.map((item) => item.id));
        const items = [
          ...previousItems,
          ...page.items.filter((item) => !knownIds.has(item.id)),
        ];
        return {
          ...current,
          [parentId]: {
            items,
            loaded: true,
            loading: false,
            ...(page.nextCursor ? { nextCursor: page.nextCursor } : {}),
          },
        };
      });
      if (parentId === ROOT_NODE_ID) setGeneratedAt(page.generatedAt);
      const firstNewNode = page.items[0];
      setActiveKey((current) => current ?? (firstNewNode ? nodeFocusKey(firstNewNode.id) : undefined));
      setAnnouncement(
        page.items.length
          ? text(`${page.items.length}개의 트리 항목을 불러왔습니다.`, `Loaded ${page.items.length} tree items.`)
          : text("이 위치에는 하위 항목이 없습니다.", "This location has no child items."),
      );
      if (cursor) {
        focusTreeKey(firstNewNode ? nodeFocusKey(firstNewNode.id) : nodeFocusKey(parentId));
      }
    } catch (reason) {
      if (controller.signal.aborted || !canInspectRef.current) return;
      const message = asMessage(reason);
      setBranches((current) => {
        const previous = current[parentId] ?? EMPTY_BRANCH;
        return {
          ...current,
          [parentId]: {
            ...previous,
            loading: false,
            error: message,
            ...(cursor ? { failedCursor: cursor } : {}),
          },
        };
      });
      setAnnouncement(text(`트리 항목을 불러오지 못했습니다: ${message}`, `Could not load tree items: ${message}`));
    } finally {
      controllersRef.current.delete(controller);
      if (loadingParentsRef.current.get(parentId) === controller) {
        loadingParentsRef.current.delete(parentId);
      }
    }
  }

  useEffect(() => {
    abortRequests();
    setBranches({});
    setExpanded(new Set());
    setActiveKey(undefined);
    setSelectedId(undefined);
    setGeneratedAt(undefined);
    setAnnouncement("");
    if (canInspect) void loadBranch(ROOT_NODE_ID);
    return abortRequests;
    // Access transitions intentionally discard all previously inspected data.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canInspect]);

  const selectedNode = useMemo(() => {
    if (!selectedId) return undefined;
    for (const branch of Object.values(branches)) {
      const found = branch.items.find((item) => item.id === selectedId);
      if (found) return found;
    }
    return undefined;
  }, [branches, selectedId]);

  function focusTreeKey(key: string) {
    setActiveKey(key);
    window.requestAnimationFrame(() => {
      const target = visibleTreeItems().find((item) => item.dataset.treeFocusKey === key);
      target?.focus({ preventScroll: true });
    });
  }

  function visibleTreeItems(): HTMLButtonElement[] {
    return Array.from(
      treeRef.current?.querySelectorAll<HTMLButtonElement>("button[role='treeitem'][data-tree-focus-key]")
        ?? [],
    );
  }

  function moveInTree(event: KeyboardEvent<HTMLButtonElement>, key: string, offset: -1 | 1) {
    event.preventDefault();
    const items = visibleTreeItems();
    const index = items.findIndex((item) => item.dataset.treeFocusKey === key);
    const target = items[index + offset];
    if (target?.dataset.treeFocusKey) focusTreeKey(target.dataset.treeFocusKey);
  }

  function moveToTreeEdge(event: KeyboardEvent<HTMLButtonElement>, edge: "first" | "last") {
    event.preventDefault();
    const items = visibleTreeItems();
    const target = edge === "first" ? items[0] : items.at(-1);
    if (target?.dataset.treeFocusKey) focusTreeKey(target.dataset.treeFocusKey);
  }

  function toggleNode(node: AdminTreeNode) {
    setSelectedId(node.id);
    if (!node.hasChildren) return;
    const opening = !expanded.has(node.id);
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(node.id)) next.delete(node.id);
      else next.add(node.id);
      return next;
    });
    if (opening && !branches[node.id]?.loaded && !branches[node.id]?.loading) {
      void loadBranch(node.id);
    }
  }

  function handleNodeKeyDown(event: KeyboardEvent<HTMLButtonElement>, node: AdminTreeNode) {
    const key = nodeFocusKey(node.id);
    switch (event.key) {
      case "ArrowDown":
        moveInTree(event, key, 1);
        break;
      case "ArrowUp":
        moveInTree(event, key, -1);
        break;
      case "Home":
        moveToTreeEdge(event, "first");
        break;
      case "End":
        moveToTreeEdge(event, "last");
        break;
      case "ArrowRight": {
        if (!node.hasChildren) break;
        event.preventDefault();
        if (!expanded.has(node.id)) {
          toggleNode(node);
          break;
        }
        const firstChild = branches[node.id]?.items[0];
        if (firstChild) focusTreeKey(nodeFocusKey(firstChild.id));
        break;
      }
      case "ArrowLeft":
        if (node.hasChildren && expanded.has(node.id)) {
          event.preventDefault();
          setExpanded((current) => {
            const next = new Set(current);
            next.delete(node.id);
            return next;
          });
        } else if (node.parentId !== ROOT_NODE_ID) {
          event.preventDefault();
          focusTreeKey(nodeFocusKey(node.parentId));
        }
        break;
      case "Enter":
      case " ":
        event.preventDefault();
        toggleNode(node);
        break;
      default:
        break;
    }
  }

  function handleActionKeyDown(
    event: KeyboardEvent<HTMLButtonElement>,
    focusKey: string,
    parentId: string,
    activate: () => void,
  ) {
    switch (event.key) {
      case "ArrowDown":
        moveInTree(event, focusKey, 1);
        break;
      case "ArrowUp":
        moveInTree(event, focusKey, -1);
        break;
      case "Home":
        moveToTreeEdge(event, "first");
        break;
      case "End":
        moveToTreeEdge(event, "last");
        break;
      case "ArrowLeft":
        if (parentId !== ROOT_NODE_ID) {
          event.preventDefault();
          focusTreeKey(nodeFocusKey(parentId));
        }
        break;
      case "Enter":
      case " ":
        event.preventDefault();
        activate();
        break;
      default:
        break;
    }
  }

  function renderNodes(nodes: AdminTreeNode[], level: number): ReactNode {
    return nodes.map((node, index) => {
      const focusKey = nodeFocusKey(node.id);
      const isExpanded = node.hasChildren && expanded.has(node.id);
      const branch = branches[node.id];
      const retryCursor = branch?.failedCursor;
      return (
        <li key={node.id} role="none">
          <button
            aria-expanded={node.hasChildren ? isExpanded : undefined}
            aria-level={level}
            aria-posinset={index + 1}
            aria-selected={selectedId === node.id}
            aria-setsize={nodes.length}
            className={`admin-tree-node admin-tree-node-${node.kind}`}
            data-tree-focus-key={focusKey}
            onClick={() => {
              setActiveKey(focusKey);
              toggleNode(node);
            }}
            onFocus={() => setActiveKey(focusKey)}
            onKeyDown={(event) => handleNodeKeyDown(event, node)}
            role="treeitem"
            tabIndex={activeKey === focusKey ? 0 : -1}
            type="button"
          >
            <span aria-hidden="true" className="admin-tree-disclosure">
              {node.hasChildren ? (isExpanded ? "▾" : "▸") : "·"}
            </span>
            <span className="admin-tree-node-label">{node.label}</span>
            <span className="admin-tree-node-kind">{nodeKindLabel(node.kind)}</span>
            {node.state ? <span className="status-badge">{node.state}</span> : null}
          </button>
          {isExpanded ? (
            <ul
              aria-busy={branch?.loading ?? true}
              aria-label={text(`${node.label} 하위 항목`, `Children of ${node.label}`)}
              className="admin-tree-group"
              role="group"
            >
              {branch?.items.length ? renderNodes(branch.items, level + 1) : null}
              {branch?.loaded && !branch.loading && !branch.error && branch.items.length === 0 ? (
                <li role="none">
                  <span aria-disabled="true" aria-level={level + 1} className="admin-tree-empty" role="treeitem">
                    {text("하위 항목 없음", "No child items")}
                  </span>
                </li>
              ) : null}
              {branch?.error ? (
                <TreeActionItem
                  activeKey={activeKey}
                  focusKey={`retry:${node.id}`}
                  label={text("불러오기 다시 시도", "Retry loading")}
                  level={level + 1}
                  onActivate={() => void loadBranch(node.id, retryCursor)}
                  onFocus={setActiveKey}
                  onKeyDown={(event) => handleActionKeyDown(
                    event,
                    `retry:${node.id}`,
                    node.id,
                    () => void loadBranch(node.id, retryCursor),
                  )}
                />
              ) : branch?.nextCursor ? (
                <TreeActionItem
                  activeKey={activeKey}
                  ariaDisabled={branch.loading}
                  focusKey={`more:${node.id}`}
                  label={branch.loading ? text("다음 항목 불러오는 중…", "Loading more items…") : text("다음 항목 불러오기", "Load more items")}
                  level={level + 1}
                  onActivate={() => {
                    if (!branch.loading) void loadBranch(node.id, branch.nextCursor);
                  }}
                  onFocus={setActiveKey}
                  onKeyDown={(event) => handleActionKeyDown(
                    event,
                    `more:${node.id}`,
                    node.id,
                    () => {
                      if (!branch.loading) void loadBranch(node.id, branch.nextCursor);
                    },
                  )}
                />
              ) : null}
            </ul>
          ) : null}
        </li>
      );
    });
  }

  function refreshTree() {
    if (!canInspectRef.current) return;
    abortRequests();
    setBranches({});
    setExpanded(new Set());
    setActiveKey(undefined);
    setSelectedId(undefined);
    setGeneratedAt(undefined);
    void loadBranch(ROOT_NODE_ID);
  }

  if (!session) {
    return <div className="dashboard-loading" role="status">{text("관리 권한을 확인하는 중…", "Checking administrator access…")}</div>;
  }
  if (session.state !== "authenticated") {
    return (
      <AdminTreeAccessGate
        detail={sessionError
          ? text(`관리 세션을 확인하지 못했습니다: ${sessionError}`, `Could not verify the administrator session: ${sessionError}`)
          : text("프로그램 트리는 인증된 인스턴스 관리자만 볼 수 있습니다.", "Only authenticated instance administrators can view the program tree.")}
        login
      />
    );
  }
  if (!session.instanceAdministrator) {
    return <AdminTreeAccessGate detail={text("블로그 멤버 권한으로는 서버 전체 프로그램 트리를 볼 수 없습니다.", "Blog membership does not grant access to the server-wide program tree.")} />;
  }

  const root = branches[ROOT_NODE_ID];
  return (
    <div className="studio-settings-page admin-tree-page">
      <header className="settings-heading">
        <div>
          <p className="eyebrow">Instance inspector</p>
          <h1>{text("프로그램 트리", "Program tree")}</h1>
          <p>{text("콘텐츠 구조와 활성 모듈, 공개 가능한 운영 상태만 계층별로 확인합니다.", "Inspect content structure, active modules, and safe operational status as a hierarchy.")}</p>
        </div>
        <button className="button button-ghost" disabled={root?.loading} onClick={refreshTree} type="button">
          {text("새로고침", "Refresh")}
        </button>
      </header>

      {root?.loading && !root.loaded ? (
        <div className="dashboard-loading" role="status">{text("프로그램 트리를 불러오는 중…", "Loading program tree…")}</div>
      ) : null}
      {root?.error && !root.loaded ? (
        <section className="settings-panel" role="alert">
          <h2>{text("트리를 불러오지 못했습니다", "Could not load the tree")}</h2>
          <p>{root.error}</p>
          <button className="button button-ghost" onClick={() => void loadBranch(ROOT_NODE_ID)} type="button">
            {text("다시 시도", "Try again")}
          </button>
        </section>
      ) : null}
      {root?.loaded ? (
        <div className="admin-tree-workspace">
          <section className="settings-panel admin-tree-browser" aria-labelledby="admin-tree-title">
            <div className="section-heading">
              <div>
                <p className="eyebrow">Safe projection</p>
                <h2 id="admin-tree-title">{text("설치 구조", "Installation structure")}</h2>
              </div>
              {generatedAt ? <time dateTime={generatedAt}>{text(`${formatTimestamp(generatedAt)} 기준`, `As of ${formatTimestamp(generatedAt)}`)}</time> : null}
            </div>
            {root.items.length ? (
              <ul
                aria-busy={root.loading}
                aria-label={text("OpenSoverignBlog 프로그램 구조", "OpenSoverignBlog program structure")}
                className="admin-program-tree"
                ref={treeRef}
                role="tree"
              >
                {renderNodes(root.items, 1)}
                {root.error ? (
                  <TreeActionItem
                    activeKey={activeKey}
                    focusKey="retry:root"
                    label={text("다음 루트 항목 다시 불러오기", "Retry loading more root items")}
                    level={1}
                    onActivate={() => void loadBranch(ROOT_NODE_ID, root.failedCursor)}
                    onFocus={setActiveKey}
                    onKeyDown={(event) => handleActionKeyDown(
                      event,
                      "retry:root",
                      ROOT_NODE_ID,
                      () => void loadBranch(ROOT_NODE_ID, root.failedCursor),
                    )}
                  />
                ) : root.nextCursor ? (
                  <TreeActionItem
                    activeKey={activeKey}
                    ariaDisabled={root.loading}
                    focusKey="more:root"
                    label={root.loading ? text("다음 루트 항목 불러오는 중…", "Loading more root items…") : text("다음 루트 항목 불러오기", "Load more root items")}
                    level={1}
                    onActivate={() => {
                      if (!root.loading) void loadBranch(ROOT_NODE_ID, root.nextCursor);
                    }}
                    onFocus={setActiveKey}
                    onKeyDown={(event) => handleActionKeyDown(
                      event,
                      "more:root",
                      ROOT_NODE_ID,
                      () => {
                        if (!root.loading) void loadBranch(ROOT_NODE_ID, root.nextCursor);
                      },
                    )}
                  />
                ) : null}
              </ul>
            ) : <p>{text("표시할 안전한 운영 메타데이터가 없습니다.", "No safe operational metadata is available to display.")}</p>}
          </section>
          <NodeInspector node={selectedNode} />
        </div>
      ) : null}
      <p aria-live="polite" className="inline-status" role="status">{announcement}</p>
    </div>
  );
}

interface TreeActionItemProps {
  activeKey: string | undefined;
  ariaDisabled?: boolean;
  focusKey: string;
  label: string;
  level: number;
  onActivate: () => void;
  onFocus: (key: string) => void;
  onKeyDown: (event: KeyboardEvent<HTMLButtonElement>) => void;
}

function TreeActionItem({
  activeKey,
  ariaDisabled = false,
  focusKey,
  label,
  level,
  onActivate,
  onFocus,
  onKeyDown,
}: TreeActionItemProps) {
  return (
    <li role="none">
      <button
        aria-disabled={ariaDisabled}
        aria-level={level}
        className="admin-tree-action"
        data-tree-focus-key={focusKey}
        onClick={() => {
          onFocus(focusKey);
          if (!ariaDisabled) onActivate();
        }}
        onFocus={() => onFocus(focusKey)}
        onKeyDown={onKeyDown}
        role="treeitem"
        tabIndex={activeKey === focusKey ? 0 : -1}
        type="button"
      >
        {label}
      </button>
    </li>
  );
}

function NodeInspector({ node }: { node: AdminTreeNode | undefined }) {
  if (!node) {
    return (
      <aside className="settings-panel admin-tree-inspector" aria-labelledby="admin-tree-inspector-title">
        <h2 id="admin-tree-inspector-title">{text("항목 정보", "Item details")}</h2>
        <p>{text("트리에서 항목을 선택하면 허용된 메타데이터만 여기에 표시됩니다.", "Select an item in the tree to display only its permitted metadata here.")}</p>
      </aside>
    );
  }
  const details: Array<[string, string]> = [
    [text("종류", "Kind"), nodeKindLabel(node.kind)],
    [text("노드 ID", "Node ID"), node.id],
    [text("상위 노드", "Parent node"), node.parentId],
    ...(node.entityId ? [[text("엔티티 ID", "Entity ID"), node.entityId] as [string, string]] : []),
    ...(node.handle ? [[text("핸들", "Handle"), node.handle] as [string, string]] : []),
    ...(node.slug ? [[text("슬러그", "Slug"), node.slug] as [string, string]] : []),
    ...(node.state ? [[text("상태", "State"), node.state] as [string, string]] : []),
    ...(node.revisionNumber !== undefined
      ? [[text("리비전", "Revision"), String(node.revisionNumber)] as [string, string]]
      : []),
    ...(node.requested !== undefined
      ? [[text("요청됨", "Requested"), node.requested ? text("예", "Yes") : text("아니요", "No")] as [string, string]]
      : []),
    ...(node.operational !== undefined
      ? [[text("동작 중", "Operational"), node.operational ? text("예", "Yes") : text("아니요", "No")] as [string, string]]
      : []),
    ...(node.summary ? [[text("설명", "Summary"), node.summary] as [string, string]] : []),
    ...(node.createdAt ? [[text("생성", "Created"), formatTimestamp(node.createdAt)] as [string, string]] : []),
    ...(node.updatedAt ? [[text("수정", "Updated"), formatTimestamp(node.updatedAt)] as [string, string]] : []),
  ];
  return (
    <aside className="settings-panel admin-tree-inspector" aria-labelledby="admin-tree-inspector-title">
      <p className="eyebrow">Selected node</p>
      <h2 id="admin-tree-inspector-title">{node.label}</h2>
      <dl>
        {details.map(([label, value]) => (
          <div key={label}>
            <dt>{label}</dt>
            <dd>{value}</dd>
          </div>
        ))}
      </dl>
    </aside>
  );
}

function AdminTreeAccessGate({ detail, login = false }: { detail: string; login?: boolean }) {
  const { capabilities, setSession } = useSession();
  const accessKeyMethod = login && capabilities
    ? adminAuthChoices(capabilities).accessKeyMethods[0]
    : undefined;
  return (
    <section className="studio-access-gate" aria-labelledby="admin-tree-access-title">
      <p className="eyebrow">Instance administrator only</p>
      <h1 id="admin-tree-access-title">{text("프로그램 트리를 열 수 없습니다", "Cannot open the program tree")}</h1>
      <p>{detail}</p>
      {accessKeyMethod ? (
        <div className="studio-inline-admin-access">
          <AdminAccessKeyForm method={accessKeyMethod} onAuthenticated={setSession} />
        </div>
      ) : login ? <AppLink className="button button-primary" href="/login">{text("관리자로 인증하기", "Authenticate as administrator")}</AppLink> : null}
    </section>
  );
}

function nodeFocusKey(id: string): string {
  return `node:${id}`;
}

function nodeKindLabel(kind: AdminTreeNode["kind"]): string {
  switch (kind) {
    case "group":
      return text("그룹", "Group");
    case "site":
      return text("사이트", "Site");
    case "category":
      return text("카테고리", "Category");
    case "document":
      return text("문서", "Document");
    case "revision":
      return text("리비전", "Revision");
    case "setting":
      return text("설정", "Setting");
    case "module":
      return text("모듈", "Module");
    case "runtime":
      return text("런타임", "Runtime");
  }
}

function formatTimestamp(value: string): string {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return value;
  return new Intl.DateTimeFormat(uiLanguage === "en" ? "en-US" : "ko-KR", {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(parsed);
}
