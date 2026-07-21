import assert from "node:assert/strict";
import test from "node:test";

import { OpenSoverignBlogClient } from "../src/index.ts";

const category = {
  id: "01900000-0000-7000-8000-000000000040",
  slug: "yangja",
  title: "양자",
  description: "양자역학 기록",
  themePreset: "ink",
  status: "active",
};

function jsonResponse(value) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}

test("public category methods encode every route segment", async () => {
  const urls = [];
  const client = new OpenSoverignBlogClient({
    baseUrl: "/team",
    fetch: async (input) => {
      urls.push(input);
      if (String(input).endsWith("/categories")) return jsonResponse({ items: [category] });
      if (String(input).includes("/posts/post%2Fone")) {
        return jsonResponse({ id: "post", category });
      }
      if (String(input).endsWith("/posts")) return jsonResponse({ items: [] });
      return jsonResponse({ category, blog: { id: "blog" }, postCount: 0 });
    },
  });

  await client.listBlogCategories("owner/name");
  await client.getBlogCategory("owner/name", "quantum/notes");
  await client.getBlogCategoryPosts("owner/name", "quantum/notes");
  await client.getBlogCategoryPost(
    "owner/name",
    "quantum/notes",
    "post/one",
    "markdown_source",
  );

  assert.deepEqual(urls, [
    "/team/api/v1/blogs/owner%2Fname/categories",
    "/team/api/v1/blogs/owner%2Fname/categories/quantum%2Fnotes",
    "/team/api/v1/blogs/owner%2Fname/categories/quantum%2Fnotes/posts",
    "/team/api/v1/blogs/owner%2Fname/categories/quantum%2Fnotes/posts/post%2Fone?view=markdown_source",
  ]);
});

test("primary category methods use handle-free on-premises aliases", async () => {
  const urls = [];
  const client = new OpenSoverignBlogClient({
    baseUrl: "/owned",
    fetch: async (input) => {
      urls.push(input);
      if (String(input).endsWith("/categories")) return jsonResponse({ items: [category] });
      if (String(input).includes("/posts/post%2Fone")) {
        return jsonResponse({ id: "post", category });
      }
      if (String(input).endsWith("/posts")) return jsonResponse({ items: [] });
      return jsonResponse({ category, blog: { id: "primary" }, postCount: 0 });
    },
  });

  await client.listPrimaryCategories();
  await client.getPrimaryCategory("quantum/notes");
  await client.getPrimaryCategoryPosts("quantum/notes");
  await client.getPrimaryCategoryPost("quantum/notes", "post/one", "markdown_source");

  assert.deepEqual(urls, [
    "/owned/api/v1/primary/categories",
    "/owned/api/v1/primary/categories/quantum%2Fnotes",
    "/owned/api/v1/primary/categories/quantum%2Fnotes/posts",
    "/owned/api/v1/primary/categories/quantum%2Fnotes/posts/post%2Fone?view=markdown_source",
  ]);
});

test("blog responses retain the server-owned primary route identity", async () => {
  const primaryBlog = {
    id: "01900000-0000-7000-8000-000000000041",
    handle: "owned",
    title: "Owned",
    owner: {
      id: "01900000-0000-7000-8000-000000000042",
      handle: "owner",
      displayName: "Owner",
    },
    theme: { presetId: "paper" },
    isPrimary: true,
  };
  const client = new OpenSoverignBlogClient({
    fetch: async () => jsonResponse(primaryBlog),
  });

  assert.equal((await client.getBlog("owned")).isPrimary, true);
});

test("Studio category methods use the category mutation contract", async () => {
  const requests = [];
  const client = new OpenSoverignBlogClient({
    fetch: async (input, init) => {
      requests.push({ input, init });
      if (init?.method === "POST" && String(input).endsWith("/archive")) {
        return jsonResponse({ ...category, status: "archived" });
      }
      if (init?.method === "POST") return jsonResponse(category);
      if (init?.method === "PUT") return jsonResponse({ ...category, title: "새 이름" });
      return jsonResponse({ items: [category] });
    },
  });

  await client.listStudioCategories();
  await client.createStudioCategory({
    slug: "yangja",
    title: "양자",
    description: "양자역학 기록",
    themePreset: "ink",
  });
  await client.updateStudioCategory("category/id", {
    title: "새 이름",
    description: "수정됨",
    themePreset: "forest",
  });
  await client.archiveStudioCategory("category/id");

  assert.equal(requests[0].input, "/api/v1/studio/categories");
  assert.equal(requests[0].init.credentials, "same-origin");

  assert.equal(requests[1].input, "/api/v1/studio/categories");
  assert.equal(requests[1].init.method, "POST");
  assert.deepEqual(JSON.parse(requests[1].init.body), {
    slug: "yangja",
    title: "양자",
    description: "양자역학 기록",
    themePreset: "ink",
  });

  assert.equal(requests[2].input, "/api/v1/studio/categories/category%2Fid");
  assert.equal(requests[2].init.method, "PUT");
  assert.deepEqual(JSON.parse(requests[2].init.body), {
    title: "새 이름",
    description: "수정됨",
    themePreset: "forest",
  });

  assert.equal(requests[3].input, "/api/v1/studio/categories/category%2Fid/archive");
  assert.equal(requests[3].init.method, "POST");
  assert.equal(requests[3].init.body, undefined);
});

test("a revision can explicitly clear its category without putting it in the URL", async () => {
  let captured;
  const client = new OpenSoverignBlogClient({
    fetch: async (input, init) => {
      captured = { input, init };
      return jsonResponse({ id: "document" });
    },
  });

  await client.createStudioRevision("document/id", {
    baseRevisionId: "revision-id",
    title: "제목",
    slug: "post",
    sourceMarkdown: "본문",
    categoryId: null,
  });

  assert.equal(captured.input, "/api/v1/studio/documents/document%2Fid/revisions");
  assert.equal(JSON.parse(captured.init.body).categoryId, null);
  assert.equal(String(captured.input).includes("categoryId"), false);
});
