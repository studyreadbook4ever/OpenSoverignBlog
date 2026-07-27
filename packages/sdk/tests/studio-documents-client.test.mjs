import assert from "node:assert/strict";
import test from "node:test";

import { OpenSoverignBlogClient } from "../src/index.ts";

const baseSnapshot = {
  schemaVersion: "1.0",
  id: "01900000-0000-7000-8000-000000000070",
  siteId: "01900000-0000-7000-8000-000000000071",
  status: "published",
  currentRevisionId: "01900000-0000-7000-8000-000000000072",
  publishedRevisionId: "01900000-0000-7000-8000-000000000072",
  revision: {
    schemaVersion: "1.0",
    id: "01900000-0000-7000-8000-000000000072",
    documentId: "01900000-0000-7000-8000-000000000070",
    revisionNumber: 1,
    title: "Published note",
    slug: "published-note",
    sourceMarkdown: "# Published note",
    embeds: [],
    authorship: { kind: "human", humanReviewed: false },
    actor: { kind: "human", id: "owner" },
    contentHash: `sha256:${"0".repeat(64)}`,
    createdAt: "2026-07-24T00:00:00Z",
  },
  createdAt: "2026-07-24T00:00:00Z",
  updatedAt: "2026-07-24T00:00:00Z",
};

function jsonResponse(value) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}

test("Studio document reads preserve explicit published placement null", async () => {
  const requests = [];
  const snapshot = { ...baseSnapshot, publishedCategoryId: null };
  const client = new OpenSoverignBlogClient({
    fetch: async (input) => {
      requests.push(input);
      return jsonResponse(snapshot);
    },
  });

  const response = await client.getStudioDocument("document/id");

  assert.equal(response.publishedCategoryId, null);
  assert.equal(
    Object.hasOwn(response, "publishedCategoryId"),
    true,
  );
  assert.deepEqual(requests, ["/api/v1/studio/documents/document%2Fid"]);
});

test("Studio document reads remain compatible with field-absent older servers", async () => {
  const client = new OpenSoverignBlogClient({
    fetch: async () => jsonResponse(baseSnapshot),
  });

  const response = await client.getStudioDocument(baseSnapshot.id);

  assert.equal(Object.hasOwn(response, "publishedCategoryId"), false);
});
