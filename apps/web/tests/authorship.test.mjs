import assert from "node:assert/strict";
import test from "node:test";

import { authorshipLabel, normalizedAuthorship } from "../src/authorship.ts";

test("legacy revisions remain human without exposing an internal actor", () => {
  assert.deepEqual(normalizedAuthorship(undefined), { kind: "human", humanReviewed: false });
  assert.equal(authorshipLabel(undefined), "사람이 작성");
});

test("AI labels carry portable generator and review metadata", () => {
  assert.equal(authorshipLabel({
    kind: "ai_assisted",
    generator: "local/model-v1",
    humanReviewed: true,
  }), "AI 보조 · local/model-v1 · 사람 검토");
});
