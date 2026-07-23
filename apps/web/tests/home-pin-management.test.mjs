import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("Studio exposes prominent administrator-only max-three home pin management", async () => {
  const [studio, styles] = await Promise.all([
    readFile(new URL("../src/studio.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/styles.css", import.meta.url), "utf8"),
  ]);

  assert.match(studio, /capabilities\?\.features\.includes\("home_curation"\)/);
  assert.match(studio, /client\.listStudioDocuments\(/);
  assert.match(studio, /client\.getHomePins\(/);
  assert.match(studio, /client\.replaceHomePins\(homePins\)/);
  assert.match(studio, /homePins\.length >= 3/);
  assert.match(studio, /aria-labelledby="studio-home-pin-title"/);
  assert.match(studio, /aria-pressed=\{homePins\.includes\(document\.id\)\}/);
  assert.match(studio, /disabled=\{!homePins\.includes\(document\.id\) && homePins\.length >= 3\}/);
  assert.match(styles, /\.studio-home-pin-panel \{/);
  assert.match(styles, /\.document-home-pin\[aria-pressed="true"\]/);
});
