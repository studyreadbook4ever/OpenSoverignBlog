import type { FeedPostSummary, ViewMode } from "@opensoverignblog/sdk";

export function articleViewFromSearch(search: string): ViewMode {
  const value = new URLSearchParams(search).get("view");
  if (value === "markdown" || value === "markdown_source" || value === "source") {
    return value === "source" ? "markdown_source" : value;
  }
  return "intent";
}

export function publicArticlePath({
  handle,
  slug,
  categorySlug,
  primary = false,
}: {
  handle: string;
  slug: string;
  categorySlug?: string;
  primary?: boolean;
}): string {
  const encodedSlug = encodeURIComponent(slug);
  const encodedCategory = categorySlug ? encodeURIComponent(categorySlug) : undefined;
  if (primary && encodedCategory) return `/${encodedCategory}/${encodedSlug}`;
  const blogRoot = `/@${encodeURIComponent(handle)}`;
  return encodedCategory
    ? `${blogRoot}/${encodedCategory}/${encodedSlug}`
    : `${blogRoot}/${encodedSlug}`;
}

export function publicFeedPostPath(post: FeedPostSummary): string {
  return publicArticlePath({
    handle: post.blog.handle,
    slug: post.slug,
    ...(post.category ? { categorySlug: post.category.slug } : {}),
    primary: post.blog.isPrimary,
  });
}

export function publicCategoryPath({
  handle,
  categorySlug,
  primary,
}: {
  handle: string;
  categorySlug: string;
  primary: boolean;
}): string {
  const category = encodeURIComponent(categorySlug);
  return primary
    ? `/${category}`
    : `/@${encodeURIComponent(handle)}/${category}`;
}

export function publicCategoryPostPath({
  handle,
  categorySlug,
  postSlug,
  primary,
}: {
  handle: string;
  categorySlug: string;
  postSlug: string;
  primary: boolean;
}): string {
  return `${publicCategoryPath({ handle, categorySlug, primary })}/${encodeURIComponent(postSlug)}`;
}

export function articleHref({
  handle,
  slug,
  legacy,
  view,
  categorySlug,
  primary = false,
}: {
  handle: string;
  slug: string;
  legacy: boolean;
  view: ViewMode;
  categorySlug?: string;
  primary?: boolean;
}): string {
  const encodedSlug = encodeURIComponent(slug);
  const pathname = legacy
    ? `/blog/${encodedSlug}`
    : publicArticlePath({
      handle,
      slug,
      ...(categorySlug ? { categorySlug } : {}),
      primary,
    });
  return `${pathname}?view=${encodeURIComponent(view)}`;
}
