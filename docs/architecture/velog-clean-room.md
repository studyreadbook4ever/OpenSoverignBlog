# Velog Clean-Room Reference

Status: Accepted research constraint
Reviewed: 2026-07-18
Applies to: OpenSoverignBlog core, renderer, editor, and extension SDKs

## Purpose

This note records what OpenSoverignBlog may learn from the public Velog repository without importing Velog implementation code. It is both an architecture reference and a provenance rule for human contributors and AI coding agents.

The review is pinned to Velog commit [`63dcace01e1c87b35a38027f681d9e7c0f210467`](https://github.com/velog-io/velog/commit/63dcace01e1c87b35a38027f681d9e7c0f210467), authored on 2025-04-13. Statements in this note describe that public snapshot, not necessarily the production service at `velog.io` or later repository revisions.

No Velog source code, CSS, assets, test fixtures, documentation text, or screenshots are copied into this document or into the implementation as a result of this review.

## Repository and license findings

The reviewed Velog repository is a TypeScript pnpm/Turborepo monorepo. Its public tree contains Next.js web applications, Fastify/Mercurius servers, cron jobs, Prisma database packages, a book-oriented Markdown editor package, and AWS infrastructure code. The root README provides only a short product description, while the public post route and viewer contain placeholders in the reviewed commit. The snapshot therefore must not be treated as a complete, production-equivalent, drop-in self-hosting distribution.

The repository root is licensed under the MIT License, with copyright held by Chaf, Inc. The nested `packages/markdown-editor` directory carries a separate MIT notice with copyright held by Shu Ding and retains Nextra-derived documentation and structure.

Consequences for this Unlicense project:

- Architecture ideas and externally observable behavior may be independently reimplemented.
- Velog implementation text must not be pasted, translated between programming languages, mechanically transformed, or used as a scaffold.
- Velog CSS, visual assets, fixtures, snapshots, and documentation prose are subject to the same no-copy rule.
- If a future change intentionally incorporates Velog material, it must be isolated as third-party material and retain every applicable MIT copyright and permission notice. It must not be represented as material whose original authors dedicated it under OpenSoverignBlog's Unlicense.
- The nested Markdown editor has its own provenance boundary; the root notice alone is not sufficient for material taken from that directory.
- Package dependencies used through their public APIs retain their own licenses and must be recorded in the dependency inventory or SBOM.
- Velog names, logos, and branding are outside the clean-room target and must not be reused.

This is an engineering provenance policy, not legal advice.

## Snapshot limitations

The reviewed repository reveals useful rendering and data-model patterns, but it does not establish that the same code is currently used for every production path. In particular:

- the public post layout calls the framework's not-found path;
- the public post viewer is a placeholder;
- the root documentation does not provide an end-to-end self-hosting procedure;
- the server is coupled to services including PostgreSQL, Redis, Elasticsearch, and several hosted integrations;
- the post API stores an opaque body string plus flags rather than a portable typed content model.

OpenSoverignBlog may use the snapshot to define behavioral requirements, but not to claim source or production compatibility with Velog.

## Patterns adopted by independent implementation

### Staged Markdown pipeline

Velog demonstrates the value of decomposing rendering into parsing, syntax features, code highlighting, embeds, mathematics, HTML generation, and final filtering. OpenSoverignBlog adopts the staged-pipeline idea, with independently designed Rust and TypeScript interfaces.

The OpenSoverignBlog pipeline is:

```text
immutable source
  -> typed document AST
  -> extension transforms
  -> policy validation
  -> publish AST
  -> deterministic render artifacts
  -> HTML, Markdown, and machine-readable projections
```

The immutable source, typed AST, and rendered artifacts are distinct records. A publish artifact includes at least its source revision, renderer version, security-policy version, enabled extension set, and content hash. Public renderers consume publish artifacts; they do not improvise a new interpretation of an untrusted body string.

Preview and publish use the same compiler and security policy. The editor may schedule compilation with cancellation, debouncing, and worker isolation, but its scheduling algorithm will be implemented independently rather than copied from Velog's adaptive throttle.

### Markdown-first authorship

Markdown remains the portable default source format. Optional ontology data, AI annotations, and rich author intent live in typed sidecars or explicit extension nodes. They do not make the basic `.md` projection unreadable or mandatory.

Imported or LLM-generated HTML may be retained as provenance-bearing source material, but it is never implicitly trusted and is not the sole canonical representation of a post.

### Typed mathematics

Inline and block mathematics are represented as dedicated AST nodes containing source notation and rendering options. A deterministic math renderer produces the accessible output artifact.

The important separation is provenance:

- author-provided HTML and SVG remain untrusted;
- math source is parsed under explicit limits;
- markup produced by the selected math renderer is treated as renderer output, not as permission for matching author-provided tags;
- stylesheets and fonts are self-hosted in the default sovereign distribution.

This avoids using a broad math allowlist to legalize arbitrary user-authored SVG, MathML attributes, or inline styles.

### Typed embed providers

Velog's fixed set of embed providers and hostname restriction motivate a provider registry, but OpenSoverignBlog does not generate embeds by interpolating HTML strings.

An embed node carries structured data such as provider identifier, resource identifier, validated parameters, and privacy mode. Its provider manifest declares:

- accepted identifier syntax;
- allowed origins and URL schemes;
- required iframe sandbox tokens;
- CSP additions;
- referrer policy;
- consent category and whether network access is blocked before consent;
- fallback link or static facade behavior;
- optional scripts, loaded by the host rather than by article content;
- accessibility title requirements;
- cache and failure behavior.

Unknown providers remain inert, portable nodes. They do not become arbitrary HTML. Third-party embeds default to a click-to-load facade and must respect the site's consent decision.

### URL history as a redirect ledger

Velog retains prior slugs and can find a post through an earlier URL. OpenSoverignBlog independently adopts this product behavior as an SEO/URL module.

The redirect ledger records the stable content identifier, prior route, canonical route, status code, change timestamp, actor, and reason. Route resolution is deterministic and detects loops. Removing the optional SEO module must not make content unreadable through its stable identifier.

### Source and projection separation

Velog's Markdown-versus-HTML flag shows the need to preserve source-format intent, but a single body string and Boolean flag are not adopted. OpenSoverignBlog instead exposes independently versioned projections:

- author-intent source;
- portable Markdown view;
- typed AST view for trusted tools and AI2AI clients;
- sanitized HTML artifact for browsers;
- plain-text/search projection.

Every projection identifies the revision from which it was derived. A user may switch between the author-intent view and portable Markdown view without changing the underlying publication.

## Patterns explicitly not adopted

### Client-only or renderer-local sanitization

The reviewed server write path removes null bytes from a post body but does not establish a central server-side HTML sanitization boundary. Safety then depends on every consumer rendering the body correctly. OpenSoverignBlog rejects this model.

Sanitization and render-policy enforcement occur in the core publication pipeline. Adapters receive a safe artifact or structured source with an explicit trust label; they never silently assume that a database body is safe HTML.

### Raw HTML enabled by default

The reviewed Markdown renderer enables dangerous HTML parsing and later filters the generated HTML. OpenSoverignBlog keeps raw HTML disabled in its portable default profile.

An installation may enable a restricted HTML capability for trusted authors. Even then, the fragment is parsed, normalized, and sanitized under a named, versioned profile. The raw fragment cannot add scripts, event handlers, forms, active SVG, style-based overlays, unsupported URL schemes, or arbitrary frames.

### Velog's HTML allowlist

The reviewed filter combines a narrow regular expression for event attributes with broad attribute and style permissions needed by its KaTeX output. Math and SVG tags can receive wildcard attributes, and several style properties accept unrestricted values. This is not a suitable security boundary and must not be reproduced.

OpenSoverignBlog uses parser-level validation rather than regular-expression stripping. Allowances are element-specific, value-specific, and provenance-specific. URL-bearing attributes are normalized before scheme and origin checks. User SVG is disabled by default and, if ever enabled, uses a separate restrictive profile.

### Divergent preview and public rendering

The reviewed component sends filtered HTML to its public `dangerouslySetInnerHTML` path while its edit path parses the unfiltered generated HTML. OpenSoverignBlog requires preview/publish parity. A preview may display extra provenance or warnings, but it cannot use a weaker content policy.

### HTML-string embed converters

Provider identifiers must not be concatenated into iframe or blockquote strings. In the reviewed code, the intended CodeSandbox `sandbox` attribute and CodePen accessibility metadata are not present in the final allowed iframe attributes, while a Twitter script is dynamically loaded when matching markup is found. These mismatches are avoided by validating a typed provider manifest as a single unit.

Article content cannot request arbitrary script execution. Consent-gated scripts are owned, deduplicated, integrity-checked where applicable, and lifecycle-managed by the host runtime.

### Executable MDX for untrusted content

The nested Velog Markdown editor compiles MDX with `next-mdx-remote` version 5 and renders the compiled source. The upstream version-5 documentation states that the library evaluates JavaScript in the browser, warns about XSS, and says not to pass user input to the renderer.

OpenSoverignBlog does not use MDX, JSX, `eval`, or `new Function` as its public content extension mechanism. Human, imported, plugin-generated, and LLM-generated content are all untrusted. Interactive features use declarative nodes resolved through a capability-limited component registry. Any future trusted-code mode must be a visibly separate plugin executed outside the content renderer with an explicit administrator grant.

### Production architecture cloning

Velog's application topology, database schema, service classes, GraphQL schema, React component hierarchy, and AWS infrastructure are not templates for OpenSoverignBlog. The project adopts its own Rust core, TypeScript UI, SQLite-first persistence, and capability-oriented extension contracts.

## Security requirements derived from the review

The following requirements are normative:

1. Human-authored, pasted, imported, LLM-generated, and plugin-generated material is untrusted by default.
2. Raw HTML is disabled unless a named policy explicitly enables a restricted subset.
3. Publication produces immutable, policy-versioned artifacts; public rendering does not consume arbitrary source HTML.
4. Preview and publish share the same parser, transforms, sanitizer, and embed policy.
5. User markup cannot create script elements, event-handler attributes, active SVG, unrestricted inline styles, forms, or arbitrary iframes.
6. Math-renderer output and user-provided markup have distinct provenance and allowlists.
7. Embeds are typed nodes, enforce sandbox and origin restrictions, and perform no third-party request before the required consent.
8. Content-controlled scripts are forbidden. Host-controlled scripts are declared by signed or locally approved extension manifests.
9. Browser renderers ship a restrictive CSP. The TypeScript UI should use Trusted Types where supported and avoid ad hoc HTML sinks.
10. Sanitizer and renderer releases include adversarial fixtures, property-based tests, URL canonicalization tests, and preview/publish parity tests.
11. Stored source remains exportable even when an extension is unavailable; unknown nodes render as safe inert fallbacks.
12. AI2AI clients receive explicit content type, trust, provenance, policy version, and available projection metadata rather than guessing from a string.

## Clean-room workflow for contributors and AI agents

When implementing a feature inspired by Velog:

1. Read this document and the externally observable requirement, not Velog implementation files.
2. Express the behavior as an OpenSoverignBlog issue, interface contract, or acceptance test without Velog-specific identifiers or structure.
3. Implement against OpenSoverignBlog's architecture using independently selected algorithms and dependencies.
4. Do not ask an AI agent to port, translate, simplify, or restyle a Velog file.
5. Do not paste Velog source into prompts, comments, tests, commit messages, or generated documentation.
6. Record the behavioral inspiration in the pull request and link this document.
7. If implementation-level comparison becomes necessary for security research, keep the researcher separate from the implementer and communicate only findings and requirements.
8. Run a provenance review before merge. Suspiciously similar naming, control flow, comments, CSS values, fixtures, or component structure must be rewritten or removed.

An AI agent working in this repository should receive the concise constraint: **implement the documented behavior independently; do not retrieve or reproduce Velog source code.**

## Primary sources

All Velog links below are pinned to the reviewed commit.

- [Velog repository](https://github.com/velog-io/velog)
- [Reviewed commit](https://github.com/velog-io/velog/commit/63dcace01e1c87b35a38027f681d9e7c0f210467)
- [Root MIT license](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/LICENSE)
- [Nested Markdown editor MIT license](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/packages/markdown-editor/LICENSE)
- [Workspace structure](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/pnpm-workspace.yaml)
- [Main Markdown renderer](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/web/src/components/MarkdownRender/MarkdownRender.tsx)
- [HTML filter](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/web/src/components/MarkdownRender/utils.ts)
- [Embed transform](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/web/src/lib/remark/embedPlugin.ts)
- [KaTeX allowlist](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/web/src/lib/katexWhiteList.ts)
- [KaTeX stylesheet loading](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/web/src/app/layout.tsx)
- [Post GraphQL model](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/server/src/graphql/Post.gql)
- [Post write path](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/server/src/services/PostApiService/index.mts)
- [Database models including URL history](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/packages/database/prisma/velog-rds/schema.prisma)
- [Historical-slug lookup](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/server/src/services/PostService/index.ts)
- [Nested MDX compiler](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/packages/markdown-editor/src/mdx-compiler.ts)
- [Nested MDX preview](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/packages/markdown-editor/src/components/markdown-preview/markdown-preview.tsx)
- [Placeholder post layout](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/web/src/app/%5Busername%5D/%5BurlSlug%5D/layout.tsx)
- [Placeholder post viewer](https://github.com/velog-io/velog/blob/63dcace01e1c87b35a38027f681d9e7c0f210467/apps/web/src/features/post/components/PostViewer/PostViewer.tsx)

Relevant upstream primary documentation:

- [remark-rehype: safe handling of embedded HTML](https://github.com/remarkjs/remark-rehype#example-supporting-html-in-markdown-properly)
- [sanitize-html 2.11.0 allowlist behavior](https://github.com/apostrophecms/sanitize-html/blob/2.11.0/README.md)
- [`next-mdx-remote` 5.0.0 security warning](https://github.com/hashicorp/next-mdx-remote/tree/v5.0.0#security)

## Decision

Velog is a behavioral and UX research reference, not a source-code base for OpenSoverignBlog. We adopt the general concepts of a staged Markdown renderer, typed mathematics, constrained embed providers, and historical URL resolution. We independently implement them behind a deterministic Rust content kernel and reject the reviewed raw-HTML, sanitizer, HTML-string embed, and executable-MDX security boundaries.
