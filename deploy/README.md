# On-premise deployment

The supported profile is one application container, an owned SQLite/blob
volume, and a self-managing Redis hot path. The bundled managed topology uses a
Redis primary, replica, and three Sentinel voters. It is deliberately somewhat
inefficient so process failover does not require an operator command. Search,
Administrator auth, member auth, a model, advertising, and a code-runner remain
separately selected capabilities.

On a Linux Docker host, Redis background checkpoints and replication require
memory overcommit. Verify `sysctl vm.overcommit_memory` returns `1`; otherwise
set it persistently at the host level before relying on the HA profile (for
example, `vm.overcommit_memory = 1` in `/etc/sysctl.d/99-osb-redis.conf`, then
run `sudo sysctl --system`). This setting is not container-namespaced and
cannot be made reliable from `compose.yaml`.

```sh
cargo run -p osb-cli -- bootstrap --intent personal
docker compose up --build -d --wait
docker compose exec -T blog osb doctor --config /config/config.toml
```

The online doctor runs inside the application container so it can resolve the
private Redis/Sentinel service names without exposing their ports or secrets
on the host. Every Redis and Sentinel process announces a stable Compose
service hostname; mixing ephemeral container IPs with hostnames makes promotion
state unsafe across container recreation. The stack tests a non-responsive
primary while that network identity remains present. Docker's restart policy
handles an exited container; deleting the whole container or losing the host is
an orchestrator/recovery event, not another Sentinel failure domain.

The published host port binds to loopback. Put a TLS reverse proxy in front and
set `OSB_PUBLIC_URL` to the exact canonical origin. Do not mount a Docker socket.
Do not place the SQLite file on a network filesystem or let multiple containers
open it independently.

Personal bootstrap defaults to `--admin-auth access-key`. It creates
`admin-access-key.txt` and `.env` with mode `0600`, adds both to a fresh
deployment `.gitignore`, and stores only a Base64-encoded Argon2id PHC in
`OSB_ADMIN_ACCESS_KEY_PHC_B64`. The plaintext access key is typed into the
administrator login form and exchanged for an opaque HttpOnly session; it is
an administrator access key, not a WebAuthn Passkey. Protect and back it up as a
secret, and use HTTPS anywhere except loopback development. If `.gitignore`
already exists, add exact `.env` and `admin-access-key.txt` entries (a leading
`/` is also accepted) before bootstrap; an unsafe file is never changed and
bootstrap stops before generating secrets.

Choose `--admin-auth external` to use the built-in generic OIDC
authorization-code adapter. Bootstrap then requires `--external-issuer-url`,
`--external-client-id`, and the exact stable
`--external-owner-subject`; the callback checks provider discovery, issuer and
`sub`, state, PKCE, nonce, and the signed ID token before creating the same
owner session. The issuer must not contain URL credentials, a query, or a
fragment. Put an optional confidential-client secret in
`OSB_EXTERNAL_CLIENT_SECRET`, not TOML. Firebase token verification and email
verification are future second-party adapters at this verified-identity
boundary, not built-in choices today. OIDC login state is kept in the application
process for ten minutes, so a multi-replica deployment must keep the start and
callback requests on the same replica with sticky routing.

Changing the administrator key, external issuer/subject binding, or auth mode is
an explicit one-shot operation. Set `OSB_ADMIN_AUTH_ROTATE=true` for the restart
that applies the new configuration. The server advances `auth_epoch`, revokes all
existing administrator sessions, and resets the external binding when relevant.
After that start succeeds, immediately put `OSB_ADMIN_AUTH_ROTATE=false` back in
the environment file before any subsequent restart or rollout.

`--admin-auth disabled` removes the remote administrator login routes. On a
personal deployment, where member auth also defaults disabled, the public site
then has no remote web control plane. Member auth is independent: community local
accounts, registration, and comments are controlled by `--auth` and the community
flags, not by the administrator choice.

Schema v2 rejects `security.admin_token` and `OSB_ADMIN_TOKEN` because they bypass
the selected administrator module. The legacy per-request Bearer remains accepted
only for API compatibility while migrating a schema-v1 or schema-less
configuration. The new Web Studio intentionally has no browser Bearer input or
storage. Registration defaults closed. If public signup is enabled, place rate
limiting and abuse controls at the trusted reverse proxy before exposing the
service; the preview server does not yet include complete abuse defenses.

## Delivery-only node

Create each delivery deployment with the writable node's stable site ID and a
release identifier unique to the restored snapshot:

```sh
osb bootstrap \
  --intent delivery \
  --site-id 00000000-0000-7000-8000-000000000001 \
  --content-release generation-20260719T120000Z
```

The generated `osb.intent.json` lists the verified maintenance restore,
startup, and in-container doctor commands in the required order. The restore
service automatically runs the one-shot storage initializer first, mounts
`/data` read-write only for that restore, keeps `/backups` read-only, and has no
network. The public `blog` service subsequently mounts both paths read-only.

Set `OSB_INTENT=delivery`, `OSB_AUTH_MODE=disabled`,
`OSB_ADMIN_AUTH=disabled`, and `OSB_DELIVERY_ONLY=true` to run the public data
plane without writable sessions, Studio, publication, upload, or comment
submission. The node opens an already-migrated SQLite database with read-only
flags and fails startup instead of migrating or creating missing data. Prepare
a consistent, checkpointed database/blob snapshot on the writable node first.
After verifying the snapshot, it may be supplied to a delivery container on a
read-only volume. Public feed, blog, article, approved-comment, and immutable
asset routes remain available and are suitable for a shared reverse-proxy/CDN
cache.

Assign every immutable delivery snapshot a distinct `OSB_CONTENT_RELEASE` so
an old and new SQLite generation cannot read each other's Redis derivatives.

## Backups

Writable nodes create managed backup generations immediately and then at the
configured interval. Each generation contains an Online Backup API SQLite
snapshot, the blob tree, and a SHA-256/size manifest. The default
`./.osb-backups` host directory separates routine backups from the live Docker
volume but is not a separate host. Point it at an independent disk or NAS for
real failure-domain separation:

```dotenv
OSB_BACKUP_VOLUME=/mnt/backup/open-soverign-blog
```

Monitor free space and
`/healthz.dependencies.backups` independently of `/readyz`. Readiness protects
the request path; it must not restart an otherwise healthy blog merely because
the backup destination is temporarily unavailable. Alert when backup state is
`degraded` or `lastCompletedAt` is older than the configured interval plus the
time needed for one full generation.

The generation intentionally contains canonical SQLite/blob data, not the
operator configuration. Store `config.toml` and the secret-free
`osb.intent.json` in the host's configuration backup so a replacement node
retains its public URL, site ID, intent, and feature contract. Store `.env` and,
when used, `admin-access-key.txt` only in a secrets system; never copy either
into a public backup catalog.

The bundled one-shot storage initializer assigns local backup trees to
container UID/GID `65532`, normalizing directories to `0700` and regular files
to `0600` without following symlinks or crossing nested mounts. Root-squashed
NFS/NAS cannot be repaired by container capabilities: pre-provision the entire
tree with UID/GID `65532`, directories `0700`, and regular files `0600`. The
initializer validates the NFS mount root and leaves its descendants untouched,
so every later copied generation must already preserve that contract.

The commands below remain available for portable, manually named bundles and
restore drills.

Use SQLite's online-backup operation; a raw copy of a live WAL database is not
a consistent backup method. The original `backup` command remains available
for a database-only backup:

```sh
docker compose exec blog osb-cli --database /data/open-soverign-blog.db \
  backup /data/backup.db
```

That command does not include first-party assets. More importantly,
`/data/backup.db` is inside the same named volume as the live database. It is
useful for a short migration step, but **it is not protection against volume or
host loss**. A backup becomes durable only after it is copied to independently
managed storage and tested.

The preferred backup is a bundle containing an online SQLite backup and the
content-addressed blob tree. The manifest records the SHA-256 digest and size
of every payload file; `manifest.json` itself is excluded because a file cannot
contain its own stable digest. The output directory must not already exist.

Create the bundle directly on a host bind mount outside the application volume:

```sh
mkdir -p "$(pwd)/backups"
sudo chown 65532:65532 "$(pwd)/backups"
sudo chmod 0700 "$(pwd)/backups"
docker compose run --rm --no-deps \
  --entrypoint osb-cli \
  -v "$(pwd)/backups:/backup" \
  blog \
  --database /data/open-soverign-blog.db \
  --blob-directory /data/blobs \
  backup-bundle /backup/osb-bundle-2026-07-18
```

The short-lived CLI uses SQLite's online backup API. The blob store is copied
without following links. A symlink, socket, device, non-regular entry, missing
payload, extra payload, changed size, or digest mismatch fails the bundle.
The image runs as UID/GID `65532`, so a newly bind-mounted host destination
must be owned by that identity as shown above (use the remapped UID for a
rootless Docker daemon). A plain user-owned `0755` directory is readable but
not writable from the container.
Keep the bind-mounted backup directory on a different disk, host, or replicated
backup system according to the installation's recovery objective.

Verify a stored bundle without opening its database as a live installation:

```sh
docker compose run --rm --no-deps \
  --entrypoint osb-cli \
  -v "$(pwd)/backups:/backup:ro" \
  blog \
  verify-bundle /backup/osb-bundle-2026-07-18
```

`restore-bundle` always verifies first. It then refuses to proceed if either
the target database or target blob directory already exists; restore never
merges with or overwrites a live installation.

## Restore drill

Run a restore drill into a new host directory, not over `/data`:

```sh
mkdir -p "$(pwd)/restore-drill"
sudo chown 65532:65532 "$(pwd)/restore-drill"
sudo chmod 0700 "$(pwd)/restore-drill"
docker compose run --rm --no-deps \
  --entrypoint osb-cli \
  -v "$(pwd)/backups:/backup:ro" \
  -v "$(pwd)/restore-drill:/restore" \
  blog \
  --database /restore/open-soverign-blog.db \
  --blob-directory /restore/blobs \
  restore-bundle /backup/osb-bundle-2026-07-18
```

The parent `/restore` directory may exist, but
`/restore/open-soverign-blog.db` and `/restore/blobs` must not. After the
command, retain the original bundle and exercise the restored database and
assets with a staging instance. Record the drill date, bundle identifier,
verification result, and observed recovery time.

For a real recovery, stop the application, provision a fresh empty volume or
directory, restore into that empty target, and point the service at it only
after the drill checks pass. Do not remove the damaged or previous volume until
the restored installation has been independently validated.

## Portable export

Create portable Markdown, structured revision JSON, and a verified copy of the
blob tree in a new directory:

```sh
docker compose exec blog osb-cli \
  --database /data/open-soverign-blog.db \
  --blob-directory /data/blobs \
  export 00000000-0000-7000-8000-000000000001 /data/export-1
```

The export writes `assets-manifest.json` with SHA-256 and size entries for the
copied blob files. An export is a portability artifact, not a substitute for a
verified off-volume backup bundle.

The data volume is independent of the image, so replacing or removing the
renderer does not remove content.

The optional code runner is a separate security domain and is intentionally not
included in `compose.yaml`. See `docs/security/CODE-RUNNER.md`.

The optional `osb-mcp` binary is likewise separate from the web server. Its
stdio adapter is read-only by default and only calls the HTTP API. Optional write
mode uses the same dedicated, static 32-128-character Base64url
`OSB_MCP_TOKEN` in the server and MCP process. It is accepted only for the six
content list/read/draft/revise/publish route shapes; it cannot authorize AI2AI,
assets, the runner, Studio settings, administrator auth, or member APIs. An
active administrator module is required, delivery nodes reject the token, and
remote write mode requires HTTPS. Change or remove the environment value and
restart every application replica to rotate or globally revoke it. This is one
global static credential, not per-client issuance. Never reuse an administrator
access key, legacy owner token, OIDC token, or browser cookie. See
[the MCP adapter guide](../apps/mcp/README.md).
