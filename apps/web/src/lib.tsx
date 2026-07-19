import { useEffect, useState, type AnchorHTMLAttributes, type MouseEvent } from "react";
import {
  OpenSoverignBlogClient,
  OpenSoverignBlogError,
  type ThemePresetId,
} from "@opensoverignblog/sdk";

export const basePath = readBasePath();

export const client = new OpenSoverignBlogClient({
  baseUrl: basePath === "/" ? "" : basePath,
});

export interface ThemePreset {
  id: ThemePresetId;
  name: string;
  description: string;
  sampleTitle: string;
}

export const THEME_PRESETS: ThemePreset[] = [
  {
    id: "paper",
    name: "Paper",
    description: "따뜻한 종이 위에 차분한 세리프 활자를 올린 읽기 중심 테마",
    sampleTitle: "생각을 오래 남기는 법",
  },
  {
    id: "ink",
    name: "Ink",
    description: "흑백 대비와 넉넉한 여백으로 글의 구조를 또렷하게 보여주는 테마",
    sampleTitle: "명료한 문장을 위한 노트",
  },
  {
    id: "forest",
    name: "Forest",
    description: "깊은 초록과 부드러운 크림색이 어우러진 편안한 테마",
    sampleTitle: "숲에서 배운 느린 리듬",
  },
  {
    id: "terminal",
    name: "Terminal",
    description: "모노스페이스 활자와 선명한 상태색을 사용한 개발 기록 테마",
    sampleTitle: "build: 작은 도구의 탄생",
  },
];

export function isNotFound(reason: unknown): boolean {
  return reason instanceof OpenSoverignBlogError && reason.status === 404;
}

export function asMessage(value: unknown): string {
  return value instanceof Error ? value.message : "알 수 없는 오류가 발생했습니다.";
}

export function navigate(href: string, replace = false) {
  const destination = publicPath(href);
  if (replace) window.history.replaceState(null, "", destination);
  else window.history.pushState(null, "", destination);
  window.dispatchEvent(new PopStateEvent("popstate"));
  window.scrollTo({ top: 0, behavior: "auto" });
}

export function AppLink({
  href,
  onClick,
  ...props
}: AnchorHTMLAttributes<HTMLAnchorElement> & { href: string }) {
  const renderedHref = publicPath(href);
  function follow(event: MouseEvent<HTMLAnchorElement>) {
    onClick?.(event);
    if (
      event.defaultPrevented ||
      event.button !== 0 ||
      event.metaKey ||
      event.ctrlKey ||
      event.shiftKey ||
      event.altKey ||
      props.target === "_blank" ||
      !href.startsWith("/")
    ) {
      return;
    }
    event.preventDefault();
    navigate(href);
  }
  return <a {...props} href={renderedHref} onClick={follow} />;
}

export function usePathname(): string {
  const [pathname, setPathname] = useState(() => applicationPath(window.location.pathname));
  useEffect(() => {
    const update = () => setPathname(applicationPath(window.location.pathname));
    window.addEventListener("popstate", update);
    return () => window.removeEventListener("popstate", update);
  }, []);
  return pathname;
}

export function publicPath(path: string): string {
  if (!path.startsWith("/") || path.startsWith("//") || basePath === "/") return path;
  return path === "/" ? `${basePath}/` : `${basePath}${path}`;
}

function applicationPath(pathname: string): string {
  if (basePath === "/") return pathname;
  if (pathname === basePath || pathname === `${basePath}/`) return "/";
  return pathname.startsWith(`${basePath}/`) ? pathname.slice(basePath.length) : pathname;
}

function readBasePath(): string {
  const value = document.querySelector<HTMLMetaElement>('meta[name="osb-base-path"]')?.content ?? "/";
  if (!value.startsWith("/") || value.startsWith("//") || value.includes("\\")) return "/";
  const normalized = value.replace(/\/+$/, "") || "/";
  return normalized.split("/").some((segment) => segment === "." || segment === "..")
    ? "/"
    : normalized;
}

export function usePageTitle(title: string) {
  useEffect(() => {
    document.title = `${title} · OpenSoverignBlog`;
  }, [title]);
}

export function formatDate(value: string | undefined): string {
  if (!value) return "날짜 없음";
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return value;
  return new Intl.DateTimeFormat("ko-KR", {
    year: "numeric",
    month: "short",
    day: "numeric",
  }).format(parsed);
}

export function slugify(value: string): string {
  return value
    .trim()
    .toLocaleLowerCase()
    .replace(/[^\p{Letter}\p{Number}]+/gu, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 120);
}

export function initials(value: string): string {
  const parts = value.trim().split(/\s+/).filter(Boolean);
  return (parts.length > 1 ? `${parts[0]?.[0] ?? ""}${parts[1]?.[0] ?? ""}` : value.slice(0, 2))
    .toLocaleUpperCase();
}
