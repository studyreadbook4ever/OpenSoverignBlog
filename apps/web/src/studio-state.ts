import type {
  AiSummary,
  CreatePostInput,
  DocumentSnapshot,
  FeedPostSummary,
  HomePinsResponse,
  HomePinTarget,
  HomeResponse,
  SeriesSummary,
} from "@opensoverignblog/sdk";

const AI_SUMMARY_SOURCE_HASH_VERSION = "osb-ai-summary-source/1";

/** The exact title Studio sends to preview, AI generation, and persistence. */
export function normalizedEditorTitle(title: string): string {
  return title.trim();
}

export function normalizeSavePayload(post: CreatePostInput): CreatePostInput {
  if (!post.authorship || post.authorship.kind === "human" || post.authorship.humanReviewed) {
    return post;
  }
  return {
    ...post,
    authorship: { ...post.authorship, humanReviewed: true },
  };
}

/**
 * Revision category placement is an optional patch: omission inherits the
 * loaded revision, while null and an ID explicitly move the new revision.
 * Keep the editor's full selection in state and minimize only the wire body.
 */
export function revisionSavePayload(
  post: CreatePostInput,
  inheritedCategoryId: string | undefined,
): CreatePostInput {
  const normalized = normalizeSavePayload(post);
  if (!Object.hasOwn(normalized, "categoryId")) return normalized;
  const selectedCategoryId = normalized.categoryId ?? null;
  const inherited = inheritedCategoryId ?? null;
  if (selectedCategoryId !== inherited) {
    return { ...normalized, categoryId: selectedCategoryId };
  }
  const payload = { ...normalized };
  delete payload.categoryId;
  return payload;
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
    aiSummary: post.aiSummary ?? null,
    categoryId: post.categoryId ?? null,
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
      aiSummary: post.aiSummary ?? null,
      categoryId: post.categoryId ?? null,
    });
  } catch {
    return `invalid-sidecar:${JSON.stringify({
      title: post.title,
      slug: post.slug,
      sourceMarkdown: post.sourceMarkdown,
      embedText,
      ontologyText,
      intent: post.intent ?? null,
      authorship: post.authorship ?? null,
      aiSummary: post.aiSummary ?? null,
      categoryId: post.categoryId ?? null,
    })}`;
  }
}

/**
 * Mirrors the server's domain-separated source hash. It lets Studio fail
 * closed when a reviewed summary no longer belongs to the current title and
 * Markdown; the server remains the final validation boundary.
 */
export async function aiSummarySourceHash(title: string, sourceMarkdown: string): Promise<string> {
  const encoder = new TextEncoder();
  const source = encoder.encode(`${AI_SUMMARY_SOURCE_HASH_VERSION}\0${title}\0${sourceMarkdown}`);
  const subtle = globalThis.crypto?.subtle;
  if (subtle) {
    const digest = await subtle.digest("SHA-256", source);
    return `sha256:${bytesToHex(new Uint8Array(digest))}`;
  }
  // Web Crypto is unavailable on some plain-HTTP on-premise origins. This
  // deterministic fallback keeps freshness checks working there; it is not
  // used for credentials or signatures.
  return `sha256:${sha256Fallback(source)}`;
}

const SHA256_INITIAL = [
  0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
  0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const SHA256_ROUND = [
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

function sha256Fallback(input: Uint8Array): string {
  const paddedLength = Math.ceil((input.length + 9) / 64) * 64;
  const padded = new Uint8Array(paddedLength);
  padded.set(input);
  padded[input.length] = 0x80;
  const bitLength = BigInt(input.length) * 8n;
  for (let index = 0; index < 8; index += 1) {
    padded[padded.length - 1 - index] = Number((bitLength >> BigInt(index * 8)) & 0xffn);
  }

  const state = [...SHA256_INITIAL];
  const schedule = new Uint32Array(64);
  const view = new DataView(padded.buffer);
  for (let offset = 0; offset < padded.length; offset += 64) {
    for (let index = 0; index < 16; index += 1) schedule[index] = view.getUint32(offset + index * 4);
    for (let index = 16; index < 64; index += 1) {
      const left = schedule[index - 15]!;
      const right = schedule[index - 2]!;
      const sigma0 = rotateRight(left, 7) ^ rotateRight(left, 18) ^ (left >>> 3);
      const sigma1 = rotateRight(right, 17) ^ rotateRight(right, 19) ^ (right >>> 10);
      schedule[index] = (schedule[index - 16]! + sigma0 + schedule[index - 7]! + sigma1) >>> 0;
    }
    let [a, b, c, d, e, f, g, h] = state;
    for (let index = 0; index < 64; index += 1) {
      const sum1 = rotateRight(e!, 6) ^ rotateRight(e!, 11) ^ rotateRight(e!, 25);
      const choice = (e! & f!) ^ (~e! & g!);
      const temp1 = (h! + sum1 + choice + SHA256_ROUND[index]! + schedule[index]!) >>> 0;
      const sum0 = rotateRight(a!, 2) ^ rotateRight(a!, 13) ^ rotateRight(a!, 22);
      const majority = (a! & b!) ^ (a! & c!) ^ (b! & c!);
      const temp2 = (sum0 + majority) >>> 0;
      h = g;
      g = f;
      f = e;
      e = (d! + temp1) >>> 0;
      d = c;
      c = b;
      b = a;
      a = (temp1 + temp2) >>> 0;
    }
    state[0] = (state[0]! + a!) >>> 0;
    state[1] = (state[1]! + b!) >>> 0;
    state[2] = (state[2]! + c!) >>> 0;
    state[3] = (state[3]! + d!) >>> 0;
    state[4] = (state[4]! + e!) >>> 0;
    state[5] = (state[5]! + f!) >>> 0;
    state[6] = (state[6]! + g!) >>> 0;
    state[7] = (state[7]! + h!) >>> 0;
  }
  return state.map((word) => word.toString(16).padStart(8, "0")).join("");
}

function rotateRight(value: number, amount: number): number {
  return (value >>> amount) | (value << (32 - amount));
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function isAiSummarySourceCurrent(
  summary: AiSummary | undefined,
  currentSourceHash: string | null | undefined,
): boolean {
  return Boolean(summary && currentSourceHash && summary.sourceHash === currentSourceHash);
}

/** Human review is an explicit action; never mutate the provider candidate. */
export function reviewAiSummaryCandidate(candidate: AiSummary): AiSummary {
  return {
    ...candidate,
    provenance: {
      ...candidate.provenance,
      humanReviewed: true,
    },
  };
}

export interface HomeCurationCandidate {
  kind: HomePinTarget["kind"];
  id: string;
  target: HomePinTarget;
  title: string;
  slug: string;
  locationLabel: string;
}

export interface HomeCurationOptions {
  studioDocuments?: DocumentSnapshot[];
  studioSeries?: SeriesSummary[];
  language?: "ko" | "en";
}

export interface PublishedSeriesMembership {
  documentIds: ReadonlySet<string>;
  seriesIdsWithPublishedItems: ReadonlySet<string>;
}

export function publishedSeriesMembership(
  studioDocuments: DocumentSnapshot[],
  studioSeries: SeriesSummary[],
): PublishedSeriesMembership {
  const activeSeriesByCategoryId = new Map(
    studioSeries
      .filter((series) => series.status === "active")
      .map((series) => [series.categoryId, series]),
  );
  const documentIds = new Set<string>();
  const seriesIdsWithPublishedItems = new Set<string>();
  for (const document of studioDocuments) {
    if (!document.publishedRevisionId || document.status === "archived") continue;
    const categoryId = publishedDocumentCategoryId(document);
    const series = categoryId ? activeSeriesByCategoryId.get(categoryId) : undefined;
    if (!series) continue;
    documentIds.add(document.id);
    seriesIdsWithPublishedItems.add(series.id);
  }
  return { documentIds, seriesIdsWithPublishedItems };
}

export function homePinTargetKey(target: HomePinTarget): string {
  return `${target.kind}:${target.id}`;
}

export function homePinTargets(response: HomePinsResponse): HomePinTarget[] {
  const targets = response.targets
    ?? response.documentIds.map((id) => ({ kind: "post" as const, id }));
  const seen = new Set<string>();
  return targets.filter((target) => {
    const key = homePinTargetKey(target);
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  }).slice(0, 3);
}

export function homeCurationRows(
  candidates: HomeCurationCandidate[],
  selectedTargets: HomePinTarget[],
  language: "ko" | "en" = "ko",
): HomeCurationCandidate[] {
  const candidateByKey = new Map(
    candidates.map((candidate) => [homePinTargetKey(candidate.target), candidate]),
  );
  const selectedKeys = new Set(selectedTargets.map(homePinTargetKey));
  const selected = selectedTargets.map((target) => {
    const candidate = candidateByKey.get(homePinTargetKey(target));
    if (candidate) return candidate;
    const kindLabel = target.kind === "series" ? "Series" : "Post";
    return {
      kind: target.kind,
      id: target.id,
      target,
      title: `${kindLabel} ${target.id.slice(0, 8)}`,
      slug: "",
      locationLabel: language === "en"
        ? "Pinned · unavailable in the current publication list"
        : "고정됨 · 현재 발행 후보 목록에 없음",
    };
  });
  return [
    ...selected,
    ...candidates.filter((candidate) => !selectedKeys.has(homePinTargetKey(candidate.target))),
  ];
}

export function homeCurationCandidates(
  home: HomeResponse,
  {
    studioDocuments = [],
    studioSeries = [],
    language = "ko",
  }: HomeCurationOptions = {},
): HomeCurationCandidate[] {
  const seen = new Set<string>();
  const candidates: HomeCurationCandidate[] = [];
  const publicSeries = [
    ...(home.units ?? []).flatMap((unit) => unit.kind === "series" ? [unit.series] : []),
    ...(home.seriesSections ?? []).map((section) => section.series),
  ];
  const documentMembership = publishedSeriesMembership(
    studioDocuments,
    [...publicSeries, ...studioSeries],
  );
  const publishedMemberIds = new Set(documentMembership.documentIds);
  const publishedSeriesIds = new Set(documentMembership.seriesIdsWithPublishedItems);

  for (const unit of home.units ?? []) {
    if (unit.kind !== "series" || unit.series.status !== "active") continue;
    if (unit.items.length) publishedSeriesIds.add(unit.series.id);
    unit.items.forEach((post) => publishedMemberIds.add(post.id));
  }
  for (const section of home.seriesSections ?? []) {
    if (section.series.status !== "active") continue;
    if (section.items.length) publishedSeriesIds.add(section.series.id);
    section.items.forEach((post) => publishedMemberIds.add(post.id));
  }

  const addSeries = (series: SeriesSummary, hasPublishedItems = true) => {
    if (series.status !== "active" || !hasPublishedItems) return;
    const target: HomePinTarget = { kind: "series", id: series.id };
    const key = homePinTargetKey(target);
    if (seen.has(key)) return;
    seen.add(key);
    candidates.push({
      kind: "series",
      id: series.id,
      target,
      title: series.title,
      slug: series.slug,
      locationLabel: language === "en"
        ? `Series · /${series.slug}`
        : `시리즈 · /${series.slug}`,
    });
  };

  const addPost = (post: FeedPostSummary) => {
    if (publishedMemberIds.has(post.id)) return;
    const target: HomePinTarget = { kind: "post", id: post.id };
    const key = homePinTargetKey(target);
    if (seen.has(key)) return;
    seen.add(key);
    candidates.push({
      kind: "post",
      id: post.id,
      target,
      title: post.title,
      slug: post.slug,
      locationLabel: language === "en"
        ? `Post · ${publicPostLocation(post)}`
        : `일반 글 · ${publicPostLocation(post)}`,
    });
  };

  if (home.units) {
    for (const unit of home.units) {
      if (unit.kind === "series") addSeries(unit.series, unit.items.length > 0);
      else addPost(unit.post);
    }
  } else {
    (home.seriesSections ?? []).forEach((section) => {
      addSeries(section.series, section.items.length > 0);
    });
    [
      ...home.pinnedItems,
      ...(home.categorySections ?? []).flatMap((section) => section.items),
      ...home.recentItems,
    ].forEach(addPost);
  }

  studioSeries.forEach((series) => {
    addSeries(series, publishedSeriesIds.has(series.id));
  });
  for (const document of studioDocuments) {
    if (!document.publishedRevisionId || document.status === "archived") continue;
    if (publishedMemberIds.has(document.id)) continue;
    const target: HomePinTarget = { kind: "post", id: document.id };
    const key = homePinTargetKey(target);
    if (seen.has(key)) continue;
    seen.add(key);
    candidates.push({
      kind: "post",
      id: document.id,
      target,
      title: document.revision.title || (language === "en" ? "Untitled post" : "제목 없는 글"),
      slug: document.revision.slug,
      locationLabel: language === "en"
        ? "Published standalone post"
        : "발행된 일반 글",
    });
  }
  return candidates;
}

function publicPostLocation(post: FeedPostSummary): string {
  const slug = encodeURIComponent(post.slug);
  const category = post.category ? encodeURIComponent(post.category.slug) : undefined;
  if (post.blog.isPrimary && category) return `/${category}/${slug}`;
  const blogRoot = `/@${encodeURIComponent(post.blog.handle)}`;
  return category ? `${blogRoot}/${category}/${slug}` : `${blogRoot}/${slug}`;
}

/**
 * New servers explicitly return null for a published standalone revision.
 * Only payloads that omit the field entirely are old enough to require the
 * current-draft category fallback.
 */
function publishedDocumentCategoryId(
  document: DocumentSnapshot,
): string | null | undefined {
  return Object.hasOwn(document, "publishedCategoryId")
    ? document.publishedCategoryId
    : document.categoryId;
}
