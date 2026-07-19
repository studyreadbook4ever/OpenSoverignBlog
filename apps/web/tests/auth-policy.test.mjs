import assert from "node:assert/strict";
import test from "node:test";

import {
  adminAuthChoices,
  safeAuthActionHref,
  studioAccessFor,
} from "../src/auth-policy.ts";

function capabilities(overrides = {}) {
  return {
    version: "1.0",
    views: ["intent", "markdown", "markdown_source"],
    features: [],
    modules: [],
    unavailableByDefault: [],
    mutationMechanisms: [],
    mutationMode: "read_only",
    ...overrides,
  };
}

test("v2 Studio access overrides the compatible v1 mutation mode", () => {
  assert.equal(studioAccessFor(capabilities({ mutationMode: "authenticated_members" })), "members");
  assert.equal(studioAccessFor(capabilities({ mutationMode: "removed_owner_bearer" })), "disabled");
  assert.equal(studioAccessFor(capabilities({
    mutationMode: "authenticated_members",
    studioAccess: "disabled",
  })), "disabled");
  assert.equal(studioAccessFor(capabilities({
    version: "2.0",
    mutationMode: "authenticated_members",
  })), "disabled");
});

test("only ready, same-origin API authentication methods become UI choices", () => {
  const accessKey = {
    id: "admin-access-key",
    kind: "access_key",
    flow: "secret_exchange",
    audience: "admin",
    label: "관리자 접근 키",
    actionHref: "/api/v1/auth/access-key/session",
  };
  const external = {
    id: "admin-external",
    kind: "external",
    flow: "redirect",
    audience: "admin",
    provider: "firebase",
    label: "Firebase로 계속",
    actionHref: "/api/v1/auth/external/start",
  };
  const unsafeExternal = { ...external, actionHref: "https://attacker.example/login" };
  const choices = adminAuthChoices(capabilities({
    version: "2.0",
    studioAccess: "admin_only",
    auth: { status: "ready", methods: [accessKey, external, unsafeExternal] },
  }));
  assert.deepEqual(choices.accessKeyMethods, [accessKey]);
  assert.deepEqual(choices.externalMethods, [external]);
  assert.equal(safeAuthActionHref(external), "/api/v1/auth/external/start");
  assert.equal(safeAuthActionHref(unsafeExternal), undefined);
});

test("disabled and misconfigured authentication fail closed", () => {
  const method = {
    id: "admin-access-key",
    kind: "access_key",
    flow: "secret_exchange",
    audience: "admin",
    label: "관리자 접근 키",
    actionHref: "/api/v1/auth/access-key/session",
  };
  for (const status of ["disabled", "misconfigured"]) {
    const choices = adminAuthChoices(capabilities({
      version: "2.0",
      studioAccess: "admin_only",
      auth: { status, methods: [method] },
    }));
    assert.equal(choices.status, status);
    assert.deepEqual(choices.accessKeyMethods, []);
    assert.deepEqual(choices.externalMethods, []);
  }
});

test("malformed v2 method payloads fail closed instead of breaking the login page", () => {
  const missingMethods = adminAuthChoices(capabilities({
    version: "2.0",
    studioAccess: "admin_only",
    auth: { status: "ready" },
  }));
  assert.deepEqual(missingMethods.accessKeyMethods, []);
  assert.deepEqual(missingMethods.externalMethods, []);

  const missingAction = adminAuthChoices(capabilities({
    version: "2.0",
    studioAccess: "admin_only",
    auth: {
      status: "ready",
      methods: [{
        id: "admin-access-key",
        kind: "access_key",
        flow: "secret_exchange",
        audience: "admin",
        label: "관리자 접근 키",
      }],
    },
  }));
  assert.deepEqual(missingAction.accessKeyMethods, []);

  const invalidElements = adminAuthChoices(capabilities({
    version: "2.0",
    studioAccess: "admin_only",
    auth: {
      status: "ready",
      methods: [
        null,
        {
          id: "wrong-id",
          kind: "access_key",
          flow: "secret_exchange",
          audience: "admin",
          label: "Wrong id",
          actionHref: "/api/v1/auth/access-key/session",
        },
        {
          id: "admin-access-key",
          kind: "access_key",
          flow: "secret_exchange",
          audience: "member",
          label: "Wrong audience",
          actionHref: "/api/v1/auth/access-key/session",
        },
      ],
    },
  }));
  assert.deepEqual(invalidElements.accessKeyMethods, []);
  assert.deepEqual(invalidElements.externalMethods, []);
});
