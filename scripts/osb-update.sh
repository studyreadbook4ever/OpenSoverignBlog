#!/bin/sh
# Linux-only, fail-closed OpenSoverignBlog release updater.
#
# Remote content is treated strictly as data.  This script fetches immutable
# Git objects, verifies an annotated signed tag and release contract, builds a
# local image, restores a verified backup into a fresh candidate volume, and
# commits only after two exact-version health checks.  It never executes a
# downloaded script and never evaluates a deployment `.env` with a shell.

set -eu
umask 077

SCRIPT_DIRECTORY=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPOSITORY_ROOT=$(CDPATH= cd -- "$SCRIPT_DIRECTORY/.." && pwd -P)
SUPPORT="$SCRIPT_DIRECTORY/osb-update-support.py"
REPOSITORY_URL="https://github.com/studyreadbook4ever/OpenSoverignBlog"
REMOTE_GIT_URL="$REPOSITORY_URL.git"

DEPLOYMENT=$(pwd -P)
ACTION=check
CONFIRMED=false
ALLOW_MAJOR=false
OFFLINE=false
REQUESTED_TARGET=
TRUSTED_FINGERPRINT=${OSB_UPDATE_TRUSTED_TAG_FINGERPRINT:-}
SELF_TEST=false

TEMP_ROOT=
TEMP_BASE=${TMPDIR:-/tmp}
ROLLBACK_ARMED=false
COMMITTED=false
ROLLBACK_ACTIVE=false
CONTROL_SNAPSHOT=
CURRENT_COMPOSE=
CURRENT_ENV=
CURRENT_VERSION=
CURRENT_IMAGE_ID=
CURRENT_DATA_VOLUME=
CURRENT_LOCK_DIGEST=
TRACKED_DATA_VOLUME=
TRACKED_LOCK_DIGEST=
COMPOSE_PROJECT=
BACKUP_ROOT=
CACHE_MODE=none
ACTIVE_CACHE_PROFILE=none
ACTIVE_COMPOSE=
ACTIVE_ENV=
ACTIVE_CONTROL_ROOT=
PREVIOUS_RUNTIME_TARGET=
PREVIOUS_RUNTIME_PRESENT=false
STATE_DIRECTORY=
JOURNAL=
NEW_RUNTIME_DIRECTORY=
PAIR_CONTAINER=

usage() {
    cat <<'EOF'
Usage: scripts/osb-update.sh [OPTIONS]

The default is a read-only stable-channel check. Applying an update requires
both --apply and --yes. Patch/minor updates are eligible by default; crossing a
major version also requires --allow-major.

Options:
  --deployment DIR              Bootstrap deployment directory (default: cwd)
  --check                       Check and print the semantic plan (default)
  --apply                       Build, stage, verify, and switch an update
  --yes                         Confirm the planned apply transaction
  --target X.Y.Z                Select an exact published stable version
  --allow-major                 Permit a major-version boundary
  --offline                     Use the bundled channel; apply is prohibited
  --trusted-tag-fingerprint HEX Required signer fingerprint for apply
  --self-test                   Run local dependency-free contract tests
  -h, --help                    Show this help

The updater never sources .env and never downloads executable shell content.
EOF
}

die() {
    printf 'osb-update: %s\n' "$*" >&2
    exit 2
}

note() {
    printf 'osb-update: %s\n' "$*"
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command is missing: $1"
}

is_image_id() {
    case "$1" in
        sha256:*) image_id_digest=${1#sha256:} ;;
        *) return 1 ;;
    esac
    [ "${#image_id_digest}" -eq 64 ] || return 1
    case "$image_id_digest" in *[!a-f0-9]*) return 1 ;; esac
    return 0
}

read_field() {
    field_path=$1
    [ -f "$field_path" ] || die "validated plan field is missing: $field_path"
    IFS= read -r field_value < "$field_path" || die "validated plan field is empty: $field_path"
    printf '%s' "$field_value"
}

journal() {
    [ -n "$JOURNAL" ] || return 0
    journal_time=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    printf '%s\t%s\n' "$journal_time" "$*" >> "$JOURNAL"
}

compose_invoke() {
    [ -n "$ACTIVE_COMPOSE" ] || die "internal error: no active Compose bundle"
    [ -n "$ACTIVE_ENV" ] || die "internal error: no active environment file"
    [ -n "$ACTIVE_CONTROL_ROOT" ] || die "internal error: no active control root"
    compose_config_source="$ACTIVE_CONTROL_ROOT/config.toml"
    compose_install_source="$ACTIVE_CONTROL_ROOT/osb.install.toml"
    compose_lock_source="$ACTIVE_CONTROL_ROOT/osb.lock.json"
    compose_css_source="$ACTIVE_CONTROL_ROOT/custom.css"
    [ -n "$BACKUP_ROOT" ] || die "internal error: no validated backup root"
    compose_backup_source=$BACKUP_ROOT
    case "$CACHE_MODE" in
        redis_managed)
            env \
                COMPOSE_PROFILES= \
                OSB_CONFIG_SOURCE="$compose_config_source" \
                OSB_INSTALL_SOURCE="$compose_install_source" \
                OSB_LOCK_SOURCE="$compose_lock_source" \
                OSB_CUSTOM_CSS_SOURCE="$compose_css_source" \
                OSB_BACKUP_VOLUME="$compose_backup_source" \
                docker compose -p "$COMPOSE_PROJECT" --env-file "$ACTIVE_ENV" -f "$ACTIVE_COMPOSE" \
                --profile redis-managed "$@"
            ;;
        redis_standalone)
            env \
                COMPOSE_PROFILES= \
                OSB_CONFIG_SOURCE="$compose_config_source" \
                OSB_INSTALL_SOURCE="$compose_install_source" \
                OSB_LOCK_SOURCE="$compose_lock_source" \
                OSB_CUSTOM_CSS_SOURCE="$compose_css_source" \
                OSB_BACKUP_VOLUME="$compose_backup_source" \
                docker compose -p "$COMPOSE_PROJECT" --env-file "$ACTIVE_ENV" -f "$ACTIVE_COMPOSE" \
                --profile redis-standalone "$@"
            ;;
        none)
            env \
                COMPOSE_PROFILES= \
                OSB_CONFIG_SOURCE="$compose_config_source" \
                OSB_INSTALL_SOURCE="$compose_install_source" \
                OSB_LOCK_SOURCE="$compose_lock_source" \
                OSB_CUSTOM_CSS_SOURCE="$compose_css_source" \
                OSB_BACKUP_VOLUME="$compose_backup_source" \
                docker compose -p "$COMPOSE_PROJECT" --env-file "$ACTIVE_ENV" -f "$ACTIVE_COMPOSE" "$@"
            ;;
        *) die "installation lock contains an unsupported cache mode" ;;
    esac
}

assert_container_pair() {
    inspected_container=$1
    expected_image=$2
    expected_volume=$3
    case "$inspected_container" in
        ''|*[!a-fA-F0-9]*)
            printf 'osb-update: blog container identifier is missing or invalid\n' >&2
            return 1
            ;;
    esac
    pair_image=$(docker inspect --format '{{.Image}}' "$inspected_container") || return 1
    pair_volume=$(docker inspect --format \
        '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Name}}{{end}}{{end}}' \
        "$inspected_container") || return 1
    if [ "$pair_image" != "$expected_image" ] || [ "$pair_volume" != "$expected_volume" ]; then
        printf 'osb-update: blog is not using the expected immutable image/data pair\n' >&2
        return 1
    fi
    return 0
}

assert_running_pair() {
    expected_image=$1
    expected_volume=$2
    PAIR_CONTAINER=$(compose_invoke ps -q blog) || return 1
    assert_container_pair "$PAIR_CONTAINER" "$expected_image" "$expected_volume"
}

assert_volume_quiesced() {
    quiesced_volume=$1
    volume_consumers=$(docker ps --quiet --filter "volume=$quiesced_volume") || return 1
    if [ -n "$volume_consumers" ]; then
        printf 'osb-update: authoritative data volume still has a running container consumer\n' >&2
        return 1
    fi
    return 0
}

capture_health() {
    expected_version=$1
    health_directory=$2
    mkdir -m 700 -p "$health_directory"
    attempt=1
    while [ "$attempt" -le 40 ]; do
        if compose_invoke exec -T blog curl --fail --silent --show-error \
            http://127.0.0.1:8787/livez > "$health_directory/livez.json" 2>/dev/null \
            && compose_invoke exec -T blog curl --fail --silent --show-error \
            http://127.0.0.1:8787/readyz > "$health_directory/readyz.json" 2>/dev/null \
            && compose_invoke exec -T blog curl --fail --silent --show-error \
            http://127.0.0.1:8787/healthz > "$health_directory/healthz.json" 2>/dev/null \
            && compose_invoke exec -T blog curl --fail --silent --show-error \
            http://127.0.0.1:8787/api/v1/feed > "$health_directory/feed.json" 2>/dev/null \
            && python3 "$SUPPORT" health \
                --expected "$expected_version" \
                --live "$health_directory/livez.json" \
                --ready "$health_directory/readyz.json" \
                --health "$health_directory/healthz.json" \
                --feed "$health_directory/feed.json"
        then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 3
    done
    return 1
}

restore_runtime_pointer() {
    [ -n "$STATE_DIRECTORY" ] || return 0
    if [ "$PREVIOUS_RUNTIME_PRESENT" = true ]; then
        pointer_temp="$STATE_DIRECTORY/.current.rollback.$$"
        ln -s "$PREVIOUS_RUNTIME_TARGET" "$pointer_temp" || return 1
        mv -f "$pointer_temp" "$STATE_DIRECTORY/current" || return 1
    else
        if [ -L "$STATE_DIRECTORY/current" ]; then
            rm -f "$STATE_DIRECTORY/current" || return 1
        fi
    fi
}

rollback() {
    [ "$ROLLBACK_ACTIVE" = false ] || return 1
    ROLLBACK_ACTIVE=true
    set +e
    note "candidate failed; restoring the protected control snapshot and last-known-good image"
    journal "rollback-start"
    rollback_failed=false

    python3 "$SUPPORT" restore --deployment "$DEPLOYMENT" --snapshot "$CONTROL_SNAPSHOT"
    [ "$?" -eq 0 ] || rollback_failed=true
    if [ -n "$NEW_RUNTIME_DIRECTORY" ] && [ -d "$NEW_RUNTIME_DIRECTORY" ]; then
        mv "$NEW_RUNTIME_DIRECTORY" "$TRANSACTION_DIRECTORY/failed-runtime"
        [ "$?" -eq 0 ] || rollback_failed=true
    fi
    restore_runtime_pointer
    [ "$?" -eq 0 ] || rollback_failed=true

    rollback_env="$STATE_DIRECTORY/transactions/rollback-$$.env"
    python3 "$SUPPORT" env-rewrite \
        --source "$DEPLOYMENT/.env" \
        --output "$rollback_env" \
        --set "OSB_IMAGE=$CURRENT_IMAGE_ID" \
        --set "OSB_DATA_VOLUME=$CURRENT_DATA_VOLUME" \
        --set "OSB_INSTALL_LOCK_DIGEST=$CURRENT_LOCK_DIGEST"
    if [ "$?" -eq 0 ]; then
        if assert_volume_quiesced "$CURRENT_DATA_VOLUME"; then
            ACTIVE_COMPOSE=$CURRENT_COMPOSE
            ACTIVE_ENV=$rollback_env
            ACTIVE_CONTROL_ROOT=$DEPLOYMENT
            compose_invoke up -d --wait --wait-timeout 180
            [ "$?" -eq 0 ] || rollback_failed=true
            assert_running_pair "$CURRENT_IMAGE_ID" "$CURRENT_DATA_VOLUME"
            [ "$?" -eq 0 ] || rollback_failed=true
            capture_health "$CURRENT_VERSION" "$STATE_DIRECTORY/transactions/rollback-health-$$"
            [ "$?" -eq 0 ] || rollback_failed=true
        else
            journal "rollback-old-volume-not-quiescent"
            rollback_failed=true
        fi
    else
        rollback_failed=true
    fi

    if [ "$rollback_failed" = true ]; then
        journal "rollback-failed manual-intervention-required"
        note "automatic rollback did not become healthy; preserve the transaction directory and follow the recovery runbook"
        return 1
    fi
    journal "rollback-complete image=$CURRENT_IMAGE_ID volume=$CURRENT_DATA_VOLUME"
    note "last-known-good image and data volume are healthy again"
    return 0
}

cleanup_temp() {
    [ -n "$TEMP_ROOT" ] || return 0
    case "$TEMP_ROOT" in
        "$TEMP_BASE"/osb-update.*)
            rm -rf -- "$TEMP_ROOT"
            ;;
        *)
            printf 'osb-update: refusing to remove unexpected temporary path %s\n' "$TEMP_ROOT" >&2
            ;;
    esac
}

finish() {
    result=$?
    trap - EXIT HUP INT TERM
    if [ "$ROLLBACK_ARMED" = true ] && [ "$COMMITTED" = false ]; then
        if ! rollback; then
            result=1
        fi
    fi
    cleanup_temp
    exit "$result"
}

trap finish EXIT
trap 'exit 130' HUP INT TERM

while [ "$#" -gt 0 ]; do
    case "$1" in
        --deployment)
            [ "$#" -ge 2 ] || die "--deployment requires a directory"
            DEPLOYMENT=$2
            shift 2
            ;;
        --check)
            ACTION=check
            shift
            ;;
        --apply)
            ACTION=apply
            shift
            ;;
        --yes)
            CONFIRMED=true
            shift
            ;;
        --target)
            [ "$#" -ge 2 ] || die "--target requires X.Y.Z"
            REQUESTED_TARGET=$2
            shift 2
            ;;
        --allow-major)
            ALLOW_MAJOR=true
            shift
            ;;
        --offline)
            OFFLINE=true
            shift
            ;;
        --trusted-tag-fingerprint)
            [ "$#" -ge 2 ] || die "--trusted-tag-fingerprint requires HEX"
            TRUSTED_FINGERPRINT=$2
            shift 2
            ;;
        --self-test)
            SELF_TEST=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            [ "$#" -eq 0 ] || die "positional arguments are not accepted"
            ;;
        *) die "unknown option: $1" ;;
    esac
done

[ "$(uname -s)" = Linux ] || die "this updater supports Linux hosts only"
require_command python3
python3 -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 10) else 1)' \
    || die "Python 3.10 or newer is required"
[ -f "$SUPPORT" ] || die "missing local updater support module: $SUPPORT"
[ -d "$DEPLOYMENT" ] || die "deployment directory does not exist: $DEPLOYMENT"
[ ! -L "$DEPLOYMENT" ] || die "deployment directory itself must not be a symlink"
DEPLOYMENT=$(CDPATH= cd -- "$DEPLOYMENT" && pwd -P)
case "$DEPLOYMENT" in *:*) die "deployment path cannot contain ':'" ;; esac

if [ "$SELF_TEST" = true ]; then
    [ "$ACTION" = check ] || die "--self-test cannot be combined with --apply"
    python3 "$SUPPORT" self-test
    note "shell orchestration self-test: ok"
    exit 0
fi

if [ "$OFFLINE" = false ]; then
    require_command git
fi
require_command mktemp
[ -d "$TEMP_BASE" ] || die "temporary directory does not exist: $TEMP_BASE"
TEMP_BASE=$(CDPATH= cd -- "$TEMP_BASE" && pwd -P)
[ "$TEMP_BASE" != / ] || die "refusing to place updater worktrees directly under /"
TEMP_ROOT=$(mktemp -d "$TEMP_BASE/osb-update.XXXXXXXX")
chmod 700 "$TEMP_ROOT"

if [ -f "$DEPLOYMENT/osb.lock.json" ]; then
    python3 "$SUPPORT" lock-info \
        --file "$DEPLOYMENT/osb.lock.json" --output "$TEMP_ROOT/current-lock"
    CURRENT_VERSION=$(read_field "$TEMP_ROOT/current-lock/version")
else
    python3 "$SUPPORT" source-info \
        --file "$REPOSITORY_ROOT/release.toml" --output "$TEMP_ROOT/current-source"
    CURRENT_VERSION=$(read_field "$TEMP_ROOT/current-source/version")
fi

CHANNEL_FILE="$TEMP_ROOT/release-channel.json"
CHANNEL_ORIGIN=bundled
if [ "$OFFLINE" = true ]; then
    cp "$REPOSITORY_ROOT/release-channel.json" "$CHANNEL_FILE"
else
    git init --bare --quiet "$TEMP_ROOT/repository.git"
    if git --git-dir="$TEMP_ROOT/repository.git" fetch --quiet --no-tags "$REMOTE_GIT_URL" \
        "+refs/heads/main:refs/heads/osb-channel" \
        && git --git-dir="$TEMP_ROOT/repository.git" show \
        "refs/heads/osb-channel:release-channel.json" > "$CHANNEL_FILE" 2>/dev/null
    then
        CHANNEL_ORIGIN=remote
    else
        note "remote main has no readable release channel; using the bundled check contract"
        cp "$REPOSITORY_ROOT/release-channel.json" "$CHANNEL_FILE"
    fi
fi

set -- python3 "$SUPPORT" channel-plan \
    --channel "$CHANNEL_FILE" --current "$CURRENT_VERSION" --output "$TEMP_ROOT/plan"
[ -n "$REQUESTED_TARGET" ] && set -- "$@" --target "$REQUESTED_TARGET"
[ "$ALLOW_MAJOR" = true ] && set -- "$@" --allow-major
"$@"

PLAN_STATUS=$(read_field "$TEMP_ROOT/plan/status")
case "$PLAN_STATUS" in
    no_release)
        note "stable channel has no published release; check-only, no files changed"
        exit 0
        ;;
    up_to_date)
        note "version $CURRENT_VERSION is current; no files changed"
        exit 0
        ;;
    major_available)
        latest_version=$(read_field "$TEMP_ROOT/plan/latest-version")
        note "stable $latest_version is available across a major boundary; rerun with --allow-major after review"
        note "check-only, no files changed"
        exit 0
        ;;
    update_available) ;;
    *) die "unsupported semantic plan status: $PLAN_STATUS" ;;
esac

TARGET_VERSION=$(read_field "$TEMP_ROOT/plan/target-version")
TARGET_TAG=$(read_field "$TEMP_ROOT/plan/target-tag")
TARGET_RELEASE_DATE=$(read_field "$TEMP_ROOT/plan/target-release-date")
TARGET_COMMIT=$(read_field "$TEMP_ROOT/plan/target-source-commit")
TARGET_RELEASE_URL=$(read_field "$TEMP_ROOT/plan/target-release-url")
TARGET_MANIFEST_SHA256=$(read_field "$TEMP_ROOT/plan/target-manifest-sha256")
note "plan: $CURRENT_VERSION -> $TARGET_VERSION ($TARGET_TAG, $TARGET_RELEASE_DATE)"

if [ "$ACTION" = check ]; then
    note "check-only, no files changed; use --apply --yes to stage this exact release"
    exit 0
fi
[ "$CONFIRMED" = true ] || die "apply requires explicit --yes after reviewing the plan"
[ "$OFFLINE" = false ] || die "offline metadata is never accepted for apply"
[ "$CHANNEL_ORIGIN" = remote ] || die "apply requires a release channel fetched from canonical remote main"

case "$TRUSTED_FINGERPRINT" in
    ''|*[!0-9A-Fa-f]*) die "apply requires a hexadecimal trusted tag fingerprint" ;;
esac
fingerprint_length=${#TRUSTED_FINGERPRINT}
[ "$fingerprint_length" -eq 40 ] || [ "$fingerprint_length" -eq 64 ] \
    || die "trusted tag fingerprint must be 40 or 64 hexadecimal characters"
TRUSTED_FINGERPRINT=$(printf '%s' "$TRUSTED_FINGERPRINT" | tr 'a-f' 'A-F')

for required in flock docker gpg curl date grep awk sed tr cut id readlink env; do
    require_command "$required"
done
docker compose version >/dev/null 2>&1 || die "Docker Compose v2 is required"
GPG_PROGRAM=$(command -v gpg)
case "$GPG_PROGRAM" in /*) ;; *) die "gpg must resolve to an absolute executable path" ;; esac
GPG_PROGRAM=$(readlink -f -- "$GPG_PROGRAM")
[ -f "$GPG_PROGRAM" ] && [ -x "$GPG_PROGRAM" ] \
    || die "gpg must resolve to one executable regular file"
if docker info --format '{{json .SecurityOptions}}' | grep -F 'rootless' >/dev/null 2>&1; then
    # Root in a rootless daemon maps back to the invoking host account and can
    # perform same-directory atomic replacement on its bind-mounted files.
    CONTROL_CONTAINER_USER=0:0
else
    CONTROL_CONTAINER_USER="$(id -u):$(id -g)"
fi

[ "$DEPLOYMENT" != / ] || die "deployment directory cannot be /"
[ "$DEPLOYMENT" != "$(CDPATH= cd -- "${HOME:?}" && pwd -P)" ] || die "deployment directory cannot be HOME"
STATE_DIRECTORY="$DEPLOYMENT/.osb-update"
if [ -L "$STATE_DIRECTORY" ]; then
    die "update state directory must not be a symlink"
fi
for state_child in transactions runtime; do
    [ ! -L "$STATE_DIRECTORY/$state_child" ] \
        || die "update state child must not be a symlink: $state_child"
done
mkdir -m 700 -p "$STATE_DIRECTORY/transactions" "$STATE_DIRECTORY/runtime"
chmod 700 "$STATE_DIRECTORY" "$STATE_DIRECTORY/transactions" "$STATE_DIRECTORY/runtime"
# Lock the already-validated state directory inode itself. Opening a writable
# path here would let a pre-created update.lock symlink truncate its target.
exec 9< "$STATE_DIRECTORY"
flock -n 9 || die "another updater transaction holds $STATE_DIRECTORY"

CURRENT_ENV="$DEPLOYMENT/.env"
python3 "$SUPPORT" env-info --file "$CURRENT_ENV" --output "$TEMP_ROOT/env-info"
COMPOSE_PROJECT=$(read_field "$TEMP_ROOT/env-info/compose-project")
BACKUP_ROOT=$(read_field "$TEMP_ROOT/env-info/backup-volume")
TRACKED_DATA_VOLUME=$(read_field "$TEMP_ROOT/env-info/data-volume")
DELIVERY_ONLY=$(read_field "$TEMP_ROOT/env-info/delivery-only")
TRACKED_LOCK_DIGEST=$(read_field "$TEMP_ROOT/env-info/lock-digest")
[ "$DELIVERY_ONLY" = false ] \
    || die "in-place updates are for writable origins; update a delivery node through its verified restore handoff"
python3 "$SUPPORT" lock-info \
    --file "$DEPLOYMENT/osb.lock.json" --output "$TEMP_ROOT/apply-lock"
apply_version=$(read_field "$TEMP_ROOT/apply-lock/version")
[ "$apply_version" = "$CURRENT_VERSION" ] || die "installation lock changed after planning"
CURRENT_LOCK_DIGEST=$(read_field "$TEMP_ROOT/apply-lock/digest")
[ "$TRACKED_LOCK_DIGEST" = "$CURRENT_LOCK_DIGEST" ] \
    || die "tracked OSB_INSTALL_LOCK_DIGEST differs from the canonical installation lock"
CACHE_MODE=$(read_field "$TEMP_ROOT/apply-lock/cache")
case "$CACHE_MODE" in
    none) ACTIVE_CACHE_PROFILE=none ;;
    redis_standalone) ACTIVE_CACHE_PROFILE=redis-standalone ;;
    redis_managed) ACTIVE_CACHE_PROFILE=redis-managed ;;
    *) die "installation lock contains an unsupported cache mode" ;;
esac

if [ -L "$STATE_DIRECTORY/current" ]; then
    PREVIOUS_RUNTIME_TARGET=$(readlink "$STATE_DIRECTORY/current")
    runtime_leaf=${PREVIOUS_RUNTIME_TARGET#runtime/}
    case "$PREVIOUS_RUNTIME_TARGET:$runtime_leaf" in
        runtime/v*:v[0-9]*.[0-9]*.[0-9]*) ;;
        *) die "current runtime pointer is outside the managed runtime tree" ;;
    esac
    case "$runtime_leaf" in
        *[!v0-9.]*|*..*|*/*) die "current runtime pointer has an unsafe release name" ;;
    esac
    [ -d "$STATE_DIRECTORY/$PREVIOUS_RUNTIME_TARGET" ] \
        && [ ! -L "$STATE_DIRECTORY/$PREVIOUS_RUNTIME_TARGET" ] \
        || die "current runtime target must be one real managed directory"
    PREVIOUS_RUNTIME_PRESENT=true
    CURRENT_COMPOSE="$STATE_DIRECTORY/current/compose.yaml"
else
    [ ! -e "$STATE_DIRECTORY/current" ] || die "current runtime pointer must be a managed symlink"
    CURRENT_COMPOSE="$REPOSITORY_ROOT/compose.yaml"
fi
[ -f "$CURRENT_COMPOSE" ] && [ ! -L "$CURRENT_COMPOSE" ] \
    || die "current Compose bundle must be one real file: $CURRENT_COMPOSE"

ACTIVE_COMPOSE=$CURRENT_COMPOSE
ACTIVE_ENV=$CURRENT_ENV
ACTIVE_CONTROL_ROOT=$DEPLOYMENT
current_container=$(compose_invoke ps -q blog)
[ -n "$current_container" ] || die "the current blog container is not running"
case "$current_container" in *[!a-fA-F0-9]*) die "unexpected blog container identifier" ;; esac
CURRENT_IMAGE_ID=$(docker inspect --format '{{.Image}}' "$current_container")
is_image_id "$CURRENT_IMAGE_ID" \
    || die "current image is not addressable by an exact immutable SHA-256 image ID"
CURRENT_DATA_VOLUME=$(docker inspect --format \
    '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Name}}{{end}}{{end}}' "$current_container")
case "$CURRENT_DATA_VOLUME" in
    ''|*[!A-Za-z0-9_.-]*) die "current /data mount must be one named Docker volume" ;;
esac
[ "$CURRENT_DATA_VOLUME" = "$TRACKED_DATA_VOLUME" ] \
    || die "running /data mount differs from the tracked OSB_DATA_VOLUME"
current_backup_root=$(docker inspect --format \
    '{{range .Mounts}}{{if eq .Destination "/backups"}}{{.Source}}{{end}}{{end}}' "$current_container")
[ "$current_backup_root" = "$BACKUP_ROOT" ] \
    || die "running /backups mount differs from the tracked OSB_BACKUP_VOLUME"
current_label=$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.version"}}' "$CURRENT_IMAGE_ID")
case "$current_label" in
    "$CURRENT_VERSION"|development|'') ;;
    *) die "current image version label does not match installation lock" ;;
esac
compose_invoke exec -T blog osb installation verify \
    --intent /config/osb.install.toml --lock /config/osb.lock.json

transaction_id=$(date -u +%Y%m%dT%H%M%SZ)-$$
TRANSACTION_DIRECTORY="$STATE_DIRECTORY/transactions/$transaction_id"
mkdir -m 700 "$TRANSACTION_DIRECTORY"
JOURNAL="$TRANSACTION_DIRECTORY/journal.tsv"
: > "$JOURNAL"
chmod 600 "$JOURNAL"
journal "planned from=$CURRENT_VERSION to=$TARGET_VERSION commit=$TARGET_COMMIT"
journal "baseline image=$CURRENT_IMAGE_ID volume=$CURRENT_DATA_VOLUME lock-digest=$CURRENT_LOCK_DIGEST compose=$CURRENT_COMPOSE"
CONTROL_SNAPSHOT="$TRANSACTION_DIRECTORY/control"
python3 "$SUPPORT" snapshot --deployment "$DEPLOYMENT" --output "$CONTROL_SNAPSHOT"
journal "control-snapshot-complete"
CANDIDATE_CONTROL="$TRANSACTION_DIRECTORY/candidate-control"
mkdir -m 755 "$CANDIDATE_CONTROL"
for control_name in config.toml custom.css osb.install.toml osb.lock.json; do
    cp "$CONTROL_SNAPSHOT/$control_name" "$CANDIDATE_CONTROL/$control_name"
    chmod 644 "$CANDIDATE_CONTROL/$control_name"
done
CANDIDATE_ENV="$CANDIDATE_CONTROL/.env"
cp "$CONTROL_SNAPSHOT/.env" "$CANDIDATE_ENV"
chmod 600 "$CANDIDATE_ENV"

GITHUB_RELEASE_API="https://api.github.com/repos/studyreadbook4ever/OpenSoverignBlog/releases/tags/$TARGET_TAG"
curl --fail --silent --show-error \
    --proto '=https' --tlsv1.2 --max-time 30 --max-filesize 2097152 \
    --header 'Accept: application/vnd.github+json' \
    --header 'X-GitHub-Api-Version: 2022-11-28' \
    --user-agent 'OpenSoverignBlog verified updater' \
    --output "$TEMP_ROOT/github-release.json" "$GITHUB_RELEASE_API" \
    || die "canonical GitHub Release does not exist: $TARGET_RELEASE_URL"
python3 "$SUPPORT" github-release-verify \
    --file "$TEMP_ROOT/github-release.json" --tag "$TARGET_TAG" \
    --url "$TARGET_RELEASE_URL" --release-date "$TARGET_RELEASE_DATE"
journal "github-release-present url=$TARGET_RELEASE_URL"

git --git-dir="$TEMP_ROOT/repository.git" fetch --quiet --no-tags "$REMOTE_GIT_URL" \
    "+refs/tags/$TARGET_TAG:refs/tags/$TARGET_TAG"
[ "$(git --git-dir="$TEMP_ROOT/repository.git" cat-file -t "refs/tags/$TARGET_TAG")" = tag ] \
    || die "release tag must be an annotated tag object"
SIGNATURE_STATUS="$TEMP_ROOT/tag-signature.status"
if ! env -u GIT_CONFIG_COUNT -u GIT_CONFIG_PARAMETERS \
    git -c gpg.format=openpgp \
    -c "gpg.program=$GPG_PROGRAM" -c "gpg.openpgp.program=$GPG_PROGRAM" \
    --git-dir="$TEMP_ROOT/repository.git" verify-tag --raw "refs/tags/$TARGET_TAG" \
    2> "$SIGNATURE_STATUS"
then
    die "release tag signature verification failed (import the trusted publisher key first)"
fi
valid_signature_count=$(awk '/^\[GNUPG:\] VALIDSIG / { count += 1 } END { print count + 0 }' "$SIGNATURE_STATUS")
[ "$valid_signature_count" -eq 1 ] || die "release tag must contain exactly one valid signature"
verified_fingerprints=$(awk '/^\[GNUPG:\] VALIDSIG / { print $3; if ($12 != "") print $12 }' \
    "$SIGNATURE_STATUS" | tr 'a-f' 'A-F')
printf '%s\n' "$verified_fingerprints" | grep -F -x "$TRUSTED_FINGERPRINT" >/dev/null \
    || die "release tag signer does not match the explicitly trusted fingerprint"
resolved_commit=$(git --git-dir="$TEMP_ROOT/repository.git" rev-parse "refs/tags/$TARGET_TAG^{commit}")
[ "$resolved_commit" = "$TARGET_COMMIT" ] || die "signed tag commit does not match release channel"
journal "signed-tag-verified trusted-signer=$TRUSTED_FINGERPRINT"

STAGE_SOURCE="$TEMP_ROOT/source"
git --git-dir="$TEMP_ROOT/repository.git" worktree add --quiet --detach "$STAGE_SOURCE" "$TARGET_COMMIT"
[ -z "$(git -C "$STAGE_SOURCE" status --porcelain --untracked-files=no)" ] \
    || die "staged signed source worktree is not clean"
python3 "$SUPPORT" release-verify \
    --file "$STAGE_SOURCE/release.toml" \
    --version "$TARGET_VERSION" \
    --release-date "$TARGET_RELEASE_DATE" \
    --sha256 "$TARGET_MANIFEST_SHA256"
grep -F 'OSB_DATA_VOLUME' "$STAGE_SOURCE/compose.yaml" >/dev/null \
    || die "target release lacks migration-safe candidate-volume support"

TARGET_IMAGE_TAG="open-soverign-blog:update-$TARGET_VERSION-$(printf '%s' "$TARGET_COMMIT" | cut -c1-12)"
note "building the verified signed source as $TARGET_IMAGE_TAG"
docker build \
    --build-arg "OSB_VERSION=$TARGET_VERSION" \
    --build-arg "OSB_REVISION=$TARGET_COMMIT" \
    --tag "$TARGET_IMAGE_TAG" "$STAGE_SOURCE"
TARGET_IMAGE_ID=$(docker image inspect --format '{{.Id}}' "$TARGET_IMAGE_TAG")
is_image_id "$TARGET_IMAGE_ID" || die "candidate build did not produce an exact immutable SHA-256 image ID"
TARGET_ARTIFACT_SHA256=${TARGET_IMAGE_ID#sha256:}
[ "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.version"}}' "$TARGET_IMAGE_ID")" = "$TARGET_VERSION" ] \
    || die "candidate image version label drift"
[ "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$TARGET_IMAGE_ID")" = "$TARGET_COMMIT" ] \
    || die "candidate image revision label drift"
[ "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.licenses"}}' "$TARGET_IMAGE_ID")" = Unlicense ] \
    || die "candidate image license label drift"
journal "candidate-image-built image=$TARGET_IMAGE_ID"

preflight_source="github-release:$TARGET_RELEASE_URL#$TARGET_COMMIT"
docker run --rm --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges --user "$CONTROL_CONTAINER_USER" \
    --volume "$CANDIDATE_CONTROL:/control" \
    --entrypoint /usr/local/bin/osb "$TARGET_IMAGE_ID" \
    installation dlc reconcile \
    --intent /control/osb.install.toml --lock /control/osb.lock.json \
    --env-file /control/.env \
    --from "$CURRENT_VERSION" --to "$TARGET_VERSION" --source "$preflight_source" \
    --artifact-sha256 "$TARGET_ARTIFACT_SHA256"
docker run --rm --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges --user "$CONTROL_CONTAINER_USER" \
    --volume "$CANDIDATE_CONTROL:/control" \
    --entrypoint /usr/local/bin/osb "$TARGET_IMAGE_ID" \
    installation verify --intent /control/osb.install.toml --lock /control/osb.lock.json
python3 "$SUPPORT" lock-info \
    --file "$CANDIDATE_CONTROL/osb.lock.json" --output "$TEMP_ROOT/candidate-lock"
CANDIDATE_LOCK_VERSION=$(read_field "$TEMP_ROOT/candidate-lock/version")
CANDIDATE_LOCK_DIGEST=$(read_field "$TEMP_ROOT/candidate-lock/digest")
[ "$CANDIDATE_LOCK_VERSION" = "$TARGET_VERSION" ] || die "typed lock helper recorded the wrong target version"
journal "typed-candidate-lock-complete digest=$CANDIDATE_LOCK_DIGEST"

CANDIDATE_DATA_VOLUME="$COMPOSE_PROJECT-osb-data-$TARGET_VERSION-$transaction_id"
case "$CANDIDATE_DATA_VOLUME" in *[!A-Za-z0-9_.-]*) die "generated candidate volume name is unsafe" ;; esac
if docker volume inspect "$CANDIDATE_DATA_VOLUME" >/dev/null 2>&1; then
    die "candidate volume already exists: $CANDIDATE_DATA_VOLUME"
fi
docker volume create \
    --label "org.opencontainers.image.title=OpenSoverignBlog update candidate" \
    --label "org.opencontainers.image.version=$TARGET_VERSION" \
    --label "io.opensoverignblog.update.transaction=$transaction_id" \
    "$CANDIDATE_DATA_VOLUME" >/dev/null
docker run --rm --network none --read-only --cap-drop ALL --cap-add CHOWN --cap-add FOWNER \
    --security-opt no-new-privileges --user 0:0 \
    --volume "$CANDIDATE_DATA_VOLUME:/data" --entrypoint /bin/sh "$CURRENT_IMAGE_ID" \
    -ec 'chown 65532:65532 /data && chmod 700 /data'
journal "candidate-volume-prepared volume=$CANDIDATE_DATA_VOLUME"

python3 "$SUPPORT" env-rewrite \
    --source "$CANDIDATE_ENV" --output "$CANDIDATE_ENV" \
    --set "OSB_IMAGE=$TARGET_IMAGE_ID" \
    --set "OSB_DATA_VOLUME=$CANDIDATE_DATA_VOLUME" \
    --set "OSB_INSTALL_LOCK_DIGEST=$CANDIDATE_LOCK_DIGEST"
ACTIVE_COMPOSE="$STAGE_SOURCE/compose.yaml"
ACTIVE_ENV=$CANDIDATE_ENV
ACTIVE_CONTROL_ROOT=$CANDIDATE_CONTROL
compose_invoke config --format json > "$TEMP_ROOT/candidate-compose-plan.json"
python3 "$SUPPORT" compose-plan-verify \
    --file "$TEMP_ROOT/candidate-compose-plan.json" \
    --image "$TARGET_IMAGE_ID" --data-volume "$CANDIDATE_DATA_VOLUME" \
    --forbidden-data-volume "$CURRENT_DATA_VOLUME" \
    --backup-root "$BACKUP_ROOT" --control-root "$CANDIDATE_CONTROL" \
    --source-root "$STAGE_SOURCE" --active-profile "$ACTIVE_CACHE_PROFILE"
journal "candidate-compose-plan-isolated"
ACTIVE_COMPOSE=$CURRENT_COMPOSE
ACTIVE_ENV=$CURRENT_ENV
ACTIVE_CONTROL_ROOT=$DEPLOYMENT
assert_running_pair "$CURRENT_IMAGE_ID" "$CURRENT_DATA_VOLUME" \
    || die "current blog image/data pair changed during staging"
QUIESCED_CONTAINER=$PAIR_CONTAINER
python3 "$SUPPORT" snapshot-matches \
    --deployment "$DEPLOYMENT" --snapshot "$CONTROL_SNAPSHOT"

ROLLBACK_ARMED=true
journal "switch-start"
ACTIVE_COMPOSE=$CURRENT_COMPOSE
ACTIVE_ENV=$CURRENT_ENV
ACTIVE_CONTROL_ROOT=$DEPLOYMENT
compose_invoke stop blog
assert_container_pair "$QUIESCED_CONTAINER" "$CURRENT_IMAGE_ID" "$CURRENT_DATA_VOLUME" \
    || die "the stopped blog is not the verified last-known-good pair"
[ "$(docker inspect --format '{{.State.Running}}' "$QUIESCED_CONTAINER")" = false ] \
    || die "the verified current blog container did not stop"
assert_volume_quiesced "$CURRENT_DATA_VOLUME" \
    || die "the authoritative data volume is not quiescent"

# The authoritative volume must be quiescent before its rollback bundle is
# created.  An online backup followed by a later stop has a write-loss window:
# posts or comments committed after the backup would be absent from the
# candidate.  Use the already-verified old immutable image to create and check
# the bundle only after the old server has stopped.
backup_container_path="/backups/update-rollbacks/$transaction_id/data"
note "creating and verifying a quiesced SQLite/blob rollback bundle"
docker run --rm --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges \
    --tmpfs /tmp:rw,noexec,nosuid,size=64m,mode=1777 \
    --volume "$CURRENT_DATA_VOLUME:/data" \
    --volume "$BACKUP_ROOT:/backups" \
    --entrypoint /usr/local/bin/osb "$CURRENT_IMAGE_ID" \
    --database /data/open-soverign-blog.db --blob-directory /data/blobs \
    backup-bundle "$backup_container_path"
docker run --rm --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges \
    --tmpfs /tmp:rw,noexec,nosuid,size=64m,mode=1777 \
    --volume "$CURRENT_DATA_VOLUME:/data:ro" \
    --volume "$BACKUP_ROOT:/backups:ro" \
    --entrypoint /usr/local/bin/osb "$CURRENT_IMAGE_ID" \
    --database /data/open-soverign-blog.db --blob-directory /data/blobs \
    verify-bundle "$backup_container_path"
assert_volume_quiesced "$CURRENT_DATA_VOLUME" \
    || die "a writer attached to the authoritative volume during backup"
journal "verified-quiesced-data-backup path=$BACKUP_ROOT/update-rollbacks/$transaction_id/data"

docker run --rm --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges \
    --tmpfs /tmp:rw,noexec,nosuid,size=64m,mode=1777 \
    --volume "$CANDIDATE_DATA_VOLUME:/data" \
    --volume "$BACKUP_ROOT:/backups:ro" \
    --entrypoint /usr/local/bin/osb "$CURRENT_IMAGE_ID" \
    --database /data/open-soverign-blog.db --blob-directory /data/blobs \
    restore "$backup_container_path"
journal "candidate-volume-restored volume=$CANDIDATE_DATA_VOLUME"

ACTIVE_COMPOSE="$STAGE_SOURCE/compose.yaml"
ACTIVE_ENV=$CANDIDATE_ENV
ACTIVE_CONTROL_ROOT=$CANDIDATE_CONTROL
compose_invoke up -d --wait --wait-timeout 180
assert_running_pair "$TARGET_IMAGE_ID" "$CANDIDATE_DATA_VOLUME" \
    || die "candidate started with an unexpected image or /data volume"
capture_health "$TARGET_VERSION" "$TRANSACTION_DIRECTORY/candidate-health-before-lock"
journal "candidate-health-before-lock=ok"

python3 "$SUPPORT" snapshot-matches \
    --deployment "$DEPLOYMENT" --snapshot "$CONTROL_SNAPSHOT"
docker run --rm --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges --user "$CONTROL_CONTAINER_USER" \
    --volume "$CANDIDATE_CONTROL:/control" \
    --entrypoint /usr/local/bin/osb "$TARGET_IMAGE_ID" \
    installation verify --intent /control/osb.install.toml --lock /control/osb.lock.json
python3 "$SUPPORT" promote-lock \
    --source "$CANDIDATE_CONTROL/osb.lock.json" \
    --target "$DEPLOYMENT/osb.lock.json" \
    --from-version "$CURRENT_VERSION" --to-version "$TARGET_VERSION"
python3 "$SUPPORT" env-rewrite \
    --source "$CANDIDATE_ENV" --output "$DEPLOYMENT/.env" \
    --set "OSB_IMAGE=$TARGET_IMAGE_ID" \
    --set "OSB_DATA_VOLUME=$CANDIDATE_DATA_VOLUME" \
    --set "OSB_INSTALL_LOCK_DIGEST=$CANDIDATE_LOCK_DIGEST"
journal "typed-lock-and-env-promoted digest=$CANDIDATE_LOCK_DIGEST"

ACTIVE_COMPOSE="$STAGE_SOURCE/compose.yaml"
ACTIVE_ENV="$DEPLOYMENT/.env"
ACTIVE_CONTROL_ROOT=$DEPLOYMENT
compose_invoke config --format json > "$TEMP_ROOT/final-compose-plan.json"
python3 "$SUPPORT" compose-plan-verify \
    --file "$TEMP_ROOT/final-compose-plan.json" \
    --image "$TARGET_IMAGE_ID" --data-volume "$CANDIDATE_DATA_VOLUME" \
    --forbidden-data-volume "$CURRENT_DATA_VOLUME" \
    --backup-root "$BACKUP_ROOT" --control-root "$DEPLOYMENT" \
    --source-root "$STAGE_SOURCE" --active-profile "$ACTIVE_CACHE_PROFILE"
compose_invoke up -d --wait --wait-timeout 180
assert_running_pair "$TARGET_IMAGE_ID" "$CANDIDATE_DATA_VOLUME" \
    || die "promoted deployment started with an unexpected image or /data volume"
capture_health "$TARGET_VERSION" "$TRANSACTION_DIRECTORY/final-health"
journal "final-health=ok"

RUNTIME_DIRECTORY="$STATE_DIRECTORY/runtime/v$TARGET_VERSION"
[ ! -e "$RUNTIME_DIRECTORY" ] || die "managed runtime already exists: $RUNTIME_DIRECTORY"
RUNTIME_TEMP="$STATE_DIRECTORY/runtime/.v$TARGET_VERSION-$transaction_id"
mkdir -m 700 "$RUNTIME_TEMP" "$RUNTIME_TEMP/deploy" "$RUNTIME_TEMP/deploy/redis"
cp "$STAGE_SOURCE/compose.yaml" "$RUNTIME_TEMP/compose.yaml"
cp "$STAGE_SOURCE/deploy/redis/sentinel.conf" "$RUNTIME_TEMP/deploy/redis/sentinel.conf"
cp "$STAGE_SOURCE/release.toml" "$RUNTIME_TEMP/release.toml"
cp "$STAGE_SOURCE/release-channel.json" "$RUNTIME_TEMP/release-channel.json"
cp "$STAGE_SOURCE/UNLICENSE" "$RUNTIME_TEMP/UNLICENSE"
chmod 644 "$RUNTIME_TEMP/compose.yaml" "$RUNTIME_TEMP/deploy/redis/sentinel.conf" \
    "$RUNTIME_TEMP/release.toml" "$RUNTIME_TEMP/release-channel.json" "$RUNTIME_TEMP/UNLICENSE"
mv "$RUNTIME_TEMP" "$RUNTIME_DIRECTORY"
NEW_RUNTIME_DIRECTORY=$RUNTIME_DIRECTORY
pointer_temp="$STATE_DIRECTORY/.current.$transaction_id"
ln -s "runtime/v$TARGET_VERSION" "$pointer_temp"
mv -f "$pointer_temp" "$STATE_DIRECTORY/current"
journal "commit image=$TARGET_IMAGE_ID volume=$CANDIDATE_DATA_VOLUME runtime=runtime/v$TARGET_VERSION"
COMMITTED=true
ROLLBACK_ARMED=false
NEW_RUNTIME_DIRECTORY=

note "update committed: $CURRENT_VERSION -> $TARGET_VERSION"
note "last-known-good volume retained: $CURRENT_DATA_VOLUME"
note "verified rollback bundle retained: $BACKUP_ROOT/update-rollbacks/$transaction_id/data"
note "runtime Compose bundle: $STATE_DIRECTORY/current/compose.yaml"
exit 0
