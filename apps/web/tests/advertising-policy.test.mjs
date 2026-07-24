import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  KAKAO_ADFIT_SCRIPT_URL,
  advertisingUnitFor,
  installKakaoAdFitLoader,
  isConfirmedAdvertisingReaderContent,
  isAdvertisingReaderPath,
  isSupportedAdvertising,
} from "../src/advertising-policy.ts";

const advertising = {
  provider: "kakao-adfit",
  scriptUrl: KAKAO_ADFIT_SCRIPT_URL,
  policyVersion: "kakao-adfit/1",
  consent: {
    required: true,
    statusHref: "/api/v1/advertising/consent",
    actionHref: "/api/v1/advertising/consent",
    purposeIds: ["ads.delivery", "ads.measurement", "ads.personalization"],
    privacyHref: "https://business.kakao.com/info/privacy/",
    policyHref: "https://adfit.kakao.com/web/html/use_kakao.html",
  },
  placements: {
    top: {
      pc: { unitId: "DAN-PC_TOP_0001", width: 728, height: 90 },
      mobile: { unitId: "DAN-MOBILE_TOP_0001", width: 320, height: 100 },
    },
    bottom: {
      pc: { unitId: "DAN-PC_BOTTOM_0001", width: 728, height: 90 },
      mobile: { unitId: "DAN-MOBILE_BOTTOM_0001", width: 320, height: 100 },
    },
  },
};

test("advertising is limited to public reader routes", () => {
  for (const pathname of [
    "/",
    "/index.html",
    "/references",
    "/@writer",
    "/@writer/post",
    "/@writer/series/post",
    "/blog/legacy-post",
    "/ontology",
    "/ontology/post",
  ]) {
    assert.equal(isAdvertisingReaderPath(pathname), true, pathname);
  }

  for (const pathname of [
    "/login",
    "/login/",
    "/onboarding",
    "/onboarding/",
    "/studio",
    "/studio/",
    "/studio/write",
    "/studio/write/document-id",
    "/studio/settings",
    "/api/v1/feed",
    "/.well-known/open-soverign-blog.json",
    "/.WELL-KNOWN/open-soverign-blog.json",
    "/ads.txt",
    "/ADS.TXT",
    "/AI2AI.md",
    "/favicon.svg",
    "/INDEX.CSS",
    "/references/archive",
    "/blog",
    "/BLOG/legacy-post",
    "/@",
    "/@/post",
    "/ontology//post",
    "//",
    "//attacker.example/",
  ]) {
    assert.equal(isAdvertisingReaderPath(pathname), false, pathname);
  }
});

test("advertising requires confirmed reader content, including home, series, and posts", () => {
  for (const pathname of [
    "/",
    "/ontology",
    "/ontology/post",
    "/@writer/post",
  ]) {
    assert.equal(
      isConfirmedAdvertisingReaderContent(pathname, true),
      true,
      pathname,
    );
    assert.equal(
      isConfirmedAdvertisingReaderContent(pathname, false),
      false,
      pathname,
    );
  }

  assert.equal(
    isConfirmedAdvertisingReaderContent("/some-client-side-404", false),
    false,
  );
  assert.equal(isConfirmedAdvertisingReaderContent("/studio", true), false);
});

test("only the official loader and fixed desktop/mobile sizes are accepted", () => {
  assert.equal(isSupportedAdvertising(advertising), true);
  assert.deepEqual(advertisingUnitFor(advertising, "top", "pc"), {
    unitId: "DAN-PC_TOP_0001",
    width: 728,
    height: 90,
  });
  assert.deepEqual(advertisingUnitFor(advertising, "bottom", "mobile"), {
    unitId: "DAN-MOBILE_BOTTOM_0001",
    width: 320,
    height: 100,
  });

  assert.equal(isSupportedAdvertising({
    ...advertising,
    scriptUrl: "https://ads.example.invalid/loader.js",
  }), false);
  assert.equal(isSupportedAdvertising({
    ...advertising,
    placements: {
      ...advertising.placements,
      top: {
        ...advertising.placements.top,
        pc: { unitId: "DAN-PC_TOP_0001", width: 300, height: 250 },
      },
    },
  }), false);
});

test("the application exposes exactly two edge slots and no iframe adapter", async () => {
  const appSource = await readFile(new URL("../src/app.tsx", import.meta.url), "utf8");
  const adSource = await readFile(new URL("../src/kakao-adfit.tsx", import.meta.url), "utf8");
  const policySource = await readFile(
    new URL("../src/advertising-policy.ts", import.meta.url),
    "utf8",
  );
  const publicPagesSource = await readFile(
    new URL("../src/public-pages.tsx", import.meta.url),
    "utf8",
  );
  const categoriesSource = await readFile(
    new URL("../src/categories.tsx", import.meta.url),
    "utf8",
  );

  assert.equal(
    (appSource.match(/<KakaoAdFitSlot placement=/g) ?? []).length,
    2,
  );
  const header = appSource.indexOf("<SiteHeader />");
  const top = appSource.indexOf('<KakaoAdFitSlot placement="top" />');
  const main = appSource.indexOf('<main className="route-main"');
  const bottom = appSource.indexOf('<KakaoAdFitSlot placement="bottom" />');
  const footer = appSource.indexOf("<SiteFooter />");
  assert.equal(header < top && top < main && main < bottom && bottom < footer, true);

  assert.match(adSource, /<ins[\s\S]*className="kakao_ad_area"/);
  assert.match(
    appSource,
    /contentReady=\{readerContentReady\}/,
  );
  assert.match(
    appSource,
    /readerContent\.pathname === pathname[\s\S]*readerContent\.status === "ready"/,
  );
  assert.match(
    publicPagesSource,
    /function NotFoundPage\(\)[\s\S]*usePublicReaderContentStatus\("error"\)/,
  );
  assert.match(
    publicPagesSource,
    /function ArticlePage\([\s\S]*usePublicReaderContentStatus\([\s\S]*post \? "ready" : "pending"/,
  );
  assert.match(
    categoriesSource,
    /function CategoryPage\([\s\S]*usePublicReaderContentStatus\([\s\S]*collection \? "ready" : "pending"/,
  );
  assert.match(adSource, /if \(!authorized \|\| !supportedAdvertising\)/);
  assert.match(
    adSource,
    /\|\| !context\.advertising[\s\S]*\|\| !context\.authorized[\s\S]*return null;/,
  );
  assert.equal(adSource.includes("<iframe"), false);
  assert.equal(policySource.includes(KAKAO_ADFIT_SCRIPT_URL), true);
});

test("the official loader is consent-markup gated, unique, and removable", () => {
  const inert = fakeDocument(false);
  installKakaoAdFitLoader(inert, KAKAO_ADFIT_SCRIPT_URL);
  assert.equal(inert.scripts.length, 0);

  const document = fakeDocument(true);
  const firstCleanup = installKakaoAdFitLoader(
    document,
    KAKAO_ADFIT_SCRIPT_URL,
  );
  assert.equal(document.scripts.length, 1);
  assert.equal(document.scripts[0].src, KAKAO_ADFIT_SCRIPT_URL);
  assert.equal(document.scripts[0].async, true);
  assert.equal(document.scripts[0].charset, "utf-8");
  assert.equal(document.scripts[0].type, "text/javascript");

  const secondCleanup = installKakaoAdFitLoader(
    document,
    KAKAO_ADFIT_SCRIPT_URL,
  );
  assert.equal(document.scripts.length, 1);
  firstCleanup();
  assert.equal(document.scripts.length, 1);
  secondCleanup();
  assert.equal(document.scripts.length, 0);

  installKakaoAdFitLoader(document, "https://ads.example.invalid/loader.js");
  assert.equal(document.scripts.length, 0);
});

function fakeDocument(hasAuthorizedUnit) {
  const scripts = [];
  function makeScript(src = "") {
    const attributes = new Map();
    const script = {
      async: false,
      charset: "",
      src,
      type: "",
      remove() {
        const index = scripts.indexOf(script);
        if (index >= 0) scripts.splice(index, 1);
      },
      setAttribute(name, value) {
        attributes.set(name, value);
      },
      hasAttribute(name) {
        return attributes.has(name);
      },
    };
    return script;
  }
  return {
    createElement(tagName) {
      assert.equal(tagName, "script");
      return makeScript();
    },
    head: {
      append(script) {
        scripts.push(script);
      },
    },
    querySelector(selector) {
      assert.equal(
        selector,
        "ins.kakao_ad_area[data-osb-adfit-placement]",
      );
      return hasAuthorizedUnit ? {} : null;
    },
    querySelectorAll(selector) {
      assert.equal(selector, "script[data-osb-kakao-adfit-loader]");
      return scripts.filter((script) => (
        script.hasAttribute("data-osb-kakao-adfit-loader")
      ));
    },
    scripts,
  };
}
