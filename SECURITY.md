# Security policy

OpenSoverignBlog is an early-stage project. Report vulnerabilities privately to
the repository owner rather than publishing exploit details in a public issue.

## Default trust model

- The server binds to loopback unless configured otherwise.
- Mutation endpoints are disabled without an administrator credential.
- Raw HTML, model output, imports, comments, and plugin output are untrusted.
- Markdown rendering is sanitized in the Rust publish pipeline.
- External embeds are typed resources, not arbitrary iframe HTML.
- Plugins do not receive direct database, filesystem, secret, or network access.
- Code execution is an optional remote runner client and never an in-process
  `eval`, shell command, Docker socket mount, or child process.
- Passive article HTML cannot initiate a third-party request: remote image
  sources are stripped, embeds are click facades, and the default CSP permits
  passive media only from the site itself. Any future consent-gated adapter
  must remain denied before DNS, preconnect, fetch, script, pixel, or iframe
  activity; the current reference server does not activate an ad provider.

## JavaScript and TypeScript contributions

Do not introduce dynamic code evaluation, untrusted MDX, inline event handlers,
unscoped `postMessage`, wildcard CORS, wildcard CSP sources, or secrets in the
browser bundle. Rendering via `dangerouslySetInnerHTML` is allowed only for a
server-sanitized publish artifact and still receives a client-side defensive
sanitization pass.

The CSP permits inline **style attributes** only (`style-src-attr`) because the
self-hosted KaTeX renderer generates positioning attributes. Inline scripts and
inline style elements remain blocked. Both the Rust sanitizer and DOMPurify
remove author-supplied `style` attributes before KaTeX runs; changing either
side of that three-part boundary requires a security regression test.

## Supported deployment boundary

The initial supported mode is a single Rust process, one local SQLite database,
and a local content-addressed blob directory. Reverse proxies must overwrite,
not append, trusted forwarding headers. Public deployments should use TLS and a
real external authentication adapter or a rotated high-entropy admin token.
