# Semantic configuration

OpenSoverignBlog treats configuration as an operator-intent contract. A human
or agent first states what the server *is* (`personal`, `community`, or
`delivery`); lower-level flags must agree with that intent. Unknown keys,
unsupported schema versions, contradictory read-only/auth settings, unsafe
backup roots, and incomplete Sentinel profiles fail startup.

Use the CLI instead of assembling environment variables by hand:

```sh
osb bootstrap \
  --directory /srv/open-soverign-blog/community \
  --intent community \
  --public-url https://notes.example.com \
  --comments enabled \
  --collaboration disabled \
  --custom-css enabled \
  --seo enabled \
  --agent-discovery enabled \
  --redis-topology managed \
  --database-profile durable
```

Run the exact project-scoped Compose start and doctor commands printed by the
CLI (and recorded in `osb.intent.json`). They include the source bundle,
deployment `.env`, and a unique Compose project name, so multiple writable or
delivery deployments can safely share one host.

Compose keeps Redis and Sentinel private, so the online doctor belongs inside
the blog container. A host-side source deployment may run `osb doctor`
directly; it applies the same non-empty `OSB_*` environment overrides as the
server. Use `--offline` when only the TOML/filesystem contract is reachable.

`bootstrap` creates these non-overwriting files:

- `config.toml`: authoritative, versioned runtime intent;
- `.env`: mode-0600 Compose settings and secrets;
- `.gitignore`: in a fresh directory, excludes `.env`, `admin-access-key.txt`,
  and local backups; an existing operator file is preserved only when it already
  has exact `.env` and `admin-access-key.txt` entries (leading `/` is accepted),
  otherwise bootstrap fails before generating secrets;
- `custom.css`: harmless first-party template; the feature flag decides whether it is served;
- `osb.intent.json`: secret-free AI handoff and next commands;
- `admin-access-key.txt`: mode-0600 plaintext credential, created only for the
  `access_key` administrator module.

## Intent and feature matrix

| TOML | Environment | Values and meaning |
| --- | --- | --- |
| `semantic.intent` | `OSB_INTENT` | `personal`, `community`, or `delivery` |
| `admin.auth` | `OSB_ADMIN_AUTH` | Administrator control plane: `access_key`, `external`, or `disabled` |
| `admin.session_days` | `OSB_ADMIN_SESSION_DAYS` | Opaque administrator session lifetime, 1–365 days |
| environment only | `OSB_ADMIN_ACCESS_KEY_PHC_B64` | Standard Base64 of an Argon2id PHC; required only for `access_key`, never plaintext |
| environment only | `OSB_ADMIN_AUTH_ROTATE` | One-shot `true` switch for an intentional administrator key/provider/mode change; reset to `false` immediately after a successful start |
| `community.auth` | `OSB_AUTH_MODE` | `local`, `oauth`, `local_and_oauth`, `disabled` |
| `community.registration_open` | `OSB_REGISTRATION_OPEN` | Allow new local accounts; existing accounts can sign in when closed |
| `community.comments` | `OSB_COMMENTS` | Mount or remove authenticated comment routes |
| `community.collaboration` | `OSB_COLLABORATION` | Request invited co-author policy; remains off in the simple owner profile |
| `appearance.custom_css` | `OSB_CUSTOM_CSS` | Serve the configured first-party owner stylesheet |
| `appearance.custom_css_file` | `OSB_CUSTOM_CSS_FILE` | Local CSS file, capped at 256 KiB |
| `discovery.agent_txt` | `OSB_AGENT_DISCOVERY` | Publish `agents.txt`, `agent.txt`, and `llms.txt` under the configured public URL |
| `features.seo` | `OSB_FEATURES` | Canonical search discovery, robots, and sitemap (`none` means an empty request set) |
| `deployment.delivery_only` | `OSB_DELIVERY_ONLY` | Open a pre-migrated SQLite artifact immutable/read-only and reject mutations |

`delivery` intent requires `delivery_only=true`, `admin.auth=disabled`, and
`community.auth=disabled`. Open registration, comments, and collaboration on a
writable node require an operational local member-auth mode. Member OAuth-only
is rejected until a member adapter exists; `local_and_oauth` continues through
local auth while reporting member OAuth as requested but unavailable. These are
validated relationships, not documentation-only conventions.

A delivery bootstrap additionally requires `--site-id` copied from the
writable node and a generation-specific `--content-release`. Its handoff lists
the verified backup copy, maintenance restore, start, and online doctor steps
in that order. The restore service automatically runs storage initialization,
is network-isolated, reads the backup mount read-only, and alone receives a
writable data mount; the public delivery service receives read-only data and
backup mounts.

`server.public_url` may include a reverse-proxy base path, for example
`https://notes.example.com/team`. Every URL emitted by discovery documents,
text indexes, redirects, and sitemaps must therefore be derived from
that complete public URL. Server code uses `SeoPolicy::public_route_url` for
multi-segment routes and `public_resource_url` for a single resource name;
string concatenation and root-relative `Location` or discovery links are not
base-path safe. The published OpenAPI discovery contract requires absolute
HTTP(S) links so this deployment property remains machine-verifiable.

Crawler discovery of `robots.txt` is a host-level exception: crawlers request
`https://host/robots.txt`, never `/team/robots.txt` as an authoritative file.
When attaching beneath an existing origin, configure the host reverse proxy's
root robots file to reference `/team/sitemap.xml` (or rely on per-page noindex)
and coordinate its origin-wide `Allow`/`Disallow` rules with the existing site.

Member OAuth/OIDC is distinct from administrator authentication.
`local_and_oauth` advertises member OAuth as requested but not operational until
a cryptographically verifying member adapter is configured; the server never
treats an unverified proxy header as a login.

## Administrator authentication

Administrator auth controls the instance owner session and is independent from
reader/member signup and login. The modes are mutually exclusive:

- `access_key` is the default for writable bootstrap profiles. Bootstrap
  generates 32 random bytes, writes the Base64url plaintext to mode-0600
  `admin-access-key.txt`, and writes only the standard-Base64-wrapped Argon2id
  PHC to `OSB_ADMIN_ACCESS_KEY_PHC_B64`. The browser exchanges the typed key for
  an opaque HttpOnly session; it does not persist the key as a Bearer token.
- `external` runs a generic OIDC authorization-code flow. Configure
  `[admin.external]` with `adapter="oidc"`, an HTTPS `issuer_url` without URL
  credentials/query/fragment, `client_id`, exact stable `owner_subject` (`sub`),
  and optional label. Put an optional client secret in
  `OSB_EXTERNAL_CLIENT_SECRET`. Provider metadata and signed ID tokens are
  verified, and state, PKCE, nonce, issuer, and exact `sub` must all match before
  an owner session is issued. Pending state/PKCE/nonce records are process-local,
  so multiple application replicas require sticky routing from the start request
  through the callback.
- `disabled` does not mount administrator login routes. With member auth also
  disabled, a schema-v2 writable personal origin remains publicly readable but
  has no remote web control plane. Delivery nodes require this mode and
  additionally enforce read-only storage.

The typed shared secret is an **administrator access key**, not a WebAuthn
Passkey: there is no platform authenticator, public-key credential, or WebAuthn
ceremony. Use HTTPS outside loopback, keep the plaintext file in a secret store,
and keep a recoverable protected copy. A key, external issuer/subject binding, or
mode change normally conflicts with the persisted control-plane fingerprint and
fails startup. To approve it, set `OSB_ADMIN_AUTH_ROTATE=true` for one coordinated
restart. The server atomically advances `auth_epoch`, invalidates existing admin
sessions, clears the prior external identity binding when applicable, and applies
the selected module. As soon as startup succeeds, write
`OSB_ADMIN_AUTH_ROTATE=false` back to the environment before any later restart or
rollout. Reusing the same target is idempotent, but leaving the one-shot switch
armed weakens the operator confirmation boundary.

The external adapter currently accepts one exact OIDC issuer/subject owner.
Firebase ID-token verification, email verification, and additional provider
policies are future second-party modules that should produce the same verified
identity boundary; they are not silently treated as generic OIDC today.

## Redis: required speed, disposable data

Redis is a core runtime dependency, not an optional plugin. It stores only
public derivative responses. Sessions, authorization, drafts, revisions,
canonical Markdown, and blobs remain in SQLite/local storage.

| TOML | Environment | Meaning |
| --- | --- | --- |
| `redis.topology` | `OSB_REDIS_TOPOLOGY` | `standalone` or `sentinel` |
| `redis.url` | `OSB_REDIS_URL` | Direct node settings and Redis credentials/TLS policy |
| `redis.sentinel_urls` | `OSB_REDIS_SENTINELS` | Comma-separated Sentinel control endpoints |
| `redis.sentinel_master` | `OSB_REDIS_SENTINEL_MASTER` | Monitored master name |
| `redis.namespace` | `OSB_REDIS_NAMESPACE` | Deployment key namespace |
| `redis.content_release` | `OSB_CONTENT_RELEASE` | Immutable delivery generation/cache isolation identifier |
| `redis.required` | `OSB_REDIS_REQUIRED` | Missing initial PING fails startup; keep `true` in supported profiles |
| `redis.password` | `OSB_REDIS_PASSWORD` | 32–128 URL-safe characters; bootstrap generates 64 random hex characters in mode-0600 `.env` |
| environment only | `OSB_CACHE_SIGNING_KEY` | 64 hex characters; authenticates cached public bodies with an application-only HMAC key that Redis never receives |
| `redis.response_ttl_seconds` | `OSB_REDIS_TTL_SECONDS` | Expiration/reclamation window for generation-scoped response derivatives |

The middleware caches only public successful GET responses. Private/session,
Studio, auth, mutation, media, and error responses bypass it. A public mutation
holds a cancellation-safe guard that suspends cache reads, then rotates a
non-repeating Redis generation after the canonical attempt. A miss records the
generation before rendering and stores only if it is still current, so an old
render cannot enter a new generation. Signed envelopes bind route, generation,
headers, and body to the application-only key. Runtime Redis failure drops to
SQLite/FS origin, marks health degraded, and re-discovers the Sentinel master
on the next attempt.

The bundled managed profile uses one authenticated Redis primary, one replica,
and three authenticated Sentinels with AOF/every-second sync plus native RDB
checkpoints. This provides automatic process failover on a host. It does not
survive loss of that host; canonical backups do. Do not add an application loop
that periodically empties and reloads Redis: it creates another consistency
protocol while duplicating Redis' own persistence and replication.

Redis nodes and Sentinels announce stable Compose service hostnames consistently,
and Sentinel rewrites the promoted hostname into its private persistent config.
Ephemeral container IPs are never used as durable control-plane identities.
Sentinel handles an unresponsive Redis process while Docker's restart policy
handles an exited container; host loss still requires canonical backup restore.

The dedicated Redis nodes use `volatile-lfu`: only TTL-bound response
derivatives are eviction candidates, while the non-expiring cache-generation
key is retained. If memory is exhausted before a derivative can be stored, the
application falls back to the authoritative origin instead of sacrificing the
coherence marker.

## SQLite and managed backup generations

| TOML | Environment | Meaning |
| --- | --- | --- |
| `storage.profile` | `OSB_DATABASE_PROFILE` | `durable`, `balanced`, or `fast` WAL/checkpoint policy |
| `operations.managed_backups` | `OSB_MANAGED_BACKUPS` | Run verified background backup generations |
| `operations.backup_directory` | `OSB_BACKUP_DIRECTORY` | Destination; filesystem roots are rejected |
| `operations.backup_interval_minutes` | `OSB_BACKUP_INTERVAL_MINUTES` | 1 minute through 7 days |
| `operations.backup_retention` | `OSB_BACKUP_RETENTION` | 2–10,000 named generations |

Each generation uses SQLite's Online Backup API, copies the content-addressed
blob tree, hashes every payload, writes `manifest.json`, fsyncs the staging
directory, and renames it atomically. Retention removes only real directories
whose names begin with `generation-` under the dedicated generations root.

Compose defaults to the visible host directory `./.osb-backups`, separate from
the live Docker data volume. For actual host-level redundancy, point the same
mount at an independently backed disk or NAS:

```dotenv
OSB_BACKUP_VOLUME=/mnt/independent-backup/open-soverign-blog
```

The Compose storage initializer recursively gives a local backup tree to
container UID/GID `65532`, with directories normalized to `0700` and regular
files to `0600`; it neither follows symlinks nor crosses nested mounts.
Root-squashed NFS/NAS exports must instead be pre-provisioned with that owner
and those modes across the entire tree. The initializer validates only the NFS
mount root and leaves descendants untouched, so copied generations must
preserve the same contract.

Do not place the live SQLite database on a network filesystem. Only the backup
destination may be remote. A backup is not proven until `osb verify-bundle` or
an equivalent restore drill has opened its database and checked every manifest
hash.

## Server, URL, and security settings

| TOML | Environment | Meaning |
| --- | --- | --- |
| `server.bind` | `OSB_BIND` | Listen address; Compose uses `0.0.0.0:8787` internally |
| `server.public_url` | `OSB_PUBLIC_URL` | Exact canonical origin and optional reverse-proxy base path |
| `server.article_base_path` | `OSB_ARTICLE_BASE_PATH` | URL-safe article path that cannot collide with server routes |
| `server.site_id` | `OSB_SITE_ID` | Stable UUID; never change after publishing |
| `server.no_index` | `OSB_NO_INDEX` | Emit `noindex` and omit sitemap while leaving pages crawlable enough to observe it |
| `storage.database` | `OSB_DATABASE` | Local SQLite file |
| `storage.blob_directory` | `OSB_BLOB_DIRECTORY` | First-party content-addressed passive assets |
| `security.admin_token` | `OSB_ADMIN_TOKEN` | Migration-only legacy owner Bearer; schema v2 rejects it, while schema v1/schema-less configurations retain temporary compatibility |

Environment variables override TOML only when non-empty. `OSB_FEATURES=none`
is the explicit empty feature request. `/api/v1/capabilities` reports requested
versus operational modules, while `/livez`, `/readyz`, and `/healthz` separate
process liveness, required Redis readiness, and degraded dependency detail.
The schema-v1/schema-less legacy owner token is API migration compatibility only:
the new Web Studio intentionally provides no browser Bearer input or storage.

The isolated code-runner settings from the previous configuration contract
remain supported. They are intentionally omitted from bootstrap; add a vetted
`[runner]` profile only after the base server passes `osb doctor`.

## MCP boundary

`apps/mcp` contains a thin stdio adapter over the public HTTP API. Its default
`OSB_MCP_MODE=read` exposes only list/read tools and needs no credential. It
does not contain an LLM, prompt system, browser automation, macro interpreter,
or direct SQLite/Redis access.

Write mode uses a separate, static environment-only `OSB_MCP_TOKEN` containing
32-128 unpadded Base64url ASCII characters. The server retains only its SHA-256
digest. It is accepted for exactly these content route shapes:

- `GET /api/v1/admin/documents`
- `GET /api/v1/admin/documents/{uuid}`
- `GET /api/v1/admin/documents/{uuid}/revisions`
- `POST /api/v1/posts`
- `POST /api/v1/documents/{uuid}/revisions`
- `POST /api/v1/documents/{uuid}/publish`

It cannot authorize administrator auth, AI2AI proposals, assets, runner calls,
Studio settings, or member APIs. Configuration requires an active administrator
module and is rejected on delivery-only nodes. Remote MCP write mode requires
HTTPS; exact localhost/loopback development may use HTTP. This release has one
global static content credential rather than per-client issuance or independent
scopes. Rotate it by changing the value in both the server and MCP client secret
environment and restarting every application/MCP replica. Remove it from the
server environment and restart every application replica for global revocation.
Never substitute an administrator access key, legacy owner token,
external-provider token, or browser session cookie. See
[the adapter guide](../../apps/mcp/README.md).
