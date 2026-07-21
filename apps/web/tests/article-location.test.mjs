import assert from "node:assert/strict";
import test from "node:test";

import {
  articleHref,
  articleViewFromSearch,
  publicCategoryPath,
  publicCategoryPostPath,
  publicArticlePath,
  publicFeedPostPath,
} from "../src/article-location.ts";

test("reads every supported article projection from the location query", () => {
  assert.equal(articleViewFromSearch(""), "intent");
  assert.equal(articleViewFromSearch("?view=intent"), "intent");
  assert.equal(articleViewFromSearch("?view=markdown"), "markdown");
  assert.equal(articleViewFromSearch("?view=markdown_source"), "markdown_source");
  assert.equal(articleViewFromSearch("?view=source"), "markdown_source");
  assert.equal(articleViewFromSearch("?view=unknown"), "intent");
});

test("canonical article URLs retain their selected projection", () => {
  assert.equal(articleHref({
    handle: "owner",
    slug: "canonical slug",
    legacy: false,
    view: "markdown_source",
  }), "/@owner/canonical%20slug?view=markdown_source");
  assert.equal(articleHref({
    handle: "ignored",
    slug: "canonical",
    legacy: true,
    view: "intent",
  }), "/blog/canonical?view=intent");
});

test("category article URLs distinguish the primary site from member blogs", () => {
  assert.equal(articleHref({
    handle: "owner/name",
    categorySlug: "quantum notes",
    slug: "first/post",
    legacy: false,
    view: "intent",
  }), "/@owner%2Fname/quantum%20notes/first%2Fpost?view=intent");
  assert.equal(articleHref({
    handle: "ignored",
    categorySlug: "yangja",
    slug: "measurement",
    legacy: false,
    primary: true,
    view: "markdown_source",
  }), "/yangja/measurement?view=markdown_source");
});

test("public listing links retain category placement in their natural path", () => {
  assert.equal(publicArticlePath({
    handle: "owner/name",
    categorySlug: "quantum notes",
    slug: "first/post",
  }), "/@owner%2Fname/quantum%20notes/first%2Fpost");
  assert.equal(publicArticlePath({
    handle: "owner",
    slug: "uncategorized",
  }), "/@owner/uncategorized");
  assert.equal(publicArticlePath({
    handle: "ignored",
    categorySlug: "yangja",
    slug: "measurement",
    primary: true,
  }), "/yangja/measurement");
});

test("feed links derive primary category routing from the server blog summary", () => {
  assert.equal(publicFeedPostPath({
    slug: "measurement",
    category: { slug: "quantum notes" },
    blog: { handle: "primary-owner", isPrimary: true },
  }), "/quantum%20notes/measurement");
  assert.equal(publicFeedPostPath({
    slug: "measurement",
    category: { slug: "quantum notes" },
    blog: { handle: "member/name", isPrimary: false },
  }), "/@member%2Fname/quantum%20notes/measurement");
  assert.equal(publicFeedPostPath({
    slug: "uncategorized",
    blog: { handle: "primary-owner", isPrimary: true },
  }), "/@primary-owner/uncategorized");
});

test("category links use the same primary-site rule as category post links", () => {
  assert.equal(publicCategoryPath({
    handle: "ignored",
    categorySlug: "quantum notes",
    primary: true,
  }), "/quantum%20notes");
  assert.equal(publicCategoryPostPath({
    handle: "member/name",
    categorySlug: "quantum notes",
    postSlug: "first/post",
    primary: false,
  }), "/@member%2Fname/quantum%20notes/first%2Fpost");
});
