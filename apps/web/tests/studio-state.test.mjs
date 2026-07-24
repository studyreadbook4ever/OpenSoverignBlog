import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";

import {
  acceptedEditorState,
  aiSummarySourceHash,
  editorFingerprint,
  homeCurationCandidates,
  homeCurationRows,
  homePinTargetKey,
  homePinTargets,
  isAiSummarySourceCurrent,
  normalizeSavePayload,
  normalizedEditorTitle,
  payloadFingerprint,
  revisionSavePayload,
  reviewAiSummaryCandidate,
} from "../src/studio-state.ts";

test("Studio uses one normalized title for preview, AI binding, and saves", () => {
  assert.equal(normalizedEditorTitle("  제목의 의미  "), "제목의 의미");
  assert.equal(normalizedEditorTitle(" \n\t "), "");
});

test("an accepted AI revision immediately matches the normalized editor draft", () => {
  const payload = normalizeSavePayload({
    title: " AI draft ",
    slug: "ai-draft",
    sourceMarkdown: "body",
    embeds: [{
      id: "source",
      provider: "first-party",
      resourceId: "asset",
      canonicalUrl: "/media/asset",
      title: "asset",
      consentPurposeIds: [],
    }],
    ontology: { schema: "test", statements: [] },
    authorship: {
      kind: "ai_assisted",
      generator: "local/model-v1",
      humanReviewed: false,
    },
  });
  const accepted = acceptedEditorState(payload);

  assert.equal(accepted.draft.authorship.humanReviewed, true);
  assert.equal(
    editorFingerprint(
      accepted.draft,
      JSON.stringify(payload.embeds),
      JSON.stringify(payload.ontology),
    ),
    accepted.fingerprint,
  );
});

test("home curation uses published placement instead of the current draft category", () => {
  const activeSeries = {
    id: "ordered-series",
    categoryId: "series-category",
    status: "active",
    title: "시리즈",
    slug: "series",
  };
  const archivedSeries = {
    id: "archived-series",
    categoryId: "archived-category",
    status: "archived",
    title: "예전 시리즈",
    slug: "archived-series",
  };
  const publicStandalone = {
    id: "standalone",
    title: "일반 글",
    slug: "standalone",
    blog: { handle: "owner", isPrimary: false },
  };
  const publishedMemberWithDraftMove = {
    id: "published-member",
    categoryId: undefined,
    publishedCategoryId: activeSeries.categoryId,
    status: "published",
    publishedRevisionId: "member-revision",
    revision: { title: "현재 초안은 독립 글", slug: "published-member" },
  };
  const publishedStandaloneWithDraftMove = {
    id: "published-standalone",
    categoryId: activeSeries.categoryId,
    publishedCategoryId: null,
    status: "published",
    publishedRevisionId: "standalone-revision",
    revision: { title: "발행본은 독립 글", slug: "published-standalone" },
  };
  const legacySeriesMember = {
    id: "legacy-member",
    categoryId: activeSeries.categoryId,
    status: "published",
    publishedRevisionId: "legacy-revision",
    revision: { title: "구형 응답 시리즈 글", slug: "legacy-member" },
  };
  const archivedCategoryDocument = {
    id: "archived-category-post",
    categoryId: archivedSeries.categoryId,
    publishedCategoryId: archivedSeries.categoryId,
    status: "published",
    publishedRevisionId: "archived-category-revision",
    revision: { title: "독립 일반 글", slug: "independent" },
  };
  const draft = {
    id: "draft",
    status: "draft",
    revision: { title: "초안", slug: "draft" },
  };
  assert.deepEqual(
    homeCurationCandidates({
      units: [{ kind: "post", post: publicStandalone }],
      pinnedItems: [],
      recentItems: [],
    }, {
      studioDocuments: [
        publishedMemberWithDraftMove,
        publishedStandaloneWithDraftMove,
        legacySeriesMember,
        archivedCategoryDocument,
        draft,
      ],
      studioSeries: [activeSeries, archivedSeries],
    }).map((candidate) => [
      candidate.kind,
      candidate.id,
      candidate.locationLabel,
    ]),
    [
      ["post", "standalone", "일반 글 · /@owner/standalone"],
      ["series", "ordered-series", "시리즈 · /series"],
      ["post", "published-standalone", "발행된 일반 글"],
      ["post", "archived-category-post", "발행된 일반 글"],
    ],
  );
});

test("settings retains stale selected targets so they can always be moved or unpinned", () => {
  const available = {
    kind: "post",
    id: "available",
    target: { kind: "post", id: "available" },
    title: "Available post",
    slug: "available",
    locationLabel: "Post · /@owner/available",
  };
  const unselected = {
    kind: "series",
    id: "series",
    target: { kind: "series", id: "series" },
    title: "Series",
    slug: "series",
    locationLabel: "Series · /series",
  };
  const rows = homeCurationRows(
    [available, unselected],
    [{ kind: "series", id: "archived" }, available.target],
    "en",
  );

  assert.deepEqual(rows.map(({ kind, id }) => [kind, id]), [
    ["series", "archived"],
    ["post", "available"],
    ["series", "series"],
  ]);
  assert.equal(rows[0].title, "Series archived");
  assert.equal(rows[0].locationLabel, "Pinned · unavailable in the current publication list");
});

test("typed home pins retain kind/order with a document-only rolling-upgrade fallback", () => {
  assert.deepEqual(
    homePinTargets({
      targets: [
        { kind: "series", id: "series" },
        { kind: "post", id: "post" },
      ],
      documentIds: ["legacy"],
    }),
    [
      { kind: "series", id: "series" },
      { kind: "post", id: "post" },
    ],
  );
  assert.deepEqual(
    homePinTargets({ documentIds: ["legacy", "legacy", "second"] }),
    [
      { kind: "post", id: "legacy" },
      { kind: "post", id: "second" },
    ],
  );
  assert.equal(homePinTargetKey({ kind: "series", id: "same" }), "series:same");
  assert.equal(homePinTargetKey({ kind: "post", id: "same" }), "post:same");
});

test("AI summary source hashing exactly mirrors the domain-separated server hash", async () => {
  const title = " 요약할 제목 ";
  const markdown = "# 본문\n\n내용";
  const expected = createHash("sha256")
    .update("osb-ai-summary-source/1")
    .update(Buffer.from([0]))
    .update(title)
    .update(Buffer.from([0]))
    .update(markdown)
    .digest("hex");

  assert.equal(await aiSummarySourceHash(title, markdown), `sha256:${expected}`);
});

test("reviewing an AI candidate is explicit, immutable, and part of the editor fingerprint", () => {
  const candidate = {
    text: "요약 초안",
    sourceHash: "sha256:source",
    provenance: {
      provider: "openai",
      model: "test-model",
      promptVersion: "osb-summary/1",
      generatedAt: "2026-07-22T00:00:00.000Z",
      humanReviewed: false,
    },
  };
  const reviewed = reviewAiSummaryCandidate(candidate);
  const post = { title: "제목", slug: "post", sourceMarkdown: "본문" };

  assert.equal(candidate.provenance.humanReviewed, false);
  assert.equal(reviewed.provenance.humanReviewed, true);
  assert.notEqual(payloadFingerprint(post), payloadFingerprint({ ...post, aiSummary: reviewed }));
  assert.equal(isAiSummarySourceCurrent(reviewed, "sha256:source"), true);
  assert.equal(isAiSummarySourceCurrent(reviewed, "sha256:changed"), false);
  assert.equal(isAiSummarySourceCurrent(reviewed, undefined), false);
});

test("revision saves inherit an unchanged category but preserve explicit moves", () => {
  const base = {
    title: "Archived category draft",
    slug: "archived-category-draft",
    sourceMarkdown: "Still editable",
    categoryId: "archived-category-id",
  };

  assert.equal(
    Object.hasOwn(revisionSavePayload(base, "archived-category-id"), "categoryId"),
    false,
  );
  assert.deepEqual(
    revisionSavePayload({ ...base, categoryId: null }, "archived-category-id"),
    { ...base, categoryId: null },
  );
  assert.deepEqual(
    revisionSavePayload({ ...base, categoryId: "active-category-id" }, "archived-category-id"),
    { ...base, categoryId: "active-category-id" },
  );
  assert.equal(
    Object.hasOwn(revisionSavePayload({ ...base, categoryId: null }, undefined), "categoryId"),
    false,
  );
  const { categoryId: _categoryId, ...restoredWithoutCategoryState } = base;
  assert.equal(
    Object.hasOwn(
      revisionSavePayload(restoredWithoutCategoryState, "archived-category-id"),
      "categoryId",
    ),
    false,
  );
});
