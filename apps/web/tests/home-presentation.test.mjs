import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  homeSeriesAnchor,
  presentHome,
} from "../src/home-presentation.ts";

const post = (id, category) => ({
  id,
  ...(category ? { category } : {}),
});
const category = (id, slug, title, description) => ({ id, slug, title, description });
const series = (id, categoryId, slug, title = slug) => ({
  id,
  categoryId,
  slug,
  title,
  status: "active",
});

test("authoritative typed units preserve one flat post/series order", () => {
  const orderedSeries = series("series-id", "category-id", "ordered-notes");
  const presentation = presentHome({
    units: [
      { kind: "post", post: post("pinned-post") },
      {
        kind: "series",
        series: orderedSeries,
        items: [post("series-1"), post("series-2")],
      },
      { kind: "post", post: post("ordinary-post") },
      { kind: "post", post: post("ordinary-post") },
    ],
    // Typed units are authoritative even if rolling-upgrade compatibility
    // projections contain a conflicting order.
    pinnedItems: [post("legacy-pin")],
    recentItems: [post("legacy-recent")],
    seriesSections: [],
    categorySections: [],
  });

  assert.deepEqual(
    presentation.units.map((unit) => (
      unit.kind === "post"
        ? ["post", unit.post.id]
        : ["series", unit.series.id, unit.items.map(({ id }) => id)]
    )),
    [
      ["post", "pinned-post"],
      ["series", "series-id", ["series-1", "series-2"]],
      ["post", "ordinary-post"],
    ],
  );
  assert.equal(presentation.units[1].anchorId, homeSeriesAnchor("ordered-notes"));
});

test("legacy Series-member pins remain visible with the old server projection", () => {
  const seriesCategory = category("series-category", "series", "Series");
  const plainCategory = category("plain-category", "notes", "Notes");
  const orderedSeries = series("series-id", seriesCategory.id, "series");
  const first = post("series-first", seriesCategory);
  const second = post("series-second", seriesCategory);

  const presentation = presentHome({
    pinnedItems: [second, post("standalone-pin")],
    seriesSections: [{
      series: orderedSeries,
      // Schema-9 servers removed pinned IDs before constructing Series rows.
      items: [first],
    }],
    categorySections: [
      { category: seriesCategory, items: [first, second] },
      { category: plainCategory, items: [post("categorized", plainCategory)] },
    ],
    recentItems: [
      second,
      post("categorized", plainCategory),
      post("standalone-recent"),
    ],
  });

  assert.deepEqual(
    presentation.units.map((unit) => (
      unit.kind === "post"
        ? ["post", unit.post.id]
        : ["series", unit.series.id, unit.items.map(({ id }) => id)]
    )),
    [
      ["post", "series-second"],
      ["post", "standalone-pin"],
      ["series", "series-id", ["series-first"]],
      ["post", "categorized"],
      ["post", "standalone-recent"],
    ],
  );
});

test("legacy recent-only payloads remain readable as standalone home units", () => {
  const presentation = presentHome({
    pinnedItems: [],
    recentItems: [post("one"), post("two"), post("one")],
  });

  assert.deepEqual(
    presentation.units.map((unit) => [unit.kind, unit.kind === "post" && unit.post.id]),
    [["post", "one"], ["post", "two"]],
  );
});

test("dense home rows no longer contain or reserve the one-character index", async () => {
  const [component, styles] = await Promise.all([
    readFile(new URL("../src/public-pages.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/styles.css", import.meta.url), "utf8"),
  ]);

  assert.doesNotMatch(component, /wiki-post-index/);
  assert.doesNotMatch(styles, /wiki-post-index/);
  assert.match(styles, /\.wiki-post-row \{[^}]*grid-template-columns: minmax\(0, 1fr\) auto;/);
  assert.match(styles, /@media \(max-width: 600px\)[\s\S]*?\.wiki-post-row \{[^}]*grid-template-columns: minmax\(0, 1fr\);/);
});

test("home chrome omits operational counters and the generic publishing hero", async () => {
  const [component, styles] = await Promise.all([
    readFile(new URL("../src/public-pages.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/styles.css", import.meta.url), "utf8"),
  ]);

  assert.doesNotMatch(component, /<dt>\{text\("공개 글", "Public posts"\)/);
  assert.doesNotMatch(component, /<dt>\{text\("운영 모드", "Mode"\)/);
  assert.doesNotMatch(component, /className="wiki-welcome"/);
  assert.doesNotMatch(component, /Markdown 원문과 서버를 작성자가 직접 소유/);
  assert.doesNotMatch(component, /\$\{items\.length\} posts/);
  assert.doesNotMatch(styles, /\.wiki-sidebar dl/);
  assert.doesNotMatch(styles, /\.wiki-welcome/);
  assert.match(styles, /\.wiki-main > \.wiki-panel:first-child \{ margin-top: 0; \}/);
});

test("home renders peer units without featured, recent, or category grouping", async () => {
  const [component, styles] = await Promise.all([
    readFile(new URL("../src/public-pages.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/styles.css", import.meta.url), "utf8"),
  ]);

  assert.match(component, /presentation\.units\.map/);
  assert.match(component, /function HomeStandalonePost/);
  assert.match(component, /className="wiki-panel wiki-panel-post"/);
  assert.match(component, /function HomeSeriesUnitPanel/);
  assert.doesNotMatch(component, /id="home-pinned"/);
  assert.doesNotMatch(component, /id="home-recent"/);
  assert.doesNotMatch(component, /tone="(?:pinned|recent|category)"/);
  assert.doesNotMatch(styles, /\.wiki-panel-recent/);
});

test("only Series units expose an accessible collapse control and start closed", async () => {
  const [component, styles] = await Promise.all([
    readFile(new URL("../src/public-pages.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/styles.css", import.meta.url), "utf8"),
  ]);

  assert.match(component, /function HomeSeriesUnitPanel[\s\S]*useState\(false\)/);
  assert.match(component, /aria-controls=\{contentId\}/);
  assert.match(component, /aria-expanded=\{expanded\}/);
  assert.match(component, /hidden=\{!expanded\}/);
  assert.match(component, /window\.addEventListener\("hashchange", revealFragmentTarget\)/);
  assert.doesNotMatch(component, /function HomeStandalonePost[\s\S]*aria-expanded/);
  assert.match(styles, /\.wiki-panel-toggle \{/);
  assert.match(styles, /\.wiki-post-list\[hidden\] \{ display: none; \}/);
});
