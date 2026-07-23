# OpenSoverignBlog

OpenSoverignBlog is a Linux-first, self-owned publishing engine. Anonymous
readers get a small read-only surface, while the server owner keeps the
authoritative SQLite database, Markdown revisions, first-party blobs, install
intent, and backups. Redis is an optional derivative cache, never the database.

The project name intentionally keeps the repository's `Soverign` spelling.
Original project code is released under the Unlicense and the public UI is an
original clean-room, dense document interface. It does not copy Velog or
Namuwiki source code, stylesheets, or assets.

> Status: architecture preview. The core publishing and on-premise paths work,
> but this is not yet a promise of production support for every optional
> adapter. Read [SECURITY.md](SECURITY.md) before exposing an instance to the
> internet.

## What the default installation does

An interactive personal bootstrap currently selects:

- anonymous public reading;
- an instance-wide `/references` page for sources, licensing, privacy, and
  operating policies;
- no reader/member accounts;
- an administrator access key exchanged for an HttpOnly browser session;
- the built-in `paper` style;
- managed Redis/Sentinel cache containers;
- durable SQLite, verified SQLite/blob backups every 15 minutes, retaining 96
  generations; and
- the SEO, home-curation, AI-authorship, social-embeds, and release-check DLCs.

All of those structural choices are prompts and flags. Redis can be absent,
administrator authentication can use generic OIDC or be entirely disabled,
and `--dlc none` produces a core-only composition. The exact choices are
remembered in a human-owned intent and machine-generated lock rather than being
guessed at each restart.

## Quick start

Prerequisites are Linux, Docker Engine with Compose v2, Git, and enough local
space for the image, canonical data, and backups. Building from source also
requires Rust 1.88 and Node.js 22+.

```sh
cargo build --release -p osb-cli

./scripts/osb-init.sh ko \
  --directory /srv/osb/my-blog \
  --public-url https://blog.example.com
```

Use `./scripts/osb-init.sh en ...` for an English installation. The wrapper also
accepts the canonical form `--language ko|en`, honors `OSB_BIN` when packaging
the CLI elsewhere, and otherwise uses a repository build or `cargo run`.

In a terminal, bootstrap asks for language first, followed by administrator
auth, style, cache, and DLCs. If language is omitted, pressing Enter selects
Korean. Non-interactive bootstrap also defaults to Korean.
It then prints one exact, project-scoped `docker compose` start command. Run
that command; do not invent another Compose project name. The generated
`osb.intent.json` also retains the same `composeProject` and next commands for a
human or another coding agent.

For a repeatable non-interactive personal deployment without Redis:

```sh
./target/release/osb bootstrap \
  --directory /srv/osb/my-blog \
  --non-interactive \
  --language en \
  --intent personal \
  --public-url https://blog.example.com \
  --admin-auth access-key \
  --auth disabled \
  --style builtin:forest \
  --cache none \
  --dlc seo \
  --dlc home-curation \
  --dlc ai-authorship \
  --dlc social-embeds \
  --dlc release-check
```

Bootstrap never overwrites an existing operator file. It creates:

| File | Purpose |
| --- | --- |
| `config.toml` | Secret-free semantic runtime configuration |
| `.env` | Mode-0600 secrets and exact runtime projection |
| `osb.install.toml` | Human-owned, secret-free structural intent |
| `osb.lock.json` | Exact engine/DLC records, digests, compatibility, history |
| `osb.intent.json` | Stable topology plus the `references.md` path/SHA-256 handoff |
| `custom.css` | Selected installed CSS boundary or harmless local template |
| `references.md` | Editable whole-site sources, licensing, privacy, and policy page |
| `admin-access-key.txt` | One-time plaintext key; access-key mode only |
| `.gitignore` | Protects secrets, backups, and `.osb-update/` snapshots |

Bootstrap pins `references.md` in `osb.intent.json`. For a direct, non-bootstrap
configuration, the built-in, inline, or file-backed references source remains
a valid runtime input without that handoff; `osb doctor` warns that integrity is
unpinned. If a sibling handoff exists, a missing or changed contracted source
is a blocking doctor failure.

If `.gitignore` already exists, bootstrap requires exact `.env`,
`admin-access-key.txt`, `.osb-backups/`, and `.osb-update/` protections before
creating secrets. A later `!` rule that could re-include any protected path is
rejected; place the exact protective entries after broader negations.

The access-key file is not copied into the application image. Store it in a
password manager, then remove the plaintext file if your recovery policy no
longer needs it. `.env` retains only an Argon2id verifier.

## Administrator and member authentication

Administrator auth and reader/member auth are separate modules.

| `--admin-auth` | Remote Studio | Intended use |
| --- | --- | --- |
| `access-key` | Yes | Paste one high-entropy administrator access key, receive an opaque HttpOnly session |
| `external` | Yes | Generic OIDC authorization-code flow bound to one exact issuer and stable owner `sub` |
| `disabled` | No | Anonymous web remains read-only; maintenance writes happen on the server with `osb local` |

The typed administrator secret is an **access key**, not a WebAuthn Passkey.
It is still suitable for the requested “enter one owner key, then use Studio”
flow: after exchange, the browser uses a short-lived, revocable server session
instead of storing or replaying the key on every mutation. Any remotely
reachable administrator mode requires HTTPS outside localhost.

External mode needs `--external-issuer-url`, `--external-client-id`, and the
one exact `--external-owner-subject`. Put the provider client secret in the
generated `.env` before starting. Discovery, state, PKCE S256, nonce,
issuer/audience validation, and exact owner-subject binding are enforced.
Firebase, email verification, and provider-specific recovery are not bundled;
they can be added later as second-party identity adapters at this boundary.

`--auth` controls reader/member accounts independently. `disabled` is the
personal default; `local` enables the built-in account path for community
comments and collaborators. OAuth-only bootstrap is rejected before files are
created because this preview ships no verified member-OAuth provider.
`local-and-oauth` remains usable through local login while recording the
reserved adapter intent; it does not make member OAuth operational. Do not
advertise Firebase/member OAuth merely because a flag can express the intent.

A deliberate administrator key or same-module external issuer/subject rotation
sets `OSB_ADMIN_AUTH_ROTATE=true` for one restart. That advances the persisted
auth epoch and revokes prior administrator sessions. Reset it to `false` after
the successful rotation. The `access_key`/`external`/`disabled` mode itself is a
tracked installation choice; this preview has no in-place auth-mode migration,
so bootstrap a replacement deployment contract instead of hand-editing the
intent and lock.

## No remote auth: server-local publishing

With `--admin-auth disabled --auth disabled`, every network client is a reader.
The image still contains a network-disabled maintenance service for the person
who owns the Docker volume. Start the writable server once so it provisions the
primary site, stop `blog`, run the local command, then run the exact start
command printed by bootstrap again. Restarting is part of the safety boundary:
it rotates the optional Redis derivative generation after the canonical SQLite
write.

Use the exact Compose project and Compose file recorded by bootstrap:

```sh
docker compose -p <compose-project> \
  --env-file /srv/osb/my-blog/.env \
  -f /path/to/OpenSoverignBlog/compose.yaml \
  stop blog

docker compose -p <compose-project> \
  --env-file /srv/osb/my-blog/.env \
  -f /path/to/OpenSoverignBlog/compose.yaml \
  --profile maintenance run --rm osb-local \
  local setup --handle my-notes --title "My Notes"

docker compose -p <compose-project> \
  --env-file /srv/osb/my-blog/.env \
  -f /path/to/OpenSoverignBlog/compose.yaml \
  --profile maintenance run --rm -T osb-local \
  local publish --title "First post" --slug first-post --markdown - < post.md
```

`osb local list` prints document UUIDs. Revise and publish the same document by
passing one back; omitted title/slug retain their current values:

```sh
docker compose -p <compose-project> \
  --env-file /srv/osb/my-blog/.env \
  -f /path/to/OpenSoverignBlog/compose.yaml \
  --profile maintenance run --rm -T osb-local \
  local publish --document-id <document-uuid> --markdown - < revised.md
```

AI/import disclosure is portable content metadata. For example, add
`--authorship ai-assisted --generator "provider/model-or-tool" --human-reviewed`.
The local command accepts only a regular non-symlink UTF-8 file or bounded stdin
and publishes the exact immutable revision it just created. The maintenance
container also mounts and validates the trusted `config.toml`; delivery-only
configuration is rejected before the local command opens SQLite. It receives
the same non-empty `OSB_INTENT`, `OSB_DELIVERY_ONLY`, and
`OSB_ARTICLE_BASE_PATH` overrides as the server, so the recorded `--env-file`
is part of both write admission and offline-import route collision checks.

For a whole-site migration, `osb local import --manifest import.json` validates
and publishes a versioned Markdown manifest in one transaction. Run
`--dry-run` first; exact retries are no-ops, while changed content under an
existing `sourceId` fails without partial writes. Historical route aliases,
including deeply nested `.html` paths, permanently redirect to the absolute
current category URL. See [offline Markdown import](docs/operations/OFFLINE_IMPORT.md)
for the schema, mount-safe Compose invocation, and redirect boundary.

## Cache, authority, and redundancy

`--cache none` reads directly from authoritative SQLite and blobs.
`redis-standalone` adds one authenticated Redis primary.
`redis-managed` adds a primary, replica, and three Sentinel voters. Redis
contains signed, expiring response derivatives and rate-limit state; losing all
of Redis does not lose a post.

The managed profile improves automatic process failover on one host. It is not
a second physical failure domain. Canonical redundancy comes from verified
SQLite-plus-blob backup generations. Point `OSB_BACKUP_VOLUME` at an
independently backed disk or NAS and regularly restore a generation on another
machine. Keeping two Docker volumes on one server does not protect against host
loss. Generations omit deployment controls, so back up and restore
`config.toml`, `references.md`, `osb.intent.json`, installation intent/lock, and
selected CSS together; keep `.env` and access credentials in a separate secrets
system.

Durability profiles are semantic choices:

- `durable` is the recommended on-premise default;
- `balanced` trades some fsync latency for throughput; and
- `fast` keeps SQLite safe enough for preview workloads but is not a substitute
  for measured storage and restore tests.

## Styles and public UI

Choose `none`, `builtin:paper`, `builtin:ink`, `builtin:forest`,
`builtin:terminal`, or `--css-file /regular/file.css`. A custom file is copied
into the deployment, bounded, SHA-256 pinned in the installation lock, and
served only through the first-party CSS boundary. The original workstation
path is not retained as the durable contract.

The public home shows up to three administrator-selected posts first, ordered
Series sections, ordinary category sections, and then recently published
changes without duplicates. Series and category sections have independent,
accessible collapse controls. Studio starts creation with an explicit
Post-versus-Series choice, appends a published Series post to the reading-order
tail, and lets the owner reorder the exact published member set. An existing
category can be promoted idempotently without changing its URLs or published
revision placement.

Studio also provides a direct title/Markdown writing surface, preview, explicit
publication review, pinned-home management, portable AI authorship disclosure,
and typed YouTube/X embed insertion. YouTube uses a privacy-enhanced,
click-to-load iframe; X remains a safe link card in this initial adapter.
Arbitrary embed HTML is never accepted.
The primary site's category pages use `/category` and `/category/slug`.
Additional community blogs use `/@handle/category` and
`/@handle/category/slug`; uncategorized posts retain `/@handle/slug`.
Category-backed Series keep those same natural routes. Category landing pages
are first-class reader pages, and Studio keeps category creation/appearance
separate from writing. Legacy flat post paths redirect to the corresponding
natural category path only when the site's published leaf slug has one
unambiguous match; duplicate leaves across categories stay 404.

## DLC lifecycle

Official DLCs are selected by stable alias or reverse-domain ID and locked to
an exact bundled manifest digest. The current catalog is:

| Alias | Runtime purpose |
| --- | --- |
| `seo` | Canonical discovery, robots, sitemap, metadata |
| `home-curation` | Up to three pinned home posts and recent fallback |
| `ai-authorship` | Portable authorship disclosure and AI2AI proposal surface |
| `ai-summary` | Opt-in, human-reviewed per-post summary generation with ephemeral provider keys |
| `social-embeds` | Strict YouTube/X references and consent-first rendering |
| `release-check` | Bounded informational stable-channel checks |
| `comments` | Authenticated community comments when community config is enabled |
| `rbac` | Owner/editor/writer collaboration when enabled |
| `external-auth` | Generic external identity composition |
| `code-runner` | Contract for an out-of-process isolated runner; broker required |
| `ads` | Monetization/consent policy contract; provider adapter required |

The last two do not become operational merely by being named. Capabilities
report `misconfigured` or `available` until their separately isolated adapter
is actually ready.

Lifecycle commands stage and fsync `osb.install.toml`, `osb.lock.json`, and the
three managed `.env` projection fields, with byte-for-byte rollback for reported
in-process errors. This is not a filesystem-wide atomic commit: serialize these
commands, never run two lifecycle/updater processes concurrently, and after a
power loss restore one matching control-file set from the protected update
snapshot or adjacent backups before retrying. Unknown secret lines are
preserved byte-for-byte. Disable/remove changes composition and records
contiguous history; it deliberately does not delete DLC tables, content, or the
host-owned migration ledger, which is restored when the same DLC is re-added.

```sh
osb installation dlc list --available
osb installation dlc add social-embeds@^0.1
osb installation dlc disable social-embeds
osb installation dlc enable social-embeds
osb installation dlc upgrade social-embeds
osb installation dlc remove social-embeds
osb installation verify --intent osb.install.toml --lock osb.lock.json
```

Only official manifests compiled into the matching CLI can be resolved. A
candidate release uses the special `reconcile` command to update the engine,
config/database/plugin-API tuple and re-resolve every requested DLC before any
live controls are promoted. Arbitrary remote plugin code is not loaded by this
lifecycle. `osb.intent.json` intentionally omits mutable lock digests and DLC
membership; read their current exact values from `osb.lock.json`.

## Bootstrap flag reference

Every interactive structural choice has a non-interactive flag:

| Flag | Meaning / default |
| --- | --- |
| `--directory DIR` | Fresh deployment directory; default `.` |
| `--non-interactive` | Never prompt; use supplied and documented defaults |
| `--language ko\|en` | Product and starter-content language; non-interactive default `ko` |
| `--compose-file FILE` | Compose bundle to record; default checkout `compose.yaml` |
| `--compose-project NAME` | Reuse an existing Compose project; default isolated `osb-UUID` |
| `--site-id UUID` | Stable content identity; required when creating a delivery restore |
| `--content-release ID` | Cache/snapshot generation; required and unique for delivery restores |
| `--intent personal\|community\|delivery` | Deployment profile; default `personal` |
| `--public-url URL` | Canonical reader URL; default localhost |
| `--auth local\|oauth\|local-and-oauth\|disabled` | Reader/member auth; personal default disabled, community default local. OAuth-only is fail-fast until an adapter ships; `local-and-oauth` retains working local login |
| `--admin-auth access-key\|external\|disabled` | Independent administrator mechanism |
| `--external-issuer-url URL` | Exact OIDC issuer for external admin |
| `--external-client-id ID` | OIDC client ID |
| `--external-owner-subject SUB` | Only stable OIDC subject allowed to administer |
| `--external-label TEXT` | External-login button label; otherwise localized from `--language` |
| `--registration enabled\|disabled` | Local member signup; default disabled |
| `--comments enabled\|disabled` | Community comments; community intent defaults enabled |
| `--collaboration enabled\|disabled` | Invited co-authors; default disabled |
| `--custom-css enabled\|disabled` | Owner CSS compatibility switch |
| `--style none\|builtin:ID` | Exact absent or built-in style |
| `--css-file FILE` | Copy and pin an exact custom stylesheet; conflicts with `--style` |
| `--references-file FILE` | Copy and integrity-pin the global references Markdown |
| `--references-label TEXT` | References navigation label; otherwise localized from `--language` |
| `--seo enabled\|disabled` | SEO intent and DLC default; default enabled |
| `--agent-discovery enabled\|disabled` | `agent.txt`, `agents.txt`, `llms.txt`; default enabled |
| `--cache none\|redis-standalone\|redis-managed` | Cache composition; prompt default managed |
| `--redis-topology standalone\|managed` | Backward-compatible cache-topology selector |
| `--dlc ID[@SEMVER_REQ]` | Repeatable official DLC selection; `--dlc none` disables recommended defaults |
| `--database-profile durable\|balanced\|fast` | SQLite durability; default durable |
| `--managed-backups enabled\|disabled` | Verified generations; default enabled |
| `--backup-interval-minutes N` | Generation interval, 1–10080; default 15 |
| `--backup-retention N` | Retained generations, 2–10000; default 96 |

Run `osb bootstrap --help` and the relevant subcommand `--help` for exact value
syntax. `delivery` requires copied `--site-id` and a new `--content-release`,
forces both auth surfaces off, mounts canonical data read-only, and rejects
comments, collaboration, registration, and in-place updates.

## Versions and verified updates

The footer reads local `release.toml` and the bounded release channel. It shows
the current version/date and latest version/date only when those facts exist.
This repository currently has no published tag or GitHub Release, so it reports
an honest `no_release` state and does not fabricate a date.

Release checks are informational until a real signed tag and GitHub Release
exist; the bundled stable channel is intentionally empty today. The Linux
updater uses a verified backup, fresh candidate data volume, staged installation
lock, exact-version health checks, and automatic last-known-good rollback. It
never executes downloaded shell or sources `.env`. Read the
[verified update runbook](docs/operations/UPDATES.md) before using `--apply`.

## AI2AI and MCP

The engine intentionally keeps AI automation thin. Start with
[AI2AI.md](AI2AI.md) and the instance's
`.well-known/open-soverign-blog.json`. The versioned envelope accepts a proposed
immutable revision with provenance/context receipts; publication remains a
separate capability.

`osb-mcp` is read-only by default and contains no model, prompt, browser macro,
or general script runtime. Optional content-write mode uses a distinct scoped
MCP token and still cannot authenticate an administrator or manage settings.
An external AI is expected to create task-specific prompts/scripts dynamically,
review their inputs and outputs, and submit a normal proposal. The engine does
not define a macro syntax or execute those artifacts.

## Repository map and development

```text
apps/server             Rust HTTP composition root
apps/web                detachable TypeScript public UI and Studio
apps/cli                bootstrap, lifecycle, backup, restore, local maintenance
apps/mcp                thin read-default MCP stdio adapter
crates/kernel           content/revision/AI2AI domain contracts
crates/storage-sqlite   authoritative SQLite repository and migrations
crates/renderer         deterministic Markdown/intent renderer
crates/plugin-api       versioned DLC manifests and installation contracts
packages/sdk            framework-neutral TypeScript client
plugins/official        bundled official DLC manifests
schemas                 public and installation JSON Schemas
docs                    architecture, operations, legal, security guidance
```

Core verification commands:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
npm ci
npm run check
npm test
npm run validate:contracts
npm run validate:release
npm run test:updater
```

Server startup is tracked and fail-closed by default: an empty or missing
`OSB_INSTALL_LOCK_DIGEST` is an error. A developer running a pre-contract local
source fixture may temporarily set the exact
`OSB_ALLOW_UNTRACKED_INSTALLATION=true`; bootstrap output always sets it to
`false`, and delivery-only nodes reject the exception. Adopt/bootstrap the
checkout and bind the emitted lock digest before treating it as a deployment.

Docker is additionally required for the Compose smoke test and full on-premise
upgrade/rollback drill.

## License and provenance

Original project code is released under the [Unlicense](UNLICENSE). Dependencies
retain their own licenses. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md),
the generated inventory under `docs/legal/`, and `deny.toml` for the dependency
policy and clean-room distribution boundary.
