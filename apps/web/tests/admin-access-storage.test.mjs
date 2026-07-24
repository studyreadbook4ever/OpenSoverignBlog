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
  assert.match(
    form,
    /text\("관리자 Access Key 입력", "Enter administrator access key"\)/,
  );
  assert.match(
    form,
    /placeholder=\{text\([\s\S]*설치 시 설정한 관리자 Access Key를 붙여넣으세요[\s\S]*Paste the administrator access key configured during setup/,
  );
  assert.match(
    form,
    /text\("관리자 키로 계속", "Continue with administrator key"\)/,
  );
  assert.match(app, /accessKeyLogin[\s\S]{0,120}text\("관리자 키 입력", "Enter administrator key"\)/);
  assert.match(login, /accessKeyLogin[\s\S]{0,120}text\("관리자 키 입력", "Enter administrator key"\)/);
  assert.match(
    studio,
    /text\([\s\S]*관리자 Access Key로 Studio 열기[\s\S]*Open Studio with an administrator access key/,
  );
  assert.match(studio, /<AdminAccessKeyForm\s+autoFocus/);
  assert.match(
    studio,
    /submitLabel=\{text\([\s\S]*관리자 키로 Studio 열기[\s\S]*Open Studio with administrator key/,
  );
  for (const source of [login, studio, categories, tree]) {
    assert.match(source, /<AdminAccessKeyForm/);
  }
});
