import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("the browser UI never persists or installs the administrator access key", async () => {
  const sources = await Promise.all([
    "src/app.tsx",
    "src/lib.tsx",
    "src/public-pages.tsx",
    "src/studio.tsx",
  ].map((path) => readFile(new URL(`../${path}`, import.meta.url), "utf8")));
  const source = sources.join("\n");
  assert.doesNotMatch(source, /osb\.adminToken/);
  assert.doesNotMatch(source, /(?:localStorage|sessionStorage)[^\n]*accessKey/i);
  assert.doesNotMatch(source, /accessKey[^\n]*(?:localStorage|sessionStorage)/i);
  assert.doesNotMatch(source, /Authorization[^\n]*accessKey/i);
});
