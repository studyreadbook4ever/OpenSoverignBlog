# ADR 0001: Modular kernel, local-first runtime, and extension boundary

> Redis and default on-premise operations in this original baseline are
> superseded by ADR 0003. SQLite/blob ownership remains unchanged.

- Status: Accepted
- Date: 2026-07-18
- Scope: Initial architecture and compatibility contract

## Context

The project is an on-premise, AI-native publishing engine that can run as a
standalone blog or attach to an existing website. It must remain easy for one
person to install and operate, while allowing the editor, renderer, AI
providers, comments, SEO, advertising, and automation to evolve independently.

The initial product constraints are:

- a Rust core and TypeScript user interfaces;
- SQLite-first operation on one host;
- Markdown as the portable authoring source;
- optional ontology data rather than a mandatory semantic model;
- normalized AI-to-AI messages and artifacts rather than provider-specific
  prompt blobs;
- optional authentication providers, RBAC, comments, SEO, advertising, and
  code execution;
- public extension contracts that do not depend on Rust's unstable dynamic
  library ABI;
- an Unlicense clean-room codebase.

Process count is not a useful measure of modularity. The initial deployment
should be small, while source and contract boundaries must make later process
extraction possible.

## Decision

We will build a modular kernel in a monorepo and ship it initially as one Rust
server process, plus an optional isolated code runner. The server owns SQLite
and runs short background jobs in the same process. The TypeScript admin
application is built to static assets and may be embedded in the server image,
so Node.js is not required in the default production deployment.

The public website may use any of three integration modes:

1. consume the versioned content HTTP API;
2. use the official TypeScript renderer or static exporter;
3. reverse proxy an official renderer under a configurable base path.

Direct access to the engine database is never an integration contract.

An indicative deployment is:

    Existing website or official renderer
                    |
             HTTP content API
                    |
       Rust server + embedded admin UI
          |          |            |
       SQLite    local blobs   plugin host
                                  |
                         optional isolated runner

## Workspace and dependency boundaries

The intended workspace is:

    apps/
      server/                 Composition root and production binary
      admin/                  TypeScript admin SPA
      web/                    Optional renderer and static exporter
    crates/
      kernel/                 IDs, errors, clocks, events, and shared ports
      contracts/              HTTP DTOs, OpenAPI, and JSON Schema
      content/                Markdown, revisions, publishing, and assets
      ontology/               Optional entities, relations, and statements
      ai/                     AI2AI envelopes, artifacts, policy, and trace
      policy/                 Authentication and authorization ports
      runtime/                Jobs, outbox, settings, and feature registry
      plugin-api/             Manifest and external protocol contracts
      plugin-host/            Capability enforcement and plugin lifecycle
      storage-api/            Persistence ports
      storage-sqlite/         SQLite implementation and migrations
      blob-fs/                Content-addressed local blob implementation
      http-api/               Public and administrative HTTP adapters
      testkit/                Fixtures, fake clocks, and contract harnesses
    features/
      local-auth/
      rbac/
      comments/
      seo/
      ads/
      code-runner-client/
    packages/
      api-client/
      admin-ui/
      renderer/
      plugin-sdk-ts/
    runner/
      isolated code execution service

The physical layout may be consolidated while the implementation is small, but
the following dependency rules are mandatory:

- kernel has no dependency on HTTP, SQLite, an async runtime, an LLM SDK, or a
  concrete plugin runtime;
- domain crates depend only on kernel and stable value/contract crates;
- domain crates expose ports and do not import concrete adapters;
- adapters implement ports and are composed only by the server;
- optional features communicate through public ports and versioned domain
  events, not another feature's private tables;
- only the composition root may know every feature and adapter;
- dependency direction and forbidden imports are checked in CI.

One crate should represent a meaningful ownership or security boundary. Small
files are not split into crates merely to make the tree look modular.

## Content, revisions, and portability

Markdown text is the canonical authoring representation. A versioned parsed
tree, rendered HTML, search documents, previews, and embeddings are derived
data and can be rebuilt.

Every accepted edit creates or advances a revision. Publication records an
immutable publish snapshot. AI and plugins propose patches or new draft
revisions; they do not silently overwrite a published revision.

Engine-specific constructs use a versioned Markdown extension that preserves
the original invocation. A macro or plugin block stores:

- its stable type and schema version;
- its source arguments;
- references to input context;
- the most recent successful artifact;
- provenance and execution policy.

If a plugin is absent, the source block remains editable and exportable. A
published page may use the last successful artifact according to site policy;
it must not silently delete the block.

Native export consists of human-readable Markdown, metadata JSON, assets by
digest, revision metadata, redirect aliases, and optional ontology statements.
Export is a portability feature, not a substitute for an operational backup.

## Optional ontology

Ontology is a feature over stable content and entity identifiers. Sites can
disable it without changing Markdown semantics or the core publishing path.

Statements record subject, predicate, object or literal, source revision,
provenance, and schema version. The initial storage may be relational SQLite
tables and JSON Lines export. Search and AI may consume ontology references
through a port; they may not require ontology to exist.

No automatic extraction is treated as fact merely because a model produced
it. Model-produced statements carry provenance and an approval state.

## AI2AI contract

The core stores a provider-neutral AI2AI envelope. It includes:

- message, thread, and parent identifiers;
- an actor of user, agent, or tool;
- intent and typed content parts;
- references to content revisions, memory scopes, and ontology entities;
- the effective policy and budget snapshot;
- proposed patches and immutable artifacts;
- provider and model execution metadata;
- trace, provenance, status, and error information.

Agents exchange envelopes and artifacts through the job/runtime boundary. They
do not mutate content tables or call one another's private implementation
directly. Provider-specific request and response formats live in adapters.
External agent protocols can later be mapped to and from the normalized
envelope without redefining the content model.

AI work is asynchronous unless a bounded, explicitly safe operation is
designated synchronous. Duplicate delivery is expected, so commands and
artifact writes require idempotency keys.

## Persistence and jobs

SQLite is the sole required database for the initial release.

- Foreign keys are enabled.
- WAL mode and a bounded busy timeout are used.
- Write transactions are short and never contain model, network, or code
  execution.
- The server is the single logical database owner.
- Background work uses a SQLite job table with leases, attempts, and
  idempotency keys.
- The default SQLite deployment does not place the database on a network file
  system or let multiple containers independently own it.
- Backups use SQLite's online backup mechanism and a consistent blob manifest,
  not a raw copy of a live database file.

Blobs are content-addressed and stored behind a storage port. The first adapter
uses a local data directory; a later object-store adapter must not change
content identifiers.

Official built-in features may own reviewed, prefixed tables and migrations.
External plugins do not receive a database handle or arbitrary SQL access.
They use a host-managed, namespaced state API. Disable, package removal, and
destructive data purge are separate lifecycle actions.

A different database backend is a future adapter, not an MVP requirement.
SQLite limitations will trigger reconsideration only when measured write
contention or multi-host requirements justify the operational cost.

## Authentication and authorization

Authentication providers and RBAC policy implementations are replaceable
features, but authorization enforcement is a kernel invariant.

The kernel exposes authentication and access-policy ports. If the RBAC feature
is absent, the safe default is a single-owner policy, not allow-all. Public
read access is an explicit resource policy. Administrative and plugin routes
always pass through the same policy and capability checks before their handler
runs.

Local authentication is an official feature. Future OIDC or reverse-proxy
providers implement the same port and cannot bypass authorization.

## Optional product features

Each feature has a defined absent state:

| Feature | Responsibility | Behavior when absent |
| --- | --- | --- |
| Local auth | Local identity and session issuance | Another configured provider is required |
| RBAC | Multi-user roles and resource policy | Safe single-owner policy remains |
| Comments | Submission, moderation, and rendering | No public comment routes or output |
| SEO | Canonical metadata, sitemap, and robots output | Renderer uses minimal safe defaults |
| Ads | Provider adapters and named render slots | Slots produce no markup |
| Code runner | Submit bounded code jobs and read artifacts | Execution is unavailable; stored artifacts remain |

Disabling a feature preserves its data. Purging data is an explicit,
independently authorized operation.

## Extension model and trust tiers

The project does not expose Rust dynamic libraries as a public plugin ABI.
There are three extension tiers:

### Trusted built-in

First-party, reviewed Rust features are linked at compile time and implement an
internal Extension trait. They run in the server process and therefore have
server-level trust. The trait is an internal source compatibility boundary,
not a promise of stable binary compatibility.

### Sandboxed WASI component

Pure transforms, macros, validators, and bounded render extensions should use
a versioned WASI Component/WIT contract. The host grants individual
capabilities. No ambient filesystem, network, clock, randomness, or secret
access is assumed.

### Isolated JSON-RPC process

Connectors that need a language runtime or libraries unavailable in WASI use
versioned JSON-RPC over standard input and output. The host launches the
executable directly without a shell. Untrusted processes run inside an
operating-system sandbox or rootless container; a process running as the same
user without a sandbox is not a security boundary.

The manifest at ../../schemas/plugin-manifest.v1.schema.json declares runtime,
API version, extension kinds, requested capabilities, limits, hooks, schemas,
and provenance. A request in the manifest is not a grant. The administrator
approves a concrete permission set, and the host checks it on every operation.

External plugins never gain implicit access to:

- core or feature database tables;
- the host filesystem or container runtime socket;
- unpublished content;
- network destinations;
- secret values;
- administrative routes;
- raw unsanitized rendering.

Plugin versions and approved capabilities are pinned in a lock file. An API
version mismatch, failed handshake, crash, or timeout disables that plugin
without preventing safe-mode startup.

## Code execution

Arbitrary code is never evaluated in the server or a general plugin process.
The optional code-runner client submits a typed job to a separately deployed
runner. The runner has no mount of the CMS database, blob directory, plugin
directory, or secret store. Its normative threat model and controls are in
../security/CODE-RUNNER.md.

The server must not receive a host container-runtime socket merely to support
code execution.

## API and TypeScript UI

The server exposes versioned public-content and administrative HTTP APIs.
OpenAPI and JSON Schema are generated or verified from the Rust contract
definitions, and the TypeScript API client is generated from the checked-in
contract. Contract snapshots prevent accidental breaking changes.

The admin SPA is a client of the administrative API, not a privileged database
client. Untrusted admin extensions render in a sandboxed iframe and communicate
through a narrow, origin-checked message API.

Long-running operations return a job identifier. Mutable resource APIs use a
revision identifier or ETag for optimistic concurrency. External events use a
transactional outbox and signed webhooks; consumers must tolerate at-least-once
delivery.

## Clean-room and licensing policy

Original project code is released under the Unlicense. The repository root
contains the controlling LICENSE, and first-party packages declare the SPDX
identifier Unlicense.

Clean-room means:

- implementation is derived from this project's requirements and public
  interface specifications, not copied from WordPress, Velog, proprietary
  products, leaked code, or incompatible examples;
- contributors record the origin of imported or generated nontrivial material;
- third-party dependencies and assets retain their own licenses and notices;
- generated code records its generator and source schema;
- official plugins declare provenance and license in their manifests;
- dependency license reports, notices, and an SBOM are release artifacts.

The Unlicense applies only to original project material. It does not override
dependency licenses, trademarks, model terms, or advertising-provider terms.

## Security invariants

The following are release-blocking invariants:

- deny by default when identity, policy, or plugin permission is ambiguous;
- no external plugin has raw database access;
- no arbitrary code executes in the server process;
- no network access is ambient for a plugin or code job;
- published content survives a missing or failed optional feature;
- secrets are references managed by the host and are excluded from export;
- rendered Markdown, comments, plugin fragments, and admin messages cross an
  explicit sanitization or trust boundary;
- upgrades can start in safe mode without third-party plugins.

## MVP verification

The initial release requires automated coverage for:

- Markdown parse, edit, revision, publish, and export/import round trips;
- ontology disabled and enabled modes;
- AI2AI envelope serialization, idempotency, provenance, and budget rejection;
- fresh SQLite migration, upgrade fixtures, crash recovery, concurrent reads,
  online backup, and restore;
- access-policy matrices and horizontal authorization failures;
- comment moderation and safe rendering;
- canonical SEO metadata and sitemap generation;
- empty advertising slots when no provider is installed;
- manifest validation, API mismatch, permission denial, timeout, crash, and
  safe-mode startup;
- missing-plugin preservation of source and last successful artifact;
- the code-runner tests required by the runner security document;
- OpenAPI, JSON Schema, Rust DTO, and generated TypeScript client compatibility.

## Consequences

Positive consequences:

- the default installation remains one server, one data directory, and an
  optional runner;
- domain logic can be tested without HTTP, SQLite, or an LLM;
- the website, admin UI, provider adapters, and optional features can evolve
  without database coupling;
- content remains readable and exportable without a plugin ecosystem;
- plugin trust is explicit instead of implied by installation.

Costs and limitations:

- SQLite intentionally limits initial horizontal scaling;
- built-ins and external plugins have different APIs and trust properties;
- capability brokering and last-good-artifact behavior require more work than
  loading arbitrary modules in-process;
- a code runner remains a high-risk optional subsystem even when isolated;
- Unlicense permits proprietary forks and does not impose reciprocity.

## Alternatives rejected for the initial release

- Microservices for each feature: too much deployment and transaction
  complexity for a one-person on-premise installation.
- Rust dynamic-library plugins: no suitable stable Rust ABI and no meaningful
  isolation.
- Git as a simultaneous second writable source of truth: ambiguous conflict
  handling for drafts, jobs, and revisions. Git export may be added as an
  adapter.
- Full event sourcing: unnecessary complexity for content queries and schema
  evolution. Immutable revisions plus an outbox are sufficient.
- Mandatory ontology: damages the simple Markdown path and portability.
- Mandatory Redis was rejected in this original baseline, then deliberately
  adopted for the public derivative hot path by ADR 0003. External search and
  vector databases remain outside the baseline.

## Revisit triggers

This decision should be revisited when one of the following is demonstrated:

- sustained SQLite write contention under a supported workload;
- a requirement for active-active or multi-host writes;
- an extension cannot be expressed safely by a built-in, WASI component, or
  isolated process contract;
- measured workload requires a separately scalable job service;
- an external AI2AI standard provides a stable superset of the normalized
  envelope and has meaningful interoperability value.
