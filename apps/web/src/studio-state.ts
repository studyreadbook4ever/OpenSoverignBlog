import type {
  CreatePostInput,
  FeedPostSummary,
  HomeResponse,
} from "@opensoverignblog/sdk";

export function normalizeSavePayload(post: CreatePostInput): CreatePostInput {
  if (!post.authorship || post.authorship.kind === "human" || post.authorship.humanReviewed) {
    return post;
  }
  return {
    ...post,
    authorship: { ...post.authorship, humanReviewed: true },
  };
}

export function acceptedEditorState(payload: CreatePostInput): {
  draft: CreatePostInput;
  fingerprint: string;
} {
  return {
    draft: payload,
    fingerprint: payloadFingerprint(payload),
  };
}

export function payloadFingerprint(post: CreatePostInput): string {
  return JSON.stringify({
    title: post.title.trim(),
    slug: post.slug.trim(),
    sourceMarkdown: post.sourceMarkdown,
    embeds: post.embeds ?? [],
    intent: post.intent ?? null,
    ontology: post.ontology ?? null,
    authorship: post.authorship ?? null,
  });
}

export function editorFingerprint(
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
      authorship: post.authorship ?? null,
    });
  } catch {
    return `invalid-sidecar:${post.title}:${post.slug}:${post.sourceMarkdown}:${embedText}:${ontologyText}`;
  }
}

export function homeCurationCandidates(home: HomeResponse): FeedPostSummary[] {
  const seen = new Set<string>();
  return [...home.pinnedItems, ...home.recentItems].filter((post) => {
    if (seen.has(post.id)) return false;
    seen.add(post.id);
    return true;
  });
}
