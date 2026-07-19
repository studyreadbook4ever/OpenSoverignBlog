# OpenSoverignBlog

OpenSoverignBlog is a clean-room, Unlicense, self-hosted publishing kernel for
humans and software agents. It is intentionally spelled `Soverign` to match the
repository name.

The reference application can run as a personal server, a local-account
community, or an immutable delivery node. Redis is the required public hot
path; SQLite and content-addressed blobs remain the owned source of truth.
Comments, collaborators, administrator authentication, owner CSS, SEO, and
agent discovery are explicit operator intents instead of accidental side
effects.

> Status: architecture preview. The contracts are being implemented before the
> feature surface is expanded. Do not expose the server to the public internet
> without reading [SECURITY.md](SECURITY.md).

## Design commitments

- Rust core and TypeScript user interfaces.
- SQLite/blob ownership with a required Redis derivative cache and no required
  cloud service.
- Separate authenticated control-plane routes from session-independent,
  ETag-enabled public delivery routes.
- Independent administrator and member authentication. A bootstrapped personal
  deployment defaults to an Argon2id-verified administrator access key exchanged
  for an opaque HttpOnly session; local member passwords remain Argon2id-based.
- Persisted blog ownership and cross-tenant authorization at the repository
  boundary.
- Allowlisted `paper`, `ink`, `forest`, and `terminal` themes, plus an explicit
  on-premise owner-CSS flag and a first-party stylesheet boundary.
- Markdown is always exportable; author-intent HTML is an optional, sanitized
  projection; ontology data is an optional sidecar.
- An AI proposes a revision through a versioned AI2AI contract. It never gets
  implicit database access and is never required for editing or reading.
- Authentication, RBAC, comments, SEO, ads, code execution, renderers, and
  model providers are capability-scoped modules.
- Public content survives a missing or failed plugin.
- Third-party code execution never runs inside the publishing process.
- Source code is original clean-room work. External projects may inform the
  specification, but their code, styles, and assets are not copied.

## Repository map

```text
apps/server             Rust composition root and HTTP API
apps/web                detachable TypeScript public UI and local-first studio
apps/mcp                thin, read-default MCP stdio adapter over the HTTP API
crates/kernel           content, revision, optional intent/ontology, AI2AI model
crates/storage-sqlite   SQLite repository and migrations
crates/renderer         Markdown and untrusted HTML publish pipeline
crates/plugin-api       versioned plugin manifests and capabilities
packages/sdk            framework-neutral TypeScript API client
plugins/official        optional feature manifests
schemas                 machine-readable public contracts
docs                    architecture, legal, security, and AI2AI guidance
```

## Bootstrap and local development

The shortest supported on-premise path is semantic bootstrap plus Compose:

```sh
cargo run -p osb-cli -- bootstrap \
  --directory ./osb-deployment \
  --intent personal \
  --public-url http://localhost:8787 \
  --redis-topology managed
# Run the exact project-scoped Compose command printed by bootstrap.
```

`osb bootstrap` writes `config.toml`, a protected `.env`, `custom.css`, and a
stable `osb.intent.json` handoff for the next human or coding agent. The default
personal `access_key` profile also gets a mode-0600 `admin-access-key.txt`;
`.env` contains only its Base64-wrapped Argon2id PHC, never the plaintext key.
Generated secret files are included in the `.gitignore` of a fresh deployment.
An existing operator `.gitignore` is preserved only after bootstrap verifies
that it already contains `.env` (or `/.env`) and `admin-access-key.txt` (or
`/admin-access-key.txt`); otherwise bootstrap stops before creating any secret.
Bootstrap never overwrites an operator file.

The bundled Compose stack starts a Redis primary, replica, and three Sentinel
voters, then waits for the application `/readyz`. This removes manual cache
failover but does not pretend that five containers on one host are five physical
failure domains. Bootstrap also generates separate Redis-authentication and
cache-integrity keys plus a host backup mount; use a NAS path for host-loss
protection.

Administrator access is configured independently from reader/member accounts:
`--admin-auth access-key` (the personal default), `external`, or `disabled`.
External mode is a generic OIDC authorization-code flow bound to one exact
issuer and stable `sub`, with state, PKCE, and nonce verification. Firebase and
email verification are future second-party adapters at the same verified-
identity boundary; they are not built-in providers today. The typed shared
secret is an **administrator access key**, not a WebAuthn Passkey. OIDC pending
state is process-local, so multiple application replicas require sticky routing
from login start through callback. A deliberate key, issuer/subject, or mode
change uses `OSB_ADMIN_AUTH_ROTATE=true` for one restart, revokes existing admin
sessions by advancing the auth epoch, and must be reset to `false` immediately.
Schema v2 rejects legacy `OSB_ADMIN_TOKEN`; only v1/schema-less migrations retain
that temporary API compatibility, and the new Web Studio deliberately offers no
browser Bearer input or storage.

For a read-only delivery copy, pass both the writable node's stable
`--site-id` and a unique restored-generation `--content-release`. Bootstrap
then records the verify/restore/start sequence in `osb.intent.json`; it never
starts an empty delivery database or silently invents another site identity.

Run the generated online-doctor command inside the application container:
Redis and Sentinel names are private Compose DNS names and the credentials stay
in the container environment. For source development against a host Redis, run the
CLI directly and supply the same `OSB_*` overrides as the server; `doctor`
uses the identical non-empty-environment precedence.

For a community profile, add `--intent community --comments enabled`; add
`--collaboration enabled` only when invited co-author access is wanted. A
delivery profile sets both semantic intent and the mutation boundary to
read-only. See [the configuration reference](docs/operations/CONFIGURATION.md)
for every flag, Redis failure behavior, and external backup mounts.

Source development still requires stable Rust, Node.js 22+, npm 10+, and a
reachable Redis endpoint. Run `cargo test --workspace`, `npm install`, and
`npm run check` before building the image.

## AI2AI

Agents should start with [AI2AI.md](AI2AI.md), then read
`.well-known/open-soverign-blog.json` beneath the instance's configured public
URL. Discovery links are absolute and retain any reverse-proxy base path.
Contracts are also published under `schemas/` so an agent can validate every
mutation before submitting it.

For a deliberately small MCP surface, build `osb-mcp` from
[apps/mcp](apps/mcp/README.md). It is read-only by default and contains no model,
prompt, browser-automation, or macro runtime. Optional write mode uses one
separate static `OSB_MCP_TOKEN` with a fixed content-only route scope; it cannot
authenticate administrator, AI2AI, asset, runner, settings, or member APIs. It
requires an active administrator module and is forbidden on delivery nodes.
Rotate or revoke it by changing/removing the environment value and restarting
every application replica. Never reuse an administrator access key, provider
token, legacy owner token, or browser session as the MCP token.

## License and provenance

Original project code is released under the [Unlicense](LICENSE). Dependencies
retain their own licenses. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md),
the generated inventory under `docs/legal/`, and `deny.toml` for the dependency
policy and clean-room distribution boundary.
