import type {
  FeedPostSummary,
  HomeCategorySection,
  HomeResponse,
  HomeSeriesSection,
} from "@opensoverignblog/sdk";

export interface PresentedHomeCategorySection extends HomeCategorySection {
  anchorId: string;
}

export interface PresentedHomeSeriesSection extends HomeSeriesSection {
  anchorId: string;
}

export interface HomePresentation {
  categorySections: PresentedHomeCategorySection[];
  seriesSections: PresentedHomeSeriesSection[];
  recentItems: FeedPostSummary[];
  total: number;
}

export function homeCategoryAnchor(categorySlug: string): string {
  return `home-category-${categorySlug}`;
}

export function homeSeriesAnchor(seriesSlug: string): string {
  return `home-series-${seriesSlug}`;
}

/**
 * Turns the backwards-compatible home payload into the rows that are actually
 * rendered. The API owns category and post order; the browser only removes
 * duplicate document ids as it walks pinned, series, category, then recent
 * content. Series wins over its backing category during rolling upgrades.
 */
export function presentHome(home: HomeResponse): HomePresentation {
  const displayedIds = new Set(home.pinnedItems.map((item) => item.id));
  const seriesSections = (home.seriesSections ?? []).flatMap((section) => {
    const items = section.items.filter((item) => {
      if (displayedIds.has(item.id)) return false;
      displayedIds.add(item.id);
      return true;
    });
    if (!items.length) return [];
    return [{
      ...section,
      anchorId: homeSeriesAnchor(section.series.slug),
      items,
    }];
  });
  const categorySections = (home.categorySections ?? []).flatMap((section) => {
    const items = section.items.filter((item) => {
      if (displayedIds.has(item.id)) return false;
      displayedIds.add(item.id);
      return true;
    });
    if (!items.length) return [];
    return [{
      ...section,
      anchorId: homeCategoryAnchor(section.category.slug),
      items,
    }];
  });
  const recentItems = home.recentItems.filter((item) => {
    if (displayedIds.has(item.id)) return false;
    displayedIds.add(item.id);
    return true;
  });

  return {
    categorySections,
    seriesSections,
    recentItems,
    total: displayedIds.size,
  };
}
