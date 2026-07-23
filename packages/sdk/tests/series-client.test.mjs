import assert from "node:assert/strict";
import test from "node:test";

import { OpenSoverignBlogClient } from "../src/index.ts";

const series = {
  id: "01900000-0000-7000-8000-000000000050",
  categoryId: "01900000-0000-7000-8000-000000000051",
  slug: "yangja",
  title: "양자",
  status: "active",
  homePosition: 1,
  createdAt: "2026-07-24T00:00:00Z",
  updatedAt: "2026-07-24T00:00:00Z",
};

function jsonResponse(value) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}

test("public Series methods encode blog and series path segments", async () => {
  const urls = [];
  const client = new OpenSoverignBlogClient({
    baseUrl: "/team",
    fetch: async (input) => {
      urls.push(input);
      if (String(input).endsWith("/series")) return jsonResponse({ items: [series] });
      if (String(input).endsWith("/posts")) return jsonResponse({ items: [] });
      return jsonResponse({ series, blog: { id: "blog" }, postCount: 0 });
    },
  });

  await client.listBlogSeries("owner/name");
  await client.getBlogSeries("owner/name", "quantum/notes");
  await client.getBlogSeriesPosts("owner/name", "quantum/notes");

  assert.deepEqual(urls, [
    "/team/api/v1/blogs/owner%2Fname/series",
    "/team/api/v1/blogs/owner%2Fname/series/quantum%2Fnotes",
    "/team/api/v1/blogs/owner%2Fname/series/quantum%2Fnotes/posts",
  ]);
});

test("primary Series methods use handle-free on-premises aliases", async () => {
  const urls = [];
  const client = new OpenSoverignBlogClient({
    baseUrl: "/owned",
    fetch: async (input) => {
      urls.push(input);
      if (String(input).endsWith("/series")) return jsonResponse({ items: [series] });
      if (String(input).endsWith("/posts")) return jsonResponse({ items: [] });
      return jsonResponse({ series, blog: { id: "primary" }, postCount: 0 });
    },
  });

  await client.listPrimarySeries();
  await client.getPrimarySeries("quantum/notes");
  await client.getPrimarySeriesPosts("quantum/notes");

  assert.deepEqual(urls, [
    "/owned/api/v1/primary/series",
    "/owned/api/v1/primary/series/quantum%2Fnotes",
    "/owned/api/v1/primary/series/quantum%2Fnotes/posts",
  ]);
});

test("Studio Series methods preserve null clearing and exact ordered ids", async () => {
  const requests = [];
  const client = new OpenSoverignBlogClient({
    fetch: async (input, init = {}) => {
      requests.push({ input, init });
      if (String(input).endsWith("/items")) return jsonResponse([]);
      if (String(input).endsWith("/series")) return jsonResponse({ items: [series] });
      return jsonResponse(series);
    },
  });

  await client.listStudioSeries();
  await client.createStudioSeries({
    slug: "yangja",
    title: "양자",
    description: null,
    themePreset: null,
  });
  await client.promoteStudioCategoryToSeries("category/id");
  await client.updateStudioSeries("series/id", {
    title: "새 이름",
    description: null,
    themePreset: null,
  });
  await client.archiveStudioSeries("series/id");
  await client.listStudioSeriesItems("series/id");
  await client.replaceStudioSeriesOrder("series/id", ["document/one", "document/two"]);

  assert.deepEqual(
    requests.map(({ input, init }) => [input, init.method ?? "GET"]),
    [
      ["/api/v1/studio/series", "GET"],
      ["/api/v1/studio/series", "POST"],
      ["/api/v1/studio/series/promote", "POST"],
      ["/api/v1/studio/series/series%2Fid", "PUT"],
      ["/api/v1/studio/series/series%2Fid/archive", "POST"],
      ["/api/v1/studio/series/series%2Fid/items", "GET"],
      ["/api/v1/studio/series/series%2Fid/items", "PUT"],
    ],
  );
  assert.deepEqual(JSON.parse(requests[1].init.body), {
    slug: "yangja",
    title: "양자",
    description: null,
    themePreset: null,
  });
  assert.deepEqual(JSON.parse(requests[2].init.body), { categoryId: "category/id" });
  assert.deepEqual(JSON.parse(requests[3].init.body), {
    title: "새 이름",
    description: null,
    themePreset: null,
  });
  assert.deepEqual(JSON.parse(requests[6].init.body), {
    documentIds: ["document/one", "document/two"],
  });
  assert.equal(requests.every(({ init }) => init.credentials === "same-origin"), true);
});
