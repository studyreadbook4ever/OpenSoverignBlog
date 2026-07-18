# ADR 0002: Community control plane and read-optimized delivery plane

- Status: Accepted
- Date: 2026-07-19
- Scope: Multi-user blogs, authoring, comments, themes, and public delivery

## Context

OpenSoverignBlog must support more than a single owner without turning public
reading into a session-dependent application. People need to sign in, create
their own blog, write and revise posts, publish an exact immutable revision,
and comment on other published posts. Operators must also be able to deploy a
public surface that has no mutation capability and is safe to cache heavily.

“Read-only” therefore describes the public delivery plane, not the whole
product. Authentication, authoring, moderation, and publication belong to a
writable control plane. Published pages must remain available while a new
draft revision is being edited.

The editor may learn from externally observable publishing workflows such as
a focused Markdown input, live preview, and a separate publication step. It
must remain an independent implementation and follow the repository's Velog
clean-room policy.

## Decision

### One content kernel, two operational planes

The immutable content and rendering kernel is shared by both planes.

The control plane owns:

- local or externally verified identities and revocable sessions;
- blog memberships and resource-scoped authorization;
- private drafts and immutable revision history;
- preview compilation, asset upload, and explicit publication;
- authenticated comments and moderation;
- blog appearance revisions.

The delivery plane exposes only public blog profiles, feeds, published post
projections, approved comments, immutable assets, and discovery metadata. Its
responses never vary by session. A delivery-only process does not mount
registration, login, Studio, upload, comment submission, or other mutation
routes.

An operator may initially run both planes in one process. The HTTP and storage
boundaries must still permit a later deployment in which the control plane
publishes a database/export snapshot consumed by one or more read-only
delivery instances.

### Publication continuity

`current_revision_id` and `published_revision_id` have distinct meanings.
Creating a draft after publication advances only the current revision. The
previous published revision remains in feeds and at its public route until a
new exact revision is explicitly published or the document is archived.

Public queries use the published revision and published blog-appearance
revision exclusively. Draft timestamps, draft titles, and draft slugs must not
affect public cache validators.

### Public caching

Public list and slug routes emit deterministic ETags and shared-cache policy.
Immutable assets and revision-addressed artifacts may use a one-year immutable
policy. Mutable aliases, feeds, blog profiles, and approved-comment lists use
short shared TTLs with stale-while-revalidate.

Session, authentication, Studio, moderation, and all mutation responses are
`private, no-store`. Public endpoints do not set cookies. Comment validators
are separate from article validators so a new comment does not invalidate the
article artifact.

### Identity and authorization

Local passwords are hashed with Argon2id and stored as PHC strings. Browser
sessions use opaque, CSPRNG-generated credentials; only a SHA-256 digest is
stored server-side. Cookies are HttpOnly, SameSite, path-scoped, expiring, and
Secure when served over HTTPS.

Authentication establishes a user. Authorization is always resolved from
persisted membership and the exact blog/document scope. Knowing another
document UUID never grants draft, revision, publication, asset, or moderation
access. OAuth/OIDC providers may later map a verified external subject to the
same internal user model and cannot bypass membership policy.

### Blogs and themes

An initial blog is a site plus an owner membership and an immutable appearance
revision. The setup flow collects a stable handle, display name, description,
and a first-party theme preset.

The shared-hosting profile accepts only versioned, allowlisted theme presets.
The dedicated on-premise profile may additionally enable bounded owner CSS.
That CSS is stored as an immutable appearance revision, rejects at-rules,
network loaders, HTML delimiters, escapes, and malformed blocks, and is served
from a same-origin `text/css` endpoint wrapped in an exact site-root `@scope`.
The public API exposes only the stylesheet URL; it never places raw owner CSS
in an inline style element. Disabling the capability removes the endpoint and
the Studio control.

### Comments

Only authenticated users can submit comments in the initial community
profile. The server derives author, blog, and document identifiers from the
session and the published document; clients cannot assert them. Comment
Markdown passes the same validation and sanitization boundary as other
untrusted content. Public routes return approved comments only.

### Editor workflow

The default editor prioritizes the common path:

1. title and portable Markdown;
2. formatting toolbar and preview compiled by the publication renderer;
3. local draft status;
4. a separate publication review containing slug and exact revision;
5. explicit publish.

On wide screens, input and preview may be shown side by side. On narrow
screens, they are equally reachable tabs. Intent HTML, ontology, embed JSON,
memory scopes, paste receipts, and revision diagnostics remain available under
advanced controls rather than competing with basic writing.

## Compatibility profiles

The reference composition supports three explicit profiles:

- `delivery_only`: public, cacheable reads with no mutation surface;
- `single_owner_token`: the original operator-token workflow;
- `authenticated_members`: sessions, per-blog membership, Studio, themes, and
  comments.

Capabilities describe only a profile that is actually configured and
operational. Hiding a control in the browser is never treated as an
authorization boundary.

## Consequences

- SQLite remains sufficient for a single writable control-plane owner and
  replicated/exported delivery snapshots.
- Public availability no longer depends on the current draft state.
- Multi-user support adds durable identity, owner/editor/writer membership,
  session, theme, and comment migrations plus cross-tenant authorization
  tests.
- Dedicated-origin owner CSS is intentionally conservative and site-scoped;
  general shared-hosting CSS remains outside the supported trust profile.
- The Studio becomes simpler for ordinary writing while advanced sovereign
  controls remain available.
- Cache, session, and authorization behavior become release gates rather than
  optional optimizations.
