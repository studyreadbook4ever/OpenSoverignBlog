import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const componentUrl = new URL("../src/public-pages.tsx", import.meta.url);
const stylesUrl = new URL("../src/styles.css", import.meta.url);

test("article Series navigation derives exact ordered neighbors for primary and member blogs", async () => {
  const component = await readFile(componentUrl, "utf8");

  assert.match(component, /async function loadSeriesPostNavigation/);
  assert.match(component, /client\.getPrimarySeriesPosts\(post\.category\.slug, signal\)/);
  assert.match(component, /client\.getBlogSeriesPosts\(post\.blog\.handle, post\.category\.slug, signal\)/);
  assert.match(component, /findIndex\(\(candidate\) => candidate\.id === post\.id\)/);
  assert.match(component, /previous: response\.items\[position - 1\]/);
  assert.match(component, /next: response\.items\[position \+ 1\]/);
  assert.match(component, /if \(isNotFound\(reason\)\) return undefined/);
  assert.match(component, /A transient collection[\s\S]*already-loaded article unreadable/);
});

test("article Series navigation is semantic, canonical, and preserves the selected projection", async () => {
  const component = await readFile(componentUrl, "utf8");

  assert.match(component, /<nav[\s\S]*aria-labelledby="series-post-navigation-title"[\s\S]*className="series-post-navigation"/);
  assert.match(component, /rel="prev"/);
  assert.match(component, /rel="next"/);
  assert.match(component, /aria-label=\{text\([\s\S]*이전 글:/);
  assert.match(component, /aria-label=\{text\([\s\S]*다음 글:/);
  assert.match(component, /function seriesPostHref\(post: FeedPostSummary, view: ViewMode\)/);
  assert.match(component, /return articleHref\(\{[\s\S]*view,[\s\S]*categorySlug: post\.category\.slug[\s\S]*post\.blog\.isPrimary/);
  assert.match(component, /publicCategoryPath\(\{[\s\S]*primary: post\.blog\.isPrimary/);
});

test("Series controls use touch-sized normal-flow links and logical RTL/mobile layout", async () => {
  const [component, styles] = await Promise.all([
    readFile(componentUrl, "utf8"),
    readFile(stylesUrl, "utf8"),
  ]);

  assert.match(styles, /\.series-post-navigation-link \{[\s\S]*min-height: 72px;/);
  assert.match(styles, /\.series-post-navigation \{[\s\S]*padding-block:/);
  assert.match(styles, /\.series-post-navigation-link \{[\s\S]*padding-inline:/);
  assert.match(styles, /:dir\(rtl\) \.series-post-navigation-arrow \{ transform: scaleX\(-1\); \}/);
  assert.match(styles, /@media \(max-width: 600px\)[\s\S]*\.series-post-navigation-links \{ grid-template-columns: minmax\(0, 1fr\); \}/);
  assert.match(styles, /@media \(forced-colors: active\)[\s\S]*\.series-post-navigation-link \{ border-color: CanvasText; \}/);
  assert.doesNotMatch(component, /(?:touchstart|touchmove|pointermove)/);
  assert.doesNotMatch(component, /window\.addEventListener\("keydown"/);
});
