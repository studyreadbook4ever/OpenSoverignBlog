import assert from "node:assert/strict";
import test from "node:test";

import { OpenSoverignBlogClient } from "../src/index.ts";

test("reads advertising consent without allowing it to be cached", async () => {
  let captured;
  const client = new OpenSoverignBlogClient({
    baseUrl: "/journal",
    fetch: async (input, init) => {
      captured = { input, init };
      return Response.json({ decision: "unknown", policyVersion: "kakao-adfit/1" });
    },
  });

  assert.deepEqual(await client.advertisingConsent(), {
    decision: "unknown",
    policyVersion: "kakao-adfit/1",
  });
  assert.equal(captured.input, "/journal/api/v1/advertising/consent");
  assert.equal(captured.init.credentials, "same-origin");
  assert.equal(captured.init.method, undefined);
  const headers = new Headers(captured.init.headers);
  assert.equal(headers.get("Cache-Control"), "no-store");
  assert.equal(headers.get("Pragma"), "no-cache");
});

test("persists only a closed advertising consent decision", async () => {
  let captured;
  const client = new OpenSoverignBlogClient({
    fetch: async (input, init) => {
      captured = { input, init };
      return Response.json({ decision: "denied" });
    },
  });

  assert.deepEqual(
    await client.setAdvertisingConsent(
      { decision: "denied" },
      "/api/v1/advertising/consent",
    ),
    { decision: "denied" },
  );
  assert.equal(captured.input, "/api/v1/advertising/consent");
  assert.equal(captured.init.method, "POST");
  assert.equal(captured.init.credentials, "same-origin");
  assert.deepEqual(JSON.parse(captured.init.body), { decision: "denied" });
  assert.equal(new Headers(captured.init.headers).has("Authorization"), false);
});

test("advertising consent endpoints cannot redirect decisions off origin", async () => {
  const client = new OpenSoverignBlogClient({
    fetch: async () => {
      throw new Error("fetch must not run");
    },
  });

  for (const href of [
    "https://attacker.example/consent",
    "//attacker.example/consent",
    "/api/v1/advertising/consent?next=https://attacker.example",
    "/api/v1/advertising/../feed",
    "/api/v1/feed",
  ]) {
    await assert.rejects(client.advertisingConsent(href), TypeError, href);
    await assert.rejects(
      client.setAdvertisingConsent({ decision: "granted" }, href),
      TypeError,
      href,
    );
  }
});
