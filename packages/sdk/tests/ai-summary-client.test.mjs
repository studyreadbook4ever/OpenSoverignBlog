import assert from "node:assert/strict";
import test from "node:test";

import { OpenSoverignBlogClient } from "../src/index.ts";

test("AI summary generation keeps the one-shot key out of URLs and JSON", async () => {
  const secret = "sk-one-shot-secret-value";
  let captured;
  const client = new OpenSoverignBlogClient({
    baseUrl: "/owned",
    fetch: async (input, init) => {
      captured = { input, init };
      return new Response(JSON.stringify({
        candidate: {
          text: "요약",
          sourceHash: `sha256:${"a".repeat(64)}`,
          provenance: {
            provider: "openai",
            model: "gpt-5.4-mini",
            promptVersion: "osb-summary-plain-text/1",
            generatedAt: "2026-07-22T00:00:00Z",
            humanReviewed: false,
          },
        },
      }), { status: 200, headers: { "Content-Type": "application/json" } });
    },
  });

  await client.generateAiSummary({
    provider: "openai",
    model: "gpt-5.4-mini",
    credentialMode: "one_shot",
    title: "제목",
    sourceMarkdown: "본문",
  }, secret);

  assert.equal(captured.input, "/owned/api/v1/studio/ai-summary/generate");
  assert.equal(captured.init.method, "POST");
  assert.equal(captured.init.redirect, "error");
  assert.equal(captured.init.credentials, "same-origin");
  const headers = new Headers(captured.init.headers);
  assert.equal(headers.get("X-OSB-AI-One-Shot-Key"), secret);
  assert.equal(headers.get("Cache-Control"), "no-store");
  assert.equal(headers.get("Pragma"), "no-cache");
  assert.equal(String(captured.input).includes(secret), false);
  assert.equal(String(captured.init.body).includes(secret), false);
});
