import type {
  AdvertisingCapabilities,
  AdvertisingPlacement,
  AdvertisingUnitDescriptor,
  AdvertisingViewport,
} from "@opensoverignblog/sdk";

export const KAKAO_ADFIT_SCRIPT_URL =
  "https://t1.kakaocdn.net/kas/static/ba.min.js" as const;

const UNIT_ID_PATTERN = /^DAN-[A-Za-z0-9_-]{4,124}$/;
const SCRIPT_MARKER = "data-osb-kakao-adfit-loader";
const RESERVED_ROOTS = new Set([
  ".well-known",
  "ai2ai.md",
  "agent.txt",
  "ads.txt",
  "agents.txt",
  "api",
  "assets",
  "blog",
  "custom.css",
  "docs",
  "favicon.svg",
  "healthz",
  "index.html",
  "index.css",
  "livez",
  "llms.txt",
  "login",
  "media",
  "onboarding",
  "openapi",
  "providers",
  "readyz",
  "references",
  "robots.txt",
  "schemas",
  "sitemap.xml",
  "studio",
  "unlicense",
  "vendor",
]);

export function isAdvertisingReaderPath(pathname: string): boolean {
  const path = normalizePath(pathname);
  if (!path) return false;
  if (path === "/" || path === "/index.html" || path === "/references") return true;
  if (path === "/login" || path === "/onboarding" || path === "/studio") return false;
  if (path.startsWith("/studio/")) return false;

  if (/^\/@[^/]+(?:\/[^/]+){0,2}$/.test(path)) return true;
  if (/^\/blog\/[^/]+$/.test(path)) return true;

  const segments = path.slice(1).split("/");
  if (
    segments.length < 1
    || segments.length > 2
    || segments.some((segment) => !segment)
    || RESERVED_ROOTS.has((segments[0] ?? "").toLowerCase())
  ) {
    return false;
  }
  return !segments[0]?.startsWith("@");
}

export function isConfirmedAdvertisingReaderContent(
  pathname: string,
  contentReady: boolean,
): boolean {
  return contentReady && isAdvertisingReaderPath(pathname);
}

export function isSupportedAdvertising(
  advertising: AdvertisingCapabilities | undefined,
): advertising is AdvertisingCapabilities {
  return Boolean(
    advertising
    && advertising.provider === "kakao-adfit"
    && advertising.scriptUrl === KAKAO_ADFIT_SCRIPT_URL
    && advertising.consent?.required === true
    && advertising.consent.statusHref === "/api/v1/advertising/consent"
    && advertising.consent.actionHref === "/api/v1/advertising/consent"
    && advertisingUnitFor(advertising, "top", "pc")
    && advertisingUnitFor(advertising, "top", "mobile")
    && advertisingUnitFor(advertising, "bottom", "pc")
    && advertisingUnitFor(advertising, "bottom", "mobile"),
  );
}

export function advertisingUnitFor(
  advertising: AdvertisingCapabilities,
  placement: AdvertisingPlacement,
  viewport: AdvertisingViewport,
): AdvertisingUnitDescriptor | undefined {
  const candidate = advertising.placements?.[placement]?.[viewport];
  const expected = viewport === "pc"
    ? { width: 728, height: 90 }
    : { width: 320, height: 100 };
  if (
    !candidate
    || candidate.width !== expected.width
    || candidate.height !== expected.height
    || !UNIT_ID_PATTERN.test(candidate.unitId)
  ) {
    return undefined;
  }
  return candidate;
}

/**
 * Loads the one official provider script only after React has committed at
 * least one consent-authorized unit. The returned cleanup owns only the script
 * created by this call.
 */
export function installKakaoAdFitLoader(
  targetDocument: Document,
  scriptUrl: string,
): () => void {
  if (
    scriptUrl !== KAKAO_ADFIT_SCRIPT_URL
    || !targetDocument.querySelector(
      "ins.kakao_ad_area[data-osb-adfit-placement]",
    )
  ) {
    return () => undefined;
  }

  for (const stale of targetDocument.querySelectorAll<HTMLScriptElement>(
    `script[${SCRIPT_MARKER}]`,
  )) {
    stale.remove();
  }
  const existing = Array.from(targetDocument.scripts).find(
    (script) => script.src === scriptUrl,
  );
  if (existing) return () => undefined;

  const script = targetDocument.createElement("script");
  script.async = true;
  script.charset = "utf-8";
  script.src = scriptUrl;
  script.type = "text/javascript";
  script.setAttribute(SCRIPT_MARKER, "true");
  targetDocument.head.append(script);
  return () => script.remove();
}

function normalizePath(pathname: string): string | undefined {
  const withoutQuery = pathname.split(/[?#]/, 1)[0] || "/";
  if (!withoutQuery.startsWith("/") || withoutQuery.startsWith("//")) return undefined;
  if (withoutQuery === "/") return "/";
  return withoutQuery.replace(/\/+$/, "") || "/";
}
