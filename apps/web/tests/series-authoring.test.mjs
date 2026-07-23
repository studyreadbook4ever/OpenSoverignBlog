import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(path, import.meta.url), "utf8");

test("Studio makes Post versus Series an explicit first authoring choice", async () => {
  const [app, series, studio] = await Promise.all([
    read("../src/app.tsx"),
    read("../src/series.tsx"),
    read("../src/studio.tsx"),
  ]);

  assert.match(app, /pathname === "\/studio\/new"/);
  assert.match(series, /setChoice\("post"\)/);
  assert.match(series, /setChoice\("series"\)/);
  assert.match(series, />Post</);
  assert.match(series, />Series</);
  assert.match(series, /client\.createStudioSeries\(input\)/);
  assert.match(series, /studio\/write\?series=/);
  assert.match(series, /Add a post to this series/);
  assert.match(studio, /href="\/studio\/new"/);
});

test("post placement distinguishes series from standalone categories", async () => {
  const studio = await read("../src/studio.tsx");

  assert.match(studio, /client\.listStudioSeries\(controller\.signal\)/);
  assert.match(studio, /<optgroup label="Series">/);
  assert.match(studio, /<optgroup label="Category">/);
  assert.match(studio, /seriesCategoryIds/);
  assert.match(studio, /시리즈 글은 발행할 때 읽는 순서의 끝에 추가됩니다/);
});

test("Series Studio exposes promotion, ordering, metadata, and archive workflows", async () => {
  const series = await read("../src/series.tsx");

  assert.match(series, /promoteStudioCategoryToSeries/);
  assert.match(series, /replaceStudioSeriesOrder/);
  assert.match(series, /updateStudioSeries/);
  assert.match(series, /archiveStudioSeries/);
  assert.match(series, /aria-label=\{text\(`\$\{document\.revision\.title\} 위로`/);
});

test("public collection routes prefer Series metadata and exact reading order", async () => {
  const categories = await read("../src/categories.tsx");

  assert.match(categories, /client\.getPrimarySeries\(categorySlug/);
  assert.match(categories, /client\.getBlogSeries\(handle, categorySlug/);
  assert.match(categories, /client\.getPrimarySeriesPosts\(categorySlug/);
  assert.match(categories, /client\.getBlogSeriesPosts\(handle, categorySlug/);
  assert.match(categories, /if \(!isNotFound\(reason\)\) throw reason/);
  assert.match(categories, /reading order/);
});
