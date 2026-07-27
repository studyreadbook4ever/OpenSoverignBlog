import assert from "node:assert/strict";
import test from "node:test";

import { OpenSoverignBlogClient } from "../src/index.ts";

test("home returns compatible flat feeds alongside ordered category sections", async () => {
  const response = {
    pinnedItems: [],
    recentItems: [],
    categorySections: [
      {
        category: {
          id: "01900000-0000-7000-8000-000000000050",
          slug: "yangja",
          title: "yangja",
          description: "양자 컴퓨팅",
          status: "active",
        },
        items: [],
      },
    ],
  };
  const requests = [];
  const client = new OpenSoverignBlogClient({
    baseUrl: "/owned",
    fetch: async (input, init) => {
      requests.push({ input, init });
      return new Response(JSON.stringify(response), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    },
  });

  assert.deepEqual(await client.home(), response);
  assert.equal(requests[0].input, "/owned/api/v1/home");
  assert.equal(requests[0].init.method, undefined);
});

test("typed home pins preserve one combined post and series order", async () => {
  const targets = [
    { kind: "series", id: "01900000-0000-7000-8000-000000000051" },
    { kind: "post", id: "01900000-0000-7000-8000-000000000052" },
  ];
  const response = {
    targets,
    documentIds: ["01900000-0000-7000-8000-000000000052"],
  };
  const requests = [];
  const client = new OpenSoverignBlogClient({
    baseUrl: "/owned",
    fetch: async (input, init) => {
      requests.push({ input, init });
      return new Response(JSON.stringify(response), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    },
  });

  assert.deepEqual(await client.replaceHomePinTargets(targets), response);
  assert.equal(requests[0].input, "/owned/api/v1/admin/home/pins");
  assert.equal(requests[0].init.method, "PUT");
  assert.deepEqual(JSON.parse(requests[0].init.body), { targets });
});
