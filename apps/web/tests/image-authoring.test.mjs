import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(path, import.meta.url), "utf8");

test("Studio offers file selection, clipboard paste, and drag-and-drop image authoring", async () => {
  const [studio, styles] = await Promise.all([
    read("../src/studio.tsx"),
    read("../src/styles.css"),
  ]);

  assert.match(studio, /multiple\s+onChange=\{uploadSelectedImages\}/);
  assert.match(studio, /onPaste=\{handleMarkdownPaste\}/);
  assert.match(studio, /transferFiles\(event\.clipboardData\)/);
  assert.match(studio, /onDragOver=\{handleImageDrag\}/);
  assert.match(studio, /onDrop=\{handleImageDrop\}/);
  assert.match(studio, /readOnly=\{imageUploadPending\}/);
  assert.match(studio, /selectStudioImageBatch\(files\)/);
  assert.match(studio, /uploadStudioImageQueue\(selected\.accepted/);
  assert.match(studio, /firstPartyAssetMarkdownUrl\(response\.url, window\.location\.href\)/);
  assert.match(studio, /markdownImageSource\(displayName, url\)/);
  assert.match(styles, /\.markdown-editor-dropzone\.is-dragging/);
  assert.match(styles, /\.image-drop-overlay/);
});

test("Studio cannot save or publish while an image batch is in flight", async () => {
  const studio = await read("../src/studio.tsx");

  assert.match(studio, /if \(imageUploadPending\) \{\s*setStatus\(text\("이미지 업로드가 끝난 뒤 글을 저장해 주세요/);
  assert.match(studio, /if \(imageUploadPending\) \{\s*setStatus\(text\("이미지 업로드가 끝난 뒤 글을 공개해 주세요/);
  assert.match(studio, /disabled=\{saving \|\| publishing \|\| imageUploadPending \|\| revisionMatchesDraft\}/);
  assert.match(studio, /disabled=\{publishing \|\| imageUploadPending \|\| !canPublish\}/);
});
