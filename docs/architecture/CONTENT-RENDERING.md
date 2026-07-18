# Content and rendering pipeline

- Status: Implemented preview pipeline with documented limitations
- Last reviewed: 2026-07-18
- Scope: content acceptance, revision storage, browser projections, and passive
  network behavior

This document describes the renderer that exists now. It does not promise a
provider embed runtime, a persisted document AST, a persisted publish artifact,
or server-side KaTeX output. Those are explicit limitations below.

The canonical implementation and contracts are:

- [content and revision types](../../crates/kernel/src/content.rs);
- [content JSON Schema](../../schemas/content-envelope.v1.schema.json);
- [SQLite revision storage](../../crates/storage-sqlite/src/lib.rs);
- [Rust publish renderer](../../crates/renderer/src/lib.rs);
- [HTTP composition and public article page](../../apps/server/src/main.rs);
- [TypeScript reader and Studio](../../apps/web/src/main.tsx); and
- [framework-neutral TypeScript types](../../packages/sdk/src/index.ts).

## Current invariants

1. Every accepted revision has a `sourceMarkdown` value. The field may be an
   empty string, but it is never replaced by HTML, an ontology, an embed, or a
   generated artifact.
2. Author-intent HTML is optional and untrusted. Its presence does not remove
   the Markdown source.
3. Ontology data is an optional sidecar. It participates in revision identity
   but does not alter Markdown parsing or browser HTML in the current renderer.
4. External embeds are typed references. Article source cannot supply an
   iframe, script, or provider HTML fragment.
5. Raw HTML in portable Markdown is displayed as text. The separate intent
   layer is the only current HTML input, and it is allowlist-sanitized.
6. Sanitization occurs in Rust before an HTML artifact reaches a browser. The
   official TypeScript reader applies DOMPurify again as defense in depth.
7. Passive article content may load only same-origin images. Third-party media
   remains an inert, click-through facade unless a future host-owned adapter is
   authorized by the consent resource gate.
8. Source, revision, and rendered-artifact hashes are integrity identifiers,
   not signatures or proof of authorship.

## End-to-end data flow

```text
Studio, API client, or AI2AI proposal
       |
       | sourceMarkdown (required field)
       | intent? + ontology? + embeds?
       v
Rust content validation
       |
       v
immutable SQLite revision snapshot + contentHash
       |
       | publish selects a revision; rendering is currently on demand
       v
view selection
  +----+-------------------+----------------------+
  |                        |                      |
  v                        v                      v
intent                 markdown            markdown_source
  |                        |                      |
  | intent exists?         | pulldown event       | HTML escape exact
  | yes -> sanitize        | stream               | Markdown text
  | no  -> Markdown path   |                      |
  |                        +-- typed math          |
  +-- typed embed tokens   +-- typed embeds       |
  |                        +-- raw HTML as text    |
  +------------+-----------+                      |
               v                                  |
          Rust allowlist sanitizer <--------------+
               |
               v
PublishArtifact { html, hashes, renderer/policy versions, asset names }
               |
       +-------+----------------+
       |                        |
       v                        v
standalone article          official React reader
HTML + strict CSP           DOMPurify + optional SPA KaTeX
```

The HTTP API returns the selected `PublishArtifact` together with the revision
identifier and portable Markdown. The public article route renders the same
Rust artifact into a small HTML shell. See the handlers and response types in
the [server composition](../../apps/server/src/main.rs).

## Content and revision model

The required and optional fields are defined in
[`RevisionSnapshot`](../../crates/kernel/src/content.rs):

- `source_markdown` is always present;
- `intent` is an optional `IntentLayer` containing a format identifier,
  untrusted source HTML, optional renderer hints, and optional provenance;
- `ontology` is an optional `OntologySidecar` containing a schema identifier
  and statements;
- `embeds` is a list of typed `EmbedReference` values; and
- `actor`, parent revision, revision number, timestamps, and identifiers retain
  revision history independently of the rendered HTML.

Content validation limits Markdown and intent HTML to 10 MiB each, limits the
ontology statement count, and validates unique embed identifiers, provider
identifiers, canonical HTTP(S) links, and bounded metadata. Runtime validation
is implemented by the Rust types; the JSON Schema is the external contract.
Keeping Rust serialization, the schema, and TypeScript definitions in sync is
an open contract-testing item recorded in the
[requirements matrix](../REQUIREMENTS.md).

The [SQLite adapter](../../crates/storage-sqlite/src/lib.rs) stores the complete
revision as JSON and never overwrites an accepted snapshot. Publication points
the document to one revision. The current implementation generates browser
artifacts when they are requested; publication does not yet persist a renderer
version and immutable HTML artifact.

## Projection behavior

### Author-intent view

`view=intent` is the default. When the revision has an intent layer, the
renderer:

1. treats `sourceHtml` as untrusted;
2. applies the Rust element, attribute, and URL allowlist;
3. replaces recognized `{{embed:id}}` text placeholders with host-generated
   link facades; and
4. sanitizes the combined result again.

Scripts, event handlers, iframes, active SVG, style elements, inline styles,
objects, and unsupported URL schemes do not survive. `rendererHints` and intent
provenance are stored today but are not interpreted by the renderer.

If an intent layer is absent, `view=intent` deliberately falls back to the
rendered Markdown projection. It never yields an empty page merely because the
optional layer is absent.

### Markdown view

`view=markdown` renders Markdown through `pulldown-cmark` with strikethrough,
tables, footnotes, task lists, heading attributes, and math events enabled. Raw
HTML and inline HTML events become text before HTML generation. Typed math and
embed events are handled as described below, and the generated HTML is then
sanitized.

This rendered Markdown projection is available through the API. The official
human-facing switch intentionally exposes the author-intent view and the
literal `.md` source view rather than presenting two visually similar rendered
HTML modes.

### `.md` source view

`view=markdown_source` escapes the exact Markdown and places it inside
`<pre><code>`. Neither Markdown syntax nor embedded HTML is interpreted. The
dedicated `source.md` endpoint also returns the original source with a
`text/markdown` media type.

The standalone article page and the React reader both offer an **Author
intent** / **Markdown source** choice. Switching projections does not create a
revision or change publication state.

## Typed embeds

An embed consists of an identifier, provider identifier, provider resource
identifier, canonical link, accessible title, and zero or more consent-purpose
identifiers. The Markdown invocation is a line whose text is:

```text
::osb-embed example-id
```

The intent-layer equivalent is:

```text
{{embed:example-id}}
```

When the identifier matches the revision sidecar, the Rust renderer emits a
`figure`, `figcaption`, and ordinary external link. It does not emit an iframe,
pixel, preconnect, provider script, or background request. An unknown or
missing identifier stays inert and portable rather than being guessed from a
URL.

The canonical external link can be followed only through a user navigation.
Provider hydration is **not implemented**. A future adapter that replaces a
facade with external media must be host-controlled, declare every resource,
and receive authorization from the
[monetization resource gate](../../features/monetization-policy/src/lib.rs)
under the [EU consent profile](../legal/EU-CONSENT.md). Article text alone can
never grant that capability.

## Math

Inline and display math events become inert source-bearing elements with
`osb-math-inline` or `osb-math-display` classes. They pass through the same Rust
sanitizer as the rest of the generated artifact. Math source is escaped; it is
not accepted as HTML, SVG, or MathML.

The official React reader finds those elements and invokes its bundled KaTeX
with `trust: false`. KaTeX JavaScript, CSS, and fonts are part of the local web
build rather than fetched from a CDN. DOMPurify runs before this host-owned math
rendering step, so user markup does not gain KaTeX's output permissions.

The standalone article route does **not** currently run KaTeX and does not
currently guarantee that the declared `katex.css` asset is served there. It
shows the safe math-source placeholder. `requiredStyleAssets` records the
renderer expectation; it is not proof that every consuming frontend has
fulfilled it.

## Trust boundaries

| Boundary | Input trust | Current enforcement |
| --- | --- | --- |
| Content acceptance | Human, pasted, imported, model, and plugin content is untrusted | Rust size, identifier, URL, and structure validation in the content types |
| Markdown parser | Markdown text is untrusted | Raw HTML events are converted to text; only recognized typed events receive host markup |
| Intent parser | Intent HTML is untrusted, including administrator or LLM output | Rust allowlist sanitizer before and after typed embed insertion |
| Renderer output | Host-generated but still treated defensively | The generated HTML passes through the same sanitizer and carries renderer/sanitizer versions |
| Official browser | Server artifact may be stale, corrupted, or supplied through a changed integration | DOMPurify blocks active tags and style attributes before the HTML sink; the server supplies CSP and other security headers |
| Math renderer | KaTeX is host code; math source remains untrusted | Self-hosted bundle, `trust: false`, no permission transfer to author markup |
| External provider | Third-party media and scripts are untrusted and privacy-relevant | Link-only facade today; no provider hydration runtime is mounted |

The use of `dangerouslySetInnerHTML` in the
[React reader](../../apps/web/src/main.tsx) is restricted to the Rust-sanitized
artifact after a second DOMPurify pass. It is not a general content or plugin
API.

## Browser network invariant

Rendering an article must not create an undeclared third-party request.
Currently this is enforced in two layers:

1. The Rust sanitizer retains an image `src` only when it is a safe relative
   URL resolving to the first-party origin model. Absolute HTTP(S),
   scheme-relative, backslash, control-character, and other external image
   values lose their `src`.
2. The reference server's Content Security Policy limits images to `'self'`
   and `data:`, connections to `'self'`, scripts/styles/fonts to the declared
   first-party policy, and frames through `default-src 'none'`. Article content
   cannot produce a data-image source under the current sanitizer even though
   `data:` remains in the outer CSP for host UI use.

Consequences:

- first-party article images must use relative URLs;
- current storage does not yet provide a user-asset upload pipeline, so a
  relative URL is useful only when the operator separately serves that asset
  from the same origin;
- third-party images, video, audio, iframes, pixels, scripts, and SDKs cannot be
  passive article markup;
- external media is represented by a click-through typed facade; and
- a future click-to-load or automatic provider adapter must remain blocked
  until its declared purposes and resources are authorized.

A detachable frontend is part of this security boundary. It must preserve the
same-origin passive-resource rule and an equivalent CSP; consuming sanitized
HTML while weakening the host CSP is not a supported security profile.

## Hashes and provenance

The revision `contentHash` is SHA-256 over the content schema version, title,
slug, Markdown, serialized embed sidecar, optional intent layer, and optional
ontology sidecar, with separators. It deliberately does not include revision
identity, actor, or timestamps. Changing an optional layer therefore changes
the content identity without making that layer mandatory for other posts.

Each on-demand `PublishArtifact` contains:

- `sourceHash`, identifying the selected projection input;
- `artifactHash`, SHA-256 of the final sanitized HTML;
- `rendererVersion`;
- `sanitizerPolicyVersion`; and
- declared style and script asset names.

For rendered Markdown and Markdown source, `sourceHash` covers the Markdown
bytes. For an explicit intent layer it covers the raw intent HTML. When the
intent view falls back to Markdown, the current implementation uses the full
revision `contentHash`. Consumers must inspect the `view` and revision ID and
must not assume all source hashes have the same domain.

Revision provenance remains in the revision actor and optional intent
provenance. Ontology statements carry evidence and an author-confirmation flag;
embed sidecars carry provider and consent-purpose identifiers. The public
artifact does not currently inline all of that provenance. None of these
hashes authenticates an actor, proves legal consent, or replaces a signature.

## Clean-room, Velog-inspired decisions

The project adopts behaviors, not Velog implementation code. The pinned
[clean-room research note](velog-clean-room.md) is the provenance authority.
No Velog source, CSS, assets, fixtures, or component hierarchy are used here.

Independently implemented decisions inspired by the product problem include:

- separating portable Markdown, author intent, structured sidecars, and
  browser projections;
- treating math and embeds as typed constructs rather than broad HTML
  permissions;
- using a central server-side sanitizer instead of trusting each client;
- retaining a readable fallback when a rich extension is absent; and
- allowing readers to inspect the author-intent and `.md` projections of the
  same revision.

The current implementation explicitly rejects raw-HTML-by-default Markdown,
executable MDX/JSX, `eval`, HTML-string iframe converters, content-controlled
scripts, client-only sanitization, and broad user SVG/style permissions.

## Current limitations

- Rendering uses a parser event stream; there is no persisted, versioned AST.
- Rendered publish artifacts are generated on demand and are not immutable
  database records tied to the publication event.
- Provider embed hydration and its consent lifecycle are not implemented.
- The standalone article page does not produce fully typeset KaTeX output.
- Intent renderer hints are stored but ignored.
- Ontology is stored, returned, hashed, and exported, but is not projected into
  browser HTML or a dedicated reader view.
- User asset upload, content-addressed blob storage, image processing, and
  asset export are not implemented.
- The official Studio does not yet provide a live render preview or full
  revision-review workflow.
- Artifact hashes are integrity checks, not signatures, transparency-log
  entries, or long-term render reproducibility guarantees.

These gaps are tracked in the
[requirements traceability matrix](../REQUIREMENTS.md). Documentation and
capability discovery must continue to report them as unavailable or partial
until end-to-end implementations and tests exist.
