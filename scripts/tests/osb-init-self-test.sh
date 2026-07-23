#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
fixture_root=$(mktemp -d /tmp/osb-init-test.XXXXXXXX)

cleanup() {
    case "$fixture_root" in
        /tmp/osb-init-test.*) rm -rf -- "$fixture_root" ;;
    esac
}
trap cleanup EXIT HUP INT TERM

fake_osb="$fixture_root/osb"
record="$fixture_root/arguments"
cat >"$fake_osb" <<'SCRIPT'
#!/bin/sh
set -eu
: "${OSB_INIT_TEST_RECORD:?}"
printf '%s\n' "$@" >"$OSB_INIT_TEST_RECORD"
SCRIPT
chmod 0755 "$fake_osb"

OSB_BIN="$fake_osb" OSB_INIT_TEST_RECORD="$record" \
    "$repository_root/scripts/osb-init.sh" en --directory /srv/osb/example

expected="$fixture_root/expected"
cat >"$expected" <<'EXPECTED'
bootstrap
--language
en
--directory
/srv/osb/example
EXPECTED
cmp "$expected" "$record"

OSB_BIN="$fake_osb" OSB_INIT_TEST_RECORD="$record" \
    "$repository_root/scripts/osb-init.sh" --language ko --non-interactive
cat >"$expected" <<'EXPECTED'
bootstrap
--language
ko
--non-interactive
EXPECTED
cmp "$expected" "$record"

if OSB_BIN="$fake_osb" OSB_INIT_TEST_RECORD="$record" \
    "$repository_root/scripts/osb-init.sh" en --language ko >/dev/null 2>&1
then
    echo "osb-init accepted conflicting language selectors" >&2
    exit 1
fi

echo "osb-init self-test passed"
