import assert from "node:assert/strict";
import test from "node:test";

import { OpenSoverignBlogClient } from "../src/index.ts";

const page = {
  schemaVersion: "open-soverign-blog-admin-tree/1",
  generatedAt: "2026-07-22T00:00:00Z",
  parentId: "group:content",
  items: [
    {
      id: "site:01900000-0000-7000-8000-000000000001",
      parentId: "group:content",
      kind: "site",
      label: "My blog",
      hasChildren: true,
      entityId: "01900000-0000-7000-8000-000000000001",
      handle: "owner",
      createdAt: "2026-07-22T00:00:00Z",
      sourceMarkdown: "must not enter the SDK projection",
      apiKey: "must not enter the SDK projection",
    },
  ],
  nextCursor: "opaque-next",
};

test("administrator tree query is encoded and response nodes are safely projected", async () => {
  let captured;
  const client = new OpenSoverignBlogClient({
    baseUrl: "/team",
    fetch: async (input, init) => {
      captured = { input, init };
      return new Response(JSON.stringify(page), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    },
  });

  const result = await client.adminTree({
    parent: "group:content",
    cursor: "opaque+/=cursor",
    limit: 100,
  });

  assert.equal(
    captured.input,
    "/team/api/v1/admin/tree?parent=group%3Acontent&cursor=opaque%2B%2F%3Dcursor&limit=100",
  );
  assert.equal(captured.init.credentials, "same-origin");
  const headers = new Headers(captured.init.headers);
  assert.equal(headers.get("Cache-Control"), "no-store");
  assert.equal(headers.get("Pragma"), "no-cache");
  assert.deepEqual(result, {
    schemaVersion: page.schemaVersion,
    generatedAt: page.generatedAt,
    parentId: page.parentId,
    items: [
      {
        id: page.items[0].id,
        parentId: page.items[0].parentId,
        kind: "site",
        label: "My blog",
        hasChildren: true,
        entityId: page.items[0].entityId,
        handle: "owner",
        createdAt: page.items[0].createdAt,
      },
    ],
    nextCursor: "opaque-next",
  });
  assert.equal(Object.hasOwn(result.items[0], "sourceMarkdown"), false);
  assert.equal(Object.hasOwn(result.items[0], "apiKey"), false);
});

test("administrator tree fails closed on an unknown node kind", async () => {
  const client = new OpenSoverignBlogClient({
    fetch: async () => new Response(JSON.stringify({
      ...page,
      items: [{
        id: "secret:1",
        parentId: "group:content",
        kind: "unrestricted_metadata",
        label: "Unsafe extension",
        hasChildren: false,
      }],
    }), { status: 200, headers: { "Content-Type": "application/json" } }),
  });

  await assert.rejects(
    client.adminTree({ parent: "group:content" }),
    /node kind is unsupported/,
  );
});

test("administrator tree rejects an invalid limit before fetching", async () => {
  let fetches = 0;
  const client = new OpenSoverignBlogClient({
    fetch: async () => {
      fetches += 1;
      throw new Error("fetch must not run");
    },
  });

  await assert.rejects(client.adminTree({ limit: 201 }), TypeError);
  assert.equal(fetches, 0);
});
