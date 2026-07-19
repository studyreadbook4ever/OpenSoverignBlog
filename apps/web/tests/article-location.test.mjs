import assert from "node:assert/strict";
import test from "node:test";

import { articleHref, articleViewFromSearch } from "../src/article-location.ts";

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
