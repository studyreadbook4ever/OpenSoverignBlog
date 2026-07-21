import assert from "node:assert/strict";
import test from "node:test";

import { isReferencesPath, REFERENCES_PATH } from "../src/references.ts";

test("the global references route accepts only its canonical path and trailing slash", () => {
  assert.equal(REFERENCES_PATH, "/references");
  assert.equal(isReferencesPath("/references"), true);
  assert.equal(isReferencesPath("/references/"), true);
  assert.equal(isReferencesPath("/references/post"), false);
  assert.equal(isReferencesPath("/reference"), false);
});
