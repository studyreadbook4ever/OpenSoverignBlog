import type { EmbedReference } from "@opensoverignblog/sdk";

const YOUTUBE_ID = /^[A-Za-z0-9_-]{11}$/;
const X_STATUS_ID = /^[0-9]{1,30}$/;
const X_HANDLE = /^[A-Za-z0-9_]{1,15}$/;

export function socialEmbedFromUrl(raw: string, language: "ko" | "en" = "ko"): EmbedReference | undefined {
  let url: URL;
  try {
    url = new URL(raw.trim());
  } catch {
    return undefined;
  }
  if (url.protocol !== "https:" || url.username || url.password) return undefined;
  return youtubeEmbed(url, language) ?? xEmbed(url, language);
}

export function isYouTubeEmbed(embed: EmbedReference): boolean {
  return embed.provider === "youtube" && YOUTUBE_ID.test(embed.resourceId);
}

export function isXEmbed(embed: EmbedReference): boolean {
  return embed.provider === "x" && X_STATUS_ID.test(embed.resourceId);
}

/** Hydrates only the provider-owned facade after an explicit reader click. */
export function installSocialEmbedHydration(root: HTMLElement, language: "ko" | "en" = "ko"): () => void {
  const text = (ko: string, en: string) => language === "en" ? en : ko;
  const cleanups: Array<() => void> = [];
  for (const figure of root.querySelectorAll<HTMLElement>("figure.osb-embed")) {
    const provider = figure.dataset.osbProvider;
    const resourceId = figure.dataset.osbResourceId ?? "";
    const link = figure.querySelector<HTMLAnchorElement>("a.osb-embed-action");
    if (!link) continue;
    const originalLinkText = link.textContent;
    if (provider === "x" && X_STATUS_ID.test(resourceId)) {
      const alreadyRich = figure.classList.contains("osb-embed-rich-card");
      link.textContent = text("X에서 원문 보기", "View original on X");
      figure.classList.add("osb-embed-rich-card");
      cleanups.push(() => {
        link.textContent = originalLinkText;
        if (!alreadyRich) figure.classList.remove("osb-embed-rich-card");
      });
      continue;
    }
    if (provider !== "youtube" || !YOUTUBE_ID.test(resourceId)) continue;
    link.textContent = text("YouTube 동영상 불러오기", "Load YouTube video");
    const load = (event: Event) => {
      event.preventDefault();
      const frame = document.createElement("iframe");
      frame.className = "osb-youtube-frame";
      frame.src = `https://www.youtube-nocookie.com/embed/${resourceId}`;
      frame.title = figure.querySelector("figcaption")?.textContent?.trim() || text("YouTube 동영상", "YouTube video");
      frame.loading = "lazy";
      frame.referrerPolicy = "strict-origin-when-cross-origin";
      frame.allow = "accelerometer; autoplay; encrypted-media; gyroscope; picture-in-picture; web-share";
      frame.allowFullscreen = true;
      figure.replaceChildren(frame);
      figure.dataset.osbLoaded = "true";
    };
    link.addEventListener("click", load, { once: true });
    cleanups.push(() => {
      link.removeEventListener("click", load);
      link.textContent = originalLinkText;
    });
  }
  return () => cleanups.forEach((cleanup) => cleanup());
}

function youtubeEmbed(url: URL, language: "ko" | "en"): EmbedReference | undefined {
  const text = (ko: string, en: string) => language === "en" ? en : ko;
  const host = url.hostname.toLowerCase();
  let resourceId: string | undefined;
  if (host === "youtu.be") {
    resourceId = singlePathSegment(url.pathname);
  } else if (["youtube.com", "www.youtube.com", "m.youtube.com"].includes(host)) {
    if (url.pathname === "/watch") {
      resourceId = url.searchParams.get("v") ?? undefined;
    } else {
      const match = url.pathname.match(/^\/(?:shorts|embed)\/([A-Za-z0-9_-]+)\/?$/);
      resourceId = match?.[1];
    }
  }
  if (!resourceId || !YOUTUBE_ID.test(resourceId)) return undefined;
  return {
    id: `youtube-${resourceId}`,
    provider: "youtube",
    resourceId,
    canonicalUrl: `https://www.youtube.com/watch?v=${resourceId}`,
    title: text("YouTube 동영상", "YouTube video"),
    consentPurposeIds: ["external-media"],
  };
}

function xEmbed(url: URL, language: "ko" | "en"): EmbedReference | undefined {
  const text = (ko: string, en: string) => language === "en" ? en : ko;
  const host = url.hostname.toLowerCase();
  if (!["x.com", "www.x.com", "twitter.com", "www.twitter.com", "mobile.twitter.com"].includes(host)) {
    return undefined;
  }
  const match = url.pathname.match(/^\/([^/]+)\/status\/([^/]+)\/?$/);
  const handle = match?.[1];
  const resourceId = match?.[2];
  if (!handle || !resourceId || !X_HANDLE.test(handle) || !X_STATUS_ID.test(resourceId)) {
    return undefined;
  }
  return {
    id: `x-${resourceId}`,
    provider: "x",
    resourceId,
    canonicalUrl: `https://x.com/${handle}/status/${resourceId}`,
    title: text(`@${handle}의 X 게시물`, `X post by @${handle}`),
    consentPurposeIds: [],
  };
}

function singlePathSegment(pathname: string): string | undefined {
  const match = pathname.match(/^\/([^/]+)\/?$/);
  return match?.[1];
}
