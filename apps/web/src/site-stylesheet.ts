export function safeBlogStylesheetUrl(
  href: string | undefined,
  handle: string,
  origin: string,
  applicationBasePath: string,
): string | undefined {
  if (!href || href.includes("\\") || /[\u0000-\u001f]/.test(href)) return undefined;
  try {
    const candidate = new URL(href, origin);
    const prefix = applicationBasePath === "/" ? "" : applicationBasePath;
    const expected = new URL(
      `${prefix}/api/v1/blogs/${encodeURIComponent(handle)}/custom.css`,
      origin,
    );
    return candidate.origin === expected.origin
      && (candidate.protocol === "http:" || candidate.protocol === "https:")
      && candidate.username === ""
      && candidate.password === ""
      && candidate.href === expected.href
      ? candidate.toString()
      : undefined;
  } catch {
    return undefined;
  }
}
