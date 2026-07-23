import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import test from "node:test";

test("the browser UI never persists or installs the administrator access key", async () => {
  const sourceRoot = new URL("../src/", import.meta.url);
  const paths = (await readdir(sourceRoot, { recursive: true }))
    .filter((path) => /\.[cm]?[jt]sx?$/.test(path));
  const sources = await Promise.all(
    paths.map((path) => readFile(new URL(path, sourceRoot), "utf8")),
  );
  const source = sources.join("\n");
  assert.doesNotMatch(source, /osb\.adminToken/);
  assert.doesNotMatch(source, /(?:localStorage|sessionStorage)[^\n]*accessKey/i);
  assert.doesNotMatch(source, /accessKey[^\n]*(?:localStorage|sessionStorage)/i);
  assert.doesNotMatch(source, /Authorization[^\n]*accessKey/i);
});

test("the reusable administrator key form clears its local credential and is visible at Studio gates", async () => {
  const [form, app, login, studio, categories, tree] = await Promise.all([
    readFile(new URL("../src/admin-access.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/app.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/public-pages.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/studio.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/categories.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/admin-tree.tsx", import.meta.url), "utf8"),
  ]);

  assert.match(form, /client\.loginWithAdminAccessKey\(/);
  assert.match(form, /method\.actionHref/);
  assert.match(form, /finally \{\s*setAccessKey\(""\)/);
  assert.match(form, /autoComplete="off"/);
  assert.match(form, /type="password"/);
  assert.match(app, /accessKeyLogin[\s\S]{0,120}text\("관리자 키 입력", "Enter administrator key"\)/);
  assert.match(login, /accessKeyLogin[\s\S]{0,120}text\("관리자 키 입력", "Enter administrator key"\)/);
  for (const source of [login, studio, categories, tree]) {
    assert.match(source, /<AdminAccessKeyForm/);
  }
});
