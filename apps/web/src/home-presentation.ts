import type {
  FeedPostSummary,
  HomeResponse,
  HomeSeriesSection,
  HomeUnit,
} from "@opensoverignblog/sdk";

export type PresentedHomeUnit =
  | {
      kind: "post";
      post: FeedPostSummary;
    }
  | {
      kind: "series";
      series: HomeSeriesSection["series"];
      items: FeedPostSummary[];
      anchorId: string;
    };

export interface HomePresentation {
  units: PresentedHomeUnit[];
}

export function homeSeriesAnchor(seriesSlug: string): string {
  return `home-series-${seriesSlug}`;
}

/**
 * Prefer the server's authoritative typed home units. During a rolling upgrade
 * an older server may omit `units`; in that case, project its pinned/series/
 * category/recent compatibility fields into the same flat unit model.
 *
 * Legacy category sections are deliberately flattened into standalone posts.
 * A non-pinned post backed by a first-class Series promotes that whole Series.
 * Legacy servers remove pinned posts from their Series projection, so their
 * document-only pins must stay standalone or the pinned article can disappear.
 */
export function presentHome(home: HomeResponse): HomePresentation {
  return {
    units: home.units
      ? normalizedTypedUnits(home.units)
      : legacyUnits(home),
  };
}

function normalizedTypedUnits(units: HomeUnit[]): PresentedHomeUnit[] {
  const seenPosts = new Set<string>();
  const seenSeries = new Set<string>();
  const presented: PresentedHomeUnit[] = [];

  for (const unit of units) {
    if (unit.kind === "post") {
      if (seenPosts.has(unit.post.id)) continue;
      seenPosts.add(unit.post.id);
      presented.push(unit);
      continue;
    }
    if (seenSeries.has(unit.series.id)) continue;
    const items = unit.items.filter((post) => {
      if (seenPosts.has(post.id)) return false;
      seenPosts.add(post.id);
      return true;
    });
    if (!items.length) continue;
    seenSeries.add(unit.series.id);
    presented.push({
      ...unit,
      items,
      anchorId: homeSeriesAnchor(unit.series.slug),
    });
  }
  return presented;
}

function legacyUnits(home: HomeResponse): PresentedHomeUnit[] {
  const seriesSections = home.seriesSections ?? [];
  const seriesByCategoryId = new Map(
    seriesSections.map((section) => [section.series.categoryId, section]),
  );
  const seenPosts = new Set<string>();
  const seenSeries = new Set<string>();
  const presented: PresentedHomeUnit[] = [];

  const appendSeries = (section: HomeSeriesSection) => {
    if (seenSeries.has(section.series.id)) return;
    const items = section.items.filter((post) => {
      if (seenPosts.has(post.id)) return false;
      seenPosts.add(post.id);
      return true;
    });
    if (!items.length) return;
    seenSeries.add(section.series.id);
    presented.push({
      kind: "series",
      series: section.series,
      items,
      anchorId: homeSeriesAnchor(section.series.slug),
    });
  };

  const appendPost = (post: FeedPostSummary) => {
    const series = post.category
      ? seriesByCategoryId.get(post.category.id)
      : undefined;
    if (series) {
      appendSeries(series);
      return;
    }
    if (seenPosts.has(post.id)) return;
    seenPosts.add(post.id);
    presented.push({ kind: "post", post });
  };

  const appendLegacyPin = (post: FeedPostSummary) => {
    if (seenPosts.has(post.id)) return;
    seenPosts.add(post.id);
    presented.push({ kind: "post", post });
  };

  home.pinnedItems.forEach(appendLegacyPin);
  seriesSections.forEach(appendSeries);
  (home.categorySections ?? []).forEach((section) => {
    section.items.forEach(appendPost);
  });
  home.recentItems.forEach(appendPost);
  return presented;
}
