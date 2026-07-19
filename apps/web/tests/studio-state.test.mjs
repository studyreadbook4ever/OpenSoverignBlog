import assert from "node:assert/strict";
import test from "node:test";

import {
  acceptedEditorState,
  editorFingerprint,
  homeCurationCandidates,
  normalizeSavePayload,
} from "../src/studio-state.ts";

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
