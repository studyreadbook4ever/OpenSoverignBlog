import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("Studio manages one ordered max-three list of Series and standalone posts", async () => {
  const [studio, state, styles] = await Promise.all([
    readFile(new URL("../src/studio.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/studio-state.ts", import.meta.url), "utf8"),
    readFile(new URL("../src/styles.css", import.meta.url), "utf8"),
  ]);

  assert.match(studio, /capabilities\?\.features\.includes\("home_curation"\)/);
  assert.match(studio, /client\.listStudioDocuments\(/);
  assert.match(studio, /client\.listStudioSeries\(/);
  assert.doesNotMatch(studio, /client\.listStudioSeriesItems\(/);
  assert.match(studio, /client\.getHomePins\(/);
  assert.match(studio, /client\.replaceHomePinTargets\(homePins\)/);
  assert.match(studio, /homePins\.length >= 3/);
  assert.match(studio, /aria-labelledby="studio-home-pin-title"/);
  assert.match(studio, /candidate\.kind === "series" \? "Series" : "Post"/);
  assert.match(studio, /&& !isSeriesMember \? \(/);
  assert.match(studio, /homeCurationRows\(curationCandidates, homePins, uiLanguage\)/);
  assert.match(state, /response\.targets\s*\?\?\s*response\.documentIds\.map/);
  assert.match(state, /Object\.hasOwn\(document, "publishedCategoryId"\)/);
  assert.match(state, /publishedSeriesMembership\(\s*studioDocuments,/);
  assert.match(styles, /\.studio-home-pin-panel \{/);
  assert.match(styles, /\.studio-home-pin-candidates \{/);
  assert.match(styles, /\.home-pin-kind-series \{/);
  assert.match(styles, /\.document-home-pin\[aria-pressed="true"\]/);
});
