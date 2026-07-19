import assert from "node:assert/strict";
import test from "node:test";

import { OpenSoverignBlogClient } from "../src/index.ts";

const authenticatedSession = {
  state: "authenticated",
  registrationOpen: false,
  user: {
    id: "01900000-0000-7000-8000-000000000001",
    handle: "owner",
    displayName: "Owner",
  },
};

test("access-key login exchanges the key in JSON without a Bearer header", async () => {
  let captured;
  const client = new OpenSoverignBlogClient({
    baseUrl: "/team",
    getAdminToken: () => "legacy-token-must-not-leak",
    fetch: async (input, init) => {
      captured = { input, init };
      return new Response(JSON.stringify(authenticatedSession), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    },
  });

  const session = await client.loginWithAdminAccessKey({ accessKey: "high-entropy-admin-key" });
  assert.deepEqual(session, authenticatedSession);
  assert.equal(captured.input, "/team/api/v1/auth/access-key/session");
  assert.equal(captured.init.method, "POST");
  assert.equal(captured.init.credentials, "same-origin");
  assert.deepEqual(JSON.parse(captured.init.body), { accessKey: "high-entropy-admin-key" });
  const headers = new Headers(captured.init.headers);
  assert.equal(headers.has("Authorization"), false);
  assert.equal(headers.get("Cache-Control"), "no-store");
  assert.equal(headers.get("Pragma"), "no-cache");
});

test("the advertised same-origin auth action can be used through the alias", async () => {
  let url;
  const client = new OpenSoverignBlogClient({
    fetch: async (input) => {
      url = input;
      return new Response(JSON.stringify(authenticatedSession), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    },
  });
  await client.adminAccessLogin(
    { accessKey: "another-high-entropy-key" },
    "/api/v1/auth/access-key/session",
  );
  assert.equal(url, "/api/v1/auth/access-key/session");
});

test("administrator auth actions cannot escape the same-origin auth namespace", async () => {
  const client = new OpenSoverignBlogClient({
    fetch: async () => {
      throw new Error("fetch must not run");
    },
  });
  for (const href of [
    "https://attacker.example/login",
    "//attacker.example/login",
    "/api/v1/feed",
    "/api/v1/auth/../feed",
    "/api/v1/auth/access-key/session#leak",
  ]) {
    await assert.rejects(
      client.loginWithAdminAccessKey({ accessKey: "secret" }, href),
      TypeError,
      href,
    );
  }
});
