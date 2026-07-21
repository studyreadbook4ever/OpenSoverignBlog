import assert from "node:assert/strict";
import test from "node:test";

import { OpenSoverignBlogClient } from "../src/index.ts";

test("references uses the base-path-safe global endpoint", async () => {
  const requests = [];
  const page = {
    label: "레퍼런스",
    sourceMarkdown: "## 출처",
    artifactHtml: "<h2>출처</h2>",
    sourceHash: "sha256:abc",
    rendererVersion: "osb-renderer/0.1.0",
  };
  const client = new OpenSoverignBlogClient({
    baseUrl: "/notes",
    fetch: async (input, init) => {
      requests.push({ input, init });
      return new Response(JSON.stringify(page), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    },
  });

  assert.deepEqual(await client.references(), page);
  assert.equal(requests[0].input, "/notes/api/v1/references");
  assert.equal(requests[0].init.credentials, "same-origin");
});
