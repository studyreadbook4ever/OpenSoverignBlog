import assert from "node:assert/strict";
import test from "node:test";

import { safeBlogStylesheetUrl } from "../src/site-stylesheet.ts";

const origin = "https://notes.example";

test("accepts only the exact blog stylesheet endpoint at root or a deployment subpath", () => {
  assert.equal(
    safeBlogStylesheetUrl(
      "https://notes.example/api/v1/blogs/alice/custom.css",
      "alice",
      origin,
      "/",
    ),
    "https://notes.example/api/v1/blogs/alice/custom.css",
  );
  assert.equal(
    safeBlogStylesheetUrl(
      "/team/api/v1/blogs/alice/custom.css",
      "alice",
      origin,
      "/team",
    ),
    "https://notes.example/team/api/v1/blogs/alice/custom.css",
  );
});

test("rejects URLs that can bypass the server-owned scoped stylesheet endpoint", () => {
  for (const href of [
    "https://attacker.example/team/api/v1/blogs/alice/custom.css",
    "https://notes.example/evil.css",
    "https://notes.example/api/v1/blogs/alice/custom.css",
    "https://notes.example/team/api/v1/blogs/bob/custom.css",
    "https://notes.example/team/api/v1/blogs/alice/custom.css?revision=1",
    "https://notes.example/team/api/v1/blogs/alice/custom.css?",
    "https://notes.example/team/api/v1/blogs/alice/custom.css#theme",
    "https://notes.example/team/api/v1/blogs/alice/custom.css#",
    "https://user:secret@notes.example/team/api/v1/blogs/alice/custom.css",
    "javascript:alert(1)",
    "\\team\\api\\v1\\blogs\\alice\\custom.css",
  ]) {
    assert.equal(
      safeBlogStylesheetUrl(href, "alice", origin, "/team"),
      undefined,
      href,
    );
  }
});
