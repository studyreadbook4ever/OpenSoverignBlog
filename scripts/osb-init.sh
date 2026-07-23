#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

selected_language=
case "${1-}" in
    ko | en)
        selected_language=$1
        shift
        ;;
esac

if [ -n "$selected_language" ]; then
    for argument in "$@"; do
        case "$argument" in
            --language | --language=*)
                echo "osb-init: use either the leading ko/en selector or --language, not both" >&2
                exit 2
                ;;
        esac
    done
    set -- --language "$selected_language" "$@"
fi

if [ -n "${OSB_BIN:-}" ]; then
    if [ ! -x "$OSB_BIN" ]; then
        echo "osb-init: OSB_BIN is not executable: $OSB_BIN" >&2
        exit 1
    fi
    exec "$OSB_BIN" bootstrap "$@"
fi

if command -v cargo >/dev/null 2>&1; then
    exec cargo run --quiet \
        --manifest-path "$repository_root/Cargo.toml" \
        -p osb-cli -- bootstrap "$@"
fi

if [ -x "$repository_root/target/release/osb" ]; then
    exec "$repository_root/target/release/osb" bootstrap "$@"
fi

if [ -x "$repository_root/target/debug/osb" ]; then
    exec "$repository_root/target/debug/osb" bootstrap "$@"
fi

echo "osb-init: build osb-cli or set OSB_BIN to an osb executable" >&2
exit 1
