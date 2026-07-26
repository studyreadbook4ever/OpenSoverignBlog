import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  installSocialEmbedHydration,
  socialEmbedDirective,
  socialEmbedFromInput,
  socialEmbedFromUrl,
} from "../src/social-embeds.ts";

test("normalizes supported YouTube URL shapes", () => {
  for (const href of [
    "https://youtu.be/dQw4w9WgXcQ?t=10",
    "https://www.youtube.com/watch?v=dQw4w9WgXcQ&feature=share",
    "https://youtube.com/shorts/dQw4w9WgXcQ",
    "https://youtube.com/live/dQw4w9WgXcQ",
    "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ?rel=0",
  ]) {
    assert.deepEqual(socialEmbedFromUrl(href), {
      id: "youtube-dQw4w9WgXcQ",
      provider: "youtube",
      resourceId: "dQw4w9WgXcQ",
      canonicalUrl: "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
      title: "YouTube 동영상",
      consentPurposeIds: ["external-media"],
    });
  }
});

test("converts copied YouTube iframe markup into an inert typed reference", () => {
  const embed = socialEmbedFromInput(
    '<iframe width="560" height="315" src="https://www.youtube.com/embed/AbCdEf123_-?si=test" allowfullscreen></iframe>',
  );
  assert.deepEqual(embed, {
    id: "youtube-AbCdEf123_-",
    provider: "youtube",
    resourceId: "AbCdEf123_-",
    canonicalUrl: "https://www.youtube.com/watch?v=AbCdEf123_-",
    title: "YouTube 동영상",
    consentPurposeIds: ["external-media"],
  });
  assert.equal(socialEmbedDirective(embed), "::osb-embed youtube-AbCdEf123_-");
  assert.equal(
    socialEmbedFromInput('<iframe src="javascript:alert(1)"></iframe>'),
    undefined,
  );
  assert.equal(
    socialEmbedFromInput('<script><iframe src="https://youtube.com/embed/dQw4w9WgXcQ"></iframe></script>'),
    undefined,
  );
});

test("normalizes X links into a script-free canonical card", () => {
  assert.deepEqual(socialEmbedFromUrl("https://twitter.com/openai/status/123456789?ref=share"), {
    id: "x-123456789",
    provider: "x",
    resourceId: "123456789",
    canonicalUrl: "https://x.com/openai/status/123456789",
    title: "@openai의 X 게시물",
    consentPurposeIds: [],
  });
});

test("rejects executable, credentialed, malformed, and lookalike URLs", () => {
  for (const href of [
    "javascript:alert(1)",
    "https://user:secret@youtube.com/watch?v=dQw4w9WgXcQ",
    "https://youtube.example/watch?v=dQw4w9WgXcQ",
    "https://x.com/not/a/status",
    "https://x.com/openai/status/not-a-number",
    '<iframe src="javascript:alert(1)"></iframe>',
    '<script><iframe src="https://youtube.com/embed/dQw4w9WgXcQ"></iframe></script>',
  ]) assert.equal(socialEmbedFromUrl(href), undefined, href);
});

test("hydrates a valid YouTube facade only after consent and restores its label", () => {
  const link = fakeLink("원문 보기");
  const figure = fakeFigure("youtube", "dQw4w9WgXcQ", link, "영상 제목");
  const originalDocument = globalThis.document;
  globalThis.document = {
    createElement(tagName) {
      assert.equal(tagName, "iframe");
      return { className: "", src: "", title: "", loading: "", referrerPolicy: "", allow: "", allowFullscreen: false };
    },
  };
  try {
    const cleanup = installSocialEmbedHydration(fakeRoot(figure));
    assert.equal(link.textContent, "YouTube 동영상 불러오기");
    assert.equal(figure.replacedWith, undefined);

    let prevented = false;
    link.click({ preventDefault: () => { prevented = true; } });
    assert.equal(prevented, true);
    assert.equal(figure.dataset.osbLoaded, "true");
    assert.equal(figure.replacedWith.src, "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ");
    assert.equal(figure.replacedWith.title, "영상 제목");
    assert.equal(figure.replacedWith.referrerPolicy, "strict-origin-when-cross-origin");
    assert.equal(figure.replacedWith.allowFullscreen, true);

    cleanup();
    assert.equal(link.textContent, "원문 보기");
  } finally {
    globalThis.document = originalDocument;
  }
});

test("X enhancement is script-free and cleanup restores the original facade", () => {
  const link = fakeLink("원문 보기");
  const figure = fakeFigure("x", "123456789", link, "X post");
  const cleanup = installSocialEmbedHydration(fakeRoot(figure));
  assert.equal(link.textContent, "X에서 원문 보기");
  assert.equal(figure.classList.contains("osb-embed-rich-card"), true);
  assert.equal(link.listenerCount(), 0);

  cleanup();
  assert.equal(link.textContent, "원문 보기");
  assert.equal(figure.classList.contains("osb-embed-rich-card"), false);
});

test("malformed facades stay inert", () => {
  const link = fakeLink("원문 보기");
  const figure = fakeFigure("youtube", "not-an-id", link, "Invalid");
  installSocialEmbedHydration(fakeRoot(figure));
  assert.equal(link.textContent, "원문 보기");
  assert.equal(link.listenerCount(), 0);
});

test("Studio exposes semantic embeds by default and inserts them at the Markdown cursor", async () => {
  const source = await readFile(new URL("../src/studio.tsx", import.meta.url), "utf8");
  assert.match(source, /socialEmbedFromInput\(url, uiLanguage\)/);
  assert.match(source, /<details className="social-embed-composer" open>/);
  assert.match(source, /insertSemanticEmbed\(directive: string\)/);
  assert.match(source, /insertMarkdownBlock\(draft\.sourceMarkdown, start, end, directive\)/);
  assert.match(source, /YouTube iframe 코드/);
  assert.doesNotMatch(source, /sourceMarkdown\.trimEnd\(\).*::osb-embed/s);
});

function fakeRoot(...figures) {
  return {
    querySelectorAll(selector) {
      assert.equal(selector, "figure.osb-embed");
      return figures;
    },
  };
}

function fakeFigure(provider, resourceId, link, caption) {
  const classes = new Set(["osb-embed"]);
  return {
    dataset: { osbProvider: provider, osbResourceId: resourceId },
    classList: {
      add: (value) => classes.add(value),
      remove: (value) => classes.delete(value),
      contains: (value) => classes.has(value),
    },
    querySelector(selector) {
      if (selector === "a.osb-embed-action") return link;
      if (selector === "figcaption") return { textContent: caption };
      return undefined;
    },
    replaceChildren(node) {
      this.replacedWith = node;
    },
  };
}

function fakeLink(text) {
  const listeners = new Map();
  return {
    textContent: text,
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    removeEventListener(type, listener) {
      if (listeners.get(type) === listener) listeners.delete(type);
    },
    click(event) {
      listeners.get("click")?.(event);
    },
    listenerCount() {
      return listeners.size;
    },
  };
}
