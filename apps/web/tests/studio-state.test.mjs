import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";

import {
  acceptedEditorState,
  aiSummarySourceHash,
  editorFingerprint,
  homeCurationCandidates,
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

test("home curation uses de-duplicated published projections with pins first", () => {
  const pinned = { id: "pinned", title: "발행 제목", slug: "published", blog: { handle: "owner" } };
  const recent = { id: "recent", title: "최근 글", slug: "recent", blog: { handle: "member" } };
  assert.deepEqual(
    homeCurationCandidates({ pinnedItems: [pinned], recentItems: [pinned, recent] }).map((post) => post.id),
    ["pinned", "recent"],
  );
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
