import assert from "node:assert/strict";
import test from "node:test";

import {
  OpenSoverignBlogClient,
  assetUploadTransportFilename,
} from "../src/index.ts";

const digest = "a".repeat(64);
const responseBody = {
  record: {
    schemaVersion: "1.0",
    digest,
    mediaType: "image/png",
    size: 8,
    originalFilename: "image.png",
    createdAt: "2026-07-27T00:00:00Z",
  },
  url: `https://blog.example/media/${digest}`,
};

test("Studio asset upload sends Unicode filenames through an ASCII-safe header", async () => {
  let captured;
  const client = new OpenSoverignBlogClient({
    fetch: async (input, init) => {
      captured = { input, init };
      return new Response(JSON.stringify(responseBody), {
        status: 201,
        headers: { "Content-Type": "application/json" },
      });
    },
  });
  const png = new Blob(
    [Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])],
    { type: "image/png" },
  );

  await client.uploadStudioAsset(png, "스크린샷 2026-07-27.png");

  assert.equal(captured.input, "/api/v1/studio/assets");
  assert.equal(captured.init.body, png);
  const headers = new Headers(captured.init.headers);
  assert.equal(headers.get("Content-Type"), "image/png");
  assert.match(headers.get("X-OSB-Filename"), /^[A-Za-z0-9_-]+\.png$/);
  assert.match(headers.get("X-OSB-Filename"), /^[\x20-\x7e]+$/);
});

test("asset upload infers a safe image media type when a pasted Blob has none", async () => {
  let headers;
  const client = new OpenSoverignBlogClient({
    fetch: async (_input, init) => {
      headers = new Headers(init.headers);
      return new Response(JSON.stringify(responseBody), {
        status: 201,
        headers: { "Content-Type": "application/json" },
      });
    },
  });
  const untypedPng = new Blob([
    Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  ]);

  await client.uploadAsset(untypedPng, "붙여넣은-이미지");

  assert.equal(headers.get("Content-Type"), "image/png");
  assert.equal(headers.get("X-OSB-Filename"), "image.png");
});

test("asset upload trusts passive-image magic before stale MIME and filename hints", async () => {
  let headers;
  const client = new OpenSoverignBlogClient({
    fetch: async (_input, init) => {
      headers = new Headers(init.headers);
      return new Response(JSON.stringify(responseBody), {
        status: 201,
        headers: { "Content-Type": "application/json" },
      });
    },
  });
  const jpegWithStaleHints = new Blob(
    [Uint8Array.from([0xff, 0xd8, 0xff, 0xdb])],
    { type: "image/png" },
  );

  await client.uploadStudioAsset(jpegWithStaleHints, "stale-extension.webp");

  assert.equal(headers.get("Content-Type"), "image/jpeg");
  assert.equal(headers.get("X-OSB-Filename"), "stale-extension.jpg");
});

test("asset transport filenames are bounded basenames, never paths", () => {
  const value = assetUploadTransportFilename(
    `../../${"very long name ".repeat(30)}.webp`,
    "image/webp",
  );
  assert.match(value, /^[A-Za-z0-9_-]+\.webp$/);
  assert.ok(value.length <= 165);
  assert.equal(value.includes(".."), false);
  assert.equal(value.includes("/"), false);
});
