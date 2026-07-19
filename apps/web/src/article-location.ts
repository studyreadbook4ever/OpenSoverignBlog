import type { ViewMode } from "@opensoverignblog/sdk";

export function articleViewFromSearch(search: string): ViewMode {
  const value = new URLSearchParams(search).get("view");
  if (value === "markdown" || value === "markdown_source" || value === "source") {
    return value === "source" ? "markdown_source" : value;
  }
  return "intent";
}

export function articleHref({
  handle,
  slug,
  legacy,
  view,
}: {
  handle: string;
  slug: string;
  legacy: boolean;
  view: ViewMode;
}): string {
  const pathname = legacy
    ? `/blog/${encodeURIComponent(slug)}`
    : `/@${encodeURIComponent(handle)}/${encodeURIComponent(slug)}`;
  return `${pathname}?view=${encodeURIComponent(view)}`;
}
