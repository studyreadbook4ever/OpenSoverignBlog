# Verified on-premise updates

OpenSoverignBlog treats an update as a data migration transaction, not as a
script download. `scripts/osb-update.sh` is a Linux-only local orchestrator. It
fetches release metadata and immutable Git objects from the canonical
repository, but it never executes remotely fetched shell content and never
loads a deployment `.env` into a shell.

There is currently no published tag or GitHub Release. Consequently the
checked-in stable channel has `latest: null`, and this command exits successfully
in explicit check-only mode without changing the deployment:

```sh
scripts/osb-update.sh --deployment /srv/osb/my-blog
```

Publishing a date in a commit is not a release. A release exists only when all
of these agree: `release.toml`, every Cargo/npm/OpenAPI/SBOM version, a channel
entry, an immutable annotated signed `vX.Y.Z` tag, and its GitHub Release.
`manifestSha256` is the SHA-256 of the exact `release.toml` bytes in that signed
tag. CI rejects version or license drift.

## Host prerequisites and trust

The updater requires Linux, Docker Engine with Compose v2, Git, GnuPG, Python 3.10+,
and `flock`. The application build itself happens in Docker. Run it from a
trusted checkout of the version already installed; do not replace it with a
command copied from a web page.

Apply is supported only for a tracked bootstrap deployment containing
`osb.install.toml`, `osb.lock.json`, and the matching digest in `.env`. For an
older pre-lock deployment, first review and run `osb installation adopt` from
the currently installed engine; the updater never guesses structural choices.
Before staging, it also requires the running container's named `/data` volume
and `/backups` bind to exactly match the tracked `OSB_DATA_VOLUME` and canonical
absolute `OSB_BACKUP_VOLUME`; drift fails closed instead of backing up or
migrating an unexpected path.
This in-place transaction targets writable origin deployments. Immutable
delivery nodes continue to receive a newly verified origin backup generation
through their recorded restore/start handoff instead of creating a local
authoritative backup.

An apply additionally requires the release publisher's full tag-signing
fingerprint from an out-of-band trusted source and that public key in the local
GnuPG keyring. The fingerprint supplied by the release channel is deliberately
not trusted. Until the project publishes that key through a separate trust
bootstrap, there is no unattended official apply path.

```sh
gpg --import /secure/path/open-soverign-blog-release-key.asc

scripts/osb-update.sh \
  --deployment /srv/osb/my-blog \
  --apply --yes \
  --trusted-tag-fingerprint FULL_HEX_FINGERPRINT
```

Patch and minor releases within the installed major version are selected by
default. A major transition is shown but not selected unless the operator adds
`--allow-major`. `--target X.Y.Z` must name an exact, newer entry in the stable
channel; downgrades, current-version reinstalls, prereleases, and build-metadata
versions are refused. `--offline` is useful for validating the bundled empty
channel, but can never apply an update.

## Transaction and rollback boundary

An apply holds an exclusive `flock` on the already validated `.osb-update`
directory inode and performs this sequence. Locking the directory avoids
opening or truncating an attacker-precreated lock-file symlink.

1. Validate the installation intent/lock and take a mode-0700, mode-0600-file
   snapshot of `.env`, config, CSS, install intent/lock, semantic intent, and the
   optional administrator access-key file. Values are copied byte-for-byte and
   never printed.
2. Verify the canonical channel contract, exact signed tag commit, explicitly
   trusted signer fingerprint, presence of the canonical GitHub Release, tagged
   `release.toml` hash, and OCI version/revision/license labels.
3. Build the signed source in a detached staging worktree. Clone the controls
   into a transaction-only directory, then run the target's typed
   `osb installation dlc reconcile` helper there. This updates the target
   engine/config/database/plugin compatibility tuple, resolves every requested
   DLC only from official manifests bundled in that candidate, and updates its
   staged environment. The candidate image's immutable SHA-256 ID is recorded
   in the lock. An unavailable or incompatible DLC fails before downtime; the
   live controls remain untouched. The updater clears inherited Compose
   profiles, enables only the cache profile recorded in the lock, renders the
   target Compose model, and rejects unexpected host binds, external/aliased
   volumes, `volumes_from`, privileged services, host devices, or any reference
   to the last-known-good data volume before starting a service.
4. Prepare an empty candidate volume, re-check every protected control, arm
   rollback, and stop the old blog container. The updater then uses the old
   immutable image to create and verify a quiesced SQLite plus
   content-addressed-blob bundle under the configured
   `OSB_BACKUP_VOLUME/update-rollbacks/<transaction>/data`. Quiescing closes
   the write-loss window that would exist between an online backup and a later
   stop; planned downtime begins here.
5. Restore that exact verified bundle with the old immutable image into the new
   Docker volume. The candidate image performs migrations only against this
   clone; the last-known-good volume is never migrated. A backup or restore
   failure is already inside the rollback boundary and restarts the old pair.
6. Start the candidate with its staged lock/digest and require exact
   target-version `/livez` and `/readyz`, healthy `/healthz`, and a valid public
   feed. Only then re-verify the staged lock with the target CLI, atomically
   promote that lock and reconciled environment, restart against the live
   controls, and repeat all health checks.
7. Store the released Compose/Redis bundle under
   `.osb-update/runtime/vX.Y.Z`, atomically move `current`, and record a
   secret-free transaction journal.

Do not edit deployment controls during this transaction. The updater compares
every live control with the protected snapshot immediately before switching and
again before promotion; an out-of-band change aborts instead of being silently
mixed with a release migration.

The updater-owned `.env` keys are `OSB_IMAGE`, `OSB_DATA_VOLUME`, and
`OSB_INSTALL_LOCK_DIGEST`; typed DLC reconciliation may also update the exact
DLC/feature projection. Bootstrap assigns a deployment-unique data-volume name.
The Compose fallback retains the historical
`<compose-project>_osb-data` name for deployments created before that key
existed.

Any handled command, container, or health-check failure after rollback is armed
and the old blog is stopped restores the protected control snapshot and
restarts the previous immutable image against the untouched previous data
volume. A failed candidate volume and any completed verified backup are retained
for diagnosis; an incomplete backup directory is retained as evidence but must
not be used for restore. The updater does not erase recovery evidence. A
container that still consumes the old volume makes automatic rollback stop for
manual intervention; it never starts a second SQLite writer on that volume. A
host power loss or `SIGKILL` cannot execute a shell trap. After either, do not
immediately rerun the updater: inspect the last transaction journal, live
lock/env, both retained volumes, and the protected snapshot, then explicitly
select the old pair or complete the recorded target pair. If automatic rollback
also fails, preserve the same evidence and restore the verified bundle on a
fresh host when necessary.

After success, keep the old volume and update backup until a separate restore
drill and an operator-defined rollback window have passed. Docker pruning is
not part of the updater. For host-loss protection, keep `OSB_BACKUP_VOLUME` on
an independently backed disk or NAS; two volumes on one Docker host are not a
second failure domain.

Use the committed bundle for later lifecycle commands:

```sh
docker compose \
  -p osb-019f7979723a75c08e9711c3b13b004d \
  --env-file /srv/osb/my-blog/.env \
  -f /srv/osb/my-blog/.osb-update/current/compose.yaml \
  up -d --wait
```

Use the exact `composeProject` recorded in `osb.intent.json` for `-p`; do not
invent a new project name, because that would select different Redis volumes
and containers.

Run the local policy and contract tests without Docker or network access:

```sh
npm run test:updater
```

The test runs POSIX shell syntax checks, optional ShellCheck when installed,
static no-remote-shell/no-`.env`-sourcing policy checks, Python contract tests,
snapshot/restore integrity tests, and the real empty-channel check path.
