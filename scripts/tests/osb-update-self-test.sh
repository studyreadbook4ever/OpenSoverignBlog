#!/bin/sh
set -eu
umask 077

TEST_DIRECTORY=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
REPOSITORY_ROOT=$(CDPATH='' cd -- "$TEST_DIRECTORY/../.." && pwd -P)
UPDATER="$REPOSITORY_ROOT/scripts/osb-update.sh"
SUPPORT="$REPOSITORY_ROOT/scripts/osb-update-support.py"

sh -n "$UPDATER"
python3 -c 'import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))' "$SUPPORT"

if command -v shellcheck >/dev/null 2>&1; then
    shellcheck -s sh "$UPDATER" "$0"
fi

if grep -En '(^|[;&])[[:space:]]*(source|\.)[[:space:]]+[^#]*\.env' "$UPDATER"; then
    echo >&2 "updater policy failure: deployment .env must never be sourced"
    exit 1
fi
if grep -En '(curl|wget)[^#]*\|[[:space:]]*(sh|bash)([[:space:]]|$)' "$UPDATER"; then
    echo >&2 "updater policy failure: remote content must never be piped to a shell"
    exit 1
fi
grep -F 'installation dlc reconcile' "$UPDATER" >/dev/null
# This assertion intentionally matches literal Compose interpolation syntax.
# shellcheck disable=SC2016
grep -F 'name: "${OSB_DATA_VOLUME:-${COMPOSE_PROJECT_NAME:-open-soverign-blog}_osb-data}"' \
    "$REPOSITORY_ROOT/compose.yaml" >/dev/null

# The authoritative database must stop before the rollback bundle is created.
# Keep this ordering check explicit so a future "online backup" refactor cannot
# reopen the interval in which acknowledged writes disappear from a candidate.
python3 - "$UPDATER" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
markers = (
    "ROLLBACK_ARMED=true",
    "compose_invoke stop blog",
    'backup-bundle "$backup_container_path"',
    'verify-bundle "$backup_container_path"',
    'restore "$backup_container_path"',
    "ACTIVE_CONTROL_ROOT=$CANDIDATE_CONTROL\ncompose_invoke up -d --wait --wait-timeout 180",
)
positions = []
for marker in markers:
    if source.count(marker) != 1:
        raise SystemExit(f"updater transaction marker must occur exactly once: {marker}")
    positions.append(source.index(marker))
if positions != sorted(positions) or len(set(positions)) != len(positions):
    raise SystemExit("updater must arm rollback, stop, back up, verify, restore, then start candidate")
if 'compose_invoke exec -T blog osb backup-bundle' in source:
    raise SystemExit("updater must not create its transition backup while the blog is running")
if source.count('assert_running_pair "$TARGET_IMAGE_ID" "$CANDIDATE_DATA_VOLUME"') != 2:
    raise SystemExit("candidate and promoted blog must both prove their actual image/data pair")
if source.count('compose-plan-verify') != 2:
    raise SystemExit("candidate and promoted Compose plans must both be isolated before start")
if source.count("COMPOSE_PROFILES=") != 3:
    raise SystemExit("every Compose path must clear inherited profiles before selecting the lock profile")
if 'exec 9> "$STATE_DIRECTORY/update.lock"' in source or 'exec 9< "$STATE_DIRECTORY"' not in source:
    raise SystemExit("updater must flock the validated state-directory inode without truncating a path")
for verifier_setting in ("gpg.format=openpgp", "gpg.program=$GPG_PROGRAM", "gpg.openpgp.program=$GPG_PROGRAM"):
    if verifier_setting not in source:
        raise SystemExit(f"signed tag verification is not pinned to local OpenPGP: {verifier_setting}")
rollback = source[source.index("rollback() {"):source.index("cleanup_temp() {")]
if rollback.index('assert_volume_quiesced "$CURRENT_DATA_VOLUME"') > rollback.index(
    "compose_invoke up -d --wait --wait-timeout 180"
):
    raise SystemExit("rollback must prove the old data volume is consumer-free before restart")
PY

python3 "$SUPPORT" self-test

TEST_TEMP=$(mktemp -d "${TMPDIR:-/tmp}/osb-update-test.XXXXXXXX")
cleanup() {
    case "$TEST_TEMP" in
        "${TMPDIR:-/tmp}"/osb-update-test.*) rm -rf -- "$TEST_TEMP" ;;
        *) echo >&2 "refusing to remove unexpected test path: $TEST_TEMP" ;;
    esac
}
trap cleanup EXIT HUP INT TERM

python3 "$SUPPORT" lock-info \
    --file "$REPOSITORY_ROOT/osb.lock.example.json" \
    --output "$TEST_TEMP/example-lock"
grep -Fx '0.1.1' "$TEST_TEMP/example-lock/version" >/dev/null
grep -Fx 'redis_managed' "$TEST_TEMP/example-lock/cache" >/dev/null

"$UPDATER" --offline --check > "$TEST_TEMP/check.log"
grep -F 'stable channel has no published release; check-only, no files changed' \
    "$TEST_TEMP/check.log" >/dev/null

echo "osb updater self-test: ok"
