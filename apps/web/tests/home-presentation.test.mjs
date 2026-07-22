import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { homeCategoryAnchor, presentHome } from "../src/home-presentation.ts";

const post = (id) => ({ id });
const category = (id, slug, title, description) => ({ id, slug, title, description });

test("home presentation preserves API section and item order without duplicate rows", () => {
  const home = {
    pinnedItems: [post("pinned")],
    categorySections: [
      {
        category: category("yangja-id", "yangja", "yangja", "양자 컴퓨팅"),
        items: [post("pinned"), post("yangja-1"), post("yangja-2")],
      },
      {
        category: category("ontology-id", "ontology", "ontology", "온톨로지"),
        items: [post("ontology-1"), post("ontology-2")],
      },
    ],
    recentItems: [
      post("ontology-2"),
      post("yangja-1"),
      post("uncategorized"),
      post("uncategorized"),
    ],
  };

  const presentation = presentHome(home);

  assert.deepEqual(
    presentation.categorySections.map(({ category: item }) => [item.title, item.description]),
    [["yangja", "양자 컴퓨팅"], ["ontology", "온톨로지"]],
  );
  assert.deepEqual(
    presentation.categorySections.map(({ items }) => items.map(({ id }) => id)),
    [["yangja-1", "yangja-2"], ["ontology-1", "ontology-2"]],
  );
  assert.deepEqual(presentation.recentItems.map(({ id }) => id), ["uncategorized"]);
  assert.equal(presentation.total, 6);
  assert.equal(presentation.categorySections[0].anchorId, "home-category-yangja");
  assert.equal(presentation.categorySections[0].anchorId, homeCategoryAnchor("yangja"));
});

test("empty and duplicate-only category sections do not produce dead sidebar targets", () => {
  const presentation = presentHome({
    pinnedItems: [post("pinned")],
    categorySections: [
      { category: category("empty", "empty", "Empty"), items: [] },
      { category: category("duplicate", "duplicate", "Duplicate"), items: [post("pinned")] },
    ],
    recentItems: [],
  });

  assert.deepEqual(presentation.categorySections, []);
  assert.equal(presentation.total, 1);
});

test("legacy home payloads still render their recent rows", () => {
  const presentation = presentHome({
    pinnedItems: [],
    recentItems: [post("one"), post("two")],
  });

  assert.deepEqual(presentation.recentItems.map(({ id }) => id), ["one", "two"]);
  assert.equal(presentation.total, 2);
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
