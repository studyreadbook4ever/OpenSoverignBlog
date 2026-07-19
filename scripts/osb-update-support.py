#!/usr/bin/env python3
"""Local, dependency-free validation helpers for the Linux updater.

This module never performs network or Docker operations.  The shell wrapper
owns those effects; this code only validates bounded data and performs atomic
local control-file operations without evaluating `.env` as shell input.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
from typing import Any, NoReturn


REPOSITORY = "https://github.com/studyreadbook4ever/OpenSoverignBlog"
CHANNEL_SCHEMA = "open-soverign-blog-release-channel/1"
RELEASE_SCHEMA = "open-soverign-blog-release/1"
SNAPSHOT_SCHEMA = "open-soverign-blog-update-control-snapshot/1"
SEMVER_PATTERN = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
SEMVER_ANY_PATTERN = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-((?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
SHA256_PATTERN = re.compile(r"^[a-f0-9]{64}$")
SHA1_PATTERN = re.compile(r"^[a-f0-9]{40}$")
PLUGIN_ID_PATTERN = re.compile(
    r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?"
    r"(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?){2,}$"
)
QUALIFIED_NAME_PATTERN = re.compile(r"^[a-z][a-z0-9_.-]{0,127}$")
ENV_KEY_PATTERN = re.compile(r"^[A-Z][A-Z0-9_]*$")
ENV_VALUE_PATTERN = re.compile(r"^[A-Za-z0-9_./:@+-]*$")
CONTROL_REQUIRED = (
    ".env",
    "config.toml",
    "custom.css",
    "osb.install.toml",
    "osb.lock.json",
)
CONTROL_OPTIONAL = ("osb.intent.json", "admin-access-key.txt")
MAX_CHANNEL_BYTES = 64 * 1024
MAX_CONTROL_BYTES = 16 * 1024 * 1024


class ContractError(ValueError):
    pass


def abort(message: str) -> NoReturn:
    raise ContractError(message)


def exact_keys(value: Any, expected: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        abort(f"{label} must be an object")
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        unknown = sorted(actual - expected)
        abort(f"{label} key mismatch (missing={missing}, unknown={unknown})")
    return value


def semver(value: Any, label: str = "version") -> tuple[int, int, int]:
    if not isinstance(value, str):
        abort(f"{label} must be a stable SemVer string")
    match = SEMVER_PATTERN.fullmatch(value)
    if match is None:
        abort(f"{label} must be stable SemVer without prerelease/build metadata")
    return tuple(int(part) for part in match.groups())  # type: ignore[return-value]


def release_date(value: Any, label: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"\d{4}-\d{2}-\d{2}", value):
        abort(f"{label} must be YYYY-MM-DD")
    try:
        if dt.date.fromisoformat(value).isoformat() != value:
            abort(f"{label} is not canonical")
    except ValueError as error:
        raise ContractError(f"{label} is invalid") from error
    return value


def read_bounded(path: Path, limit: int, label: str) -> bytes:
    try:
        info = path.lstat()
    except OSError as error:
        raise ContractError(f"cannot inspect {label}: {error}") from error
    if not stat.S_ISREG(info.st_mode) or path.is_symlink():
        abort(f"{label} must be a non-symlink regular file")
    if info.st_size > limit:
        abort(f"{label} exceeds {limit} bytes")
    try:
        return path.read_bytes()
    except OSError as error:
        raise ContractError(f"cannot read {label}: {error}") from error


def parse_json_file(path: Path, limit: int, label: str) -> Any:
    raw = read_bounded(path, limit, label)
    try:
        return json.loads(raw, parse_constant=lambda value: abort(f"{label} contains non-JSON number {value}"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ContractError(f"{label} is not valid UTF-8 JSON: {error}") from error


def validate_release_entry(value: Any, index: int) -> dict[str, Any]:
    label = f"releases[{index}]"
    entry = exact_keys(
        value,
        {"version", "tag", "releaseDate", "sourceCommit", "releaseUrl", "manifestSha256"},
        label,
    )
    semver(entry["version"], f"{label}.version")
    expected_tag = f"v{entry['version']}"
    if entry["tag"] != expected_tag:
        abort(f"{label}.tag must be {expected_tag}")
    release_date(entry["releaseDate"], f"{label}.releaseDate")
    if not isinstance(entry["sourceCommit"], str) or not SHA1_PATTERN.fullmatch(entry["sourceCommit"]):
        abort(f"{label}.sourceCommit must be a lowercase 40-hex commit")
    expected_url = f"{REPOSITORY}/releases/tag/{expected_tag}"
    if entry["releaseUrl"] != expected_url:
        abort(f"{label}.releaseUrl is not canonical")
    if not isinstance(entry["manifestSha256"], str) or not SHA256_PATTERN.fullmatch(
        entry["manifestSha256"]
    ):
        abort(f"{label}.manifestSha256 must be lowercase SHA-256")
    return entry


def validate_channel(path: Path) -> dict[str, Any]:
    channel = exact_keys(
        parse_json_file(path, MAX_CHANNEL_BYTES, "release channel"),
        {"schemaVersion", "channel", "repositoryUrl", "latest", "releases"},
        "release channel",
    )
    if channel["schemaVersion"] != CHANNEL_SCHEMA:
        abort("unsupported release channel schema")
    if channel["channel"] != "stable":
        abort("only the stable release channel is accepted")
    if channel["repositoryUrl"] != REPOSITORY:
        abort("release channel repository is not canonical")
    if not isinstance(channel["releases"], list) or len(channel["releases"]) > 64:
        abort("release channel must contain at most 64 releases")
    releases = [validate_release_entry(value, index) for index, value in enumerate(channel["releases"])]
    versions: set[str] = set()
    commits: set[str] = set()
    previous: tuple[int, int, int] | None = None
    for index, entry in enumerate(releases):
        parsed = semver(entry["version"])
        if entry["version"] in versions or entry["sourceCommit"] in commits:
            abort("release channel contains a duplicate version or source commit")
        if previous is not None and previous <= parsed:
            abort("release channel must be strictly newest-first")
        versions.add(entry["version"])
        commits.add(entry["sourceCommit"])
        previous = parsed
        if index == 0 and channel["latest"] != entry:
            abort("release channel latest must exactly equal releases[0]")
    if not releases and channel["latest"] is not None:
        abort("an empty release channel must have latest=null")
    return channel


def write_field(directory: Path, name: str, value: str) -> None:
    if not re.fullmatch(r"[a-z][a-z0-9-]*", name):
        abort("internal output field name is unsafe")
    if "\n" in value or "\r" in value or "\x00" in value:
        abort(f"unsafe newline in output field {name}")
    target = directory / name
    descriptor = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(descriptor, f"{value}\n".encode())
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def command_channel_plan(args: argparse.Namespace) -> None:
    current = semver(args.current, "current version")
    channel = validate_channel(args.channel)
    releases: list[dict[str, Any]] = channel["releases"]
    selected: dict[str, Any] | None = None
    status: str

    if args.target is not None:
        requested = semver(args.target, "requested target")
        selected = next((item for item in releases if item["version"] == args.target), None)
        if selected is None:
            abort("requested target is not in the validated stable channel")
        if requested <= current:
            abort("downgrades and reinstalling the current version are refused")
        if requested[0] != current[0] and not args.allow_major:
            abort("a major update requires --allow-major")
        status = "update_available"
    elif not releases:
        status = "no_release"
    else:
        newer = [item for item in releases if semver(item["version"]) > current]
        eligible = [item for item in newer if args.allow_major or semver(item["version"])[0] == current[0]]
        if eligible:
            selected = eligible[0]
            status = "update_available"
        elif newer:
            status = "major_available"
        else:
            status = "up_to_date"

    args.output.mkdir(mode=0o700, parents=False, exist_ok=False)
    write_field(args.output, "status", status)
    write_field(args.output, "current-version", args.current)
    if releases:
        write_field(args.output, "latest-version", releases[0]["version"])
    if selected is not None:
        for source, destination in (
            ("version", "target-version"),
            ("tag", "target-tag"),
            ("releaseDate", "target-release-date"),
            ("sourceCommit", "target-source-commit"),
            ("releaseUrl", "target-release-url"),
            ("manifestSha256", "target-manifest-sha256"),
        ):
            write_field(args.output, destination, selected[source])


def parse_release_toml(path: Path) -> dict[str, str]:
    raw = read_bounded(path, 16 * 1024, "release.toml")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ContractError("release.toml is not UTF-8") from error
    values: dict[str, str] = {}
    for number, line in enumerate(text.splitlines(), 1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        match = re.fullmatch(r'([a-z][a-z0-9_]*) = "([^"\r\n]*)"', stripped)
        if match is None:
            abort(f"release.toml line {number} is outside the updater contract")
        key, value = match.groups()
        if key in values:
            abort(f"release.toml repeats {key}")
        values[key] = value
    base_keys = {
        "schema_version",
        "version",
        "status",
        "channel",
        "repository_url",
        "developer_url",
        "license",
        "license_path",
    }
    if values.get("status") == "released":
        base_keys.add("release_date")
    exact_keys(values, base_keys, "release.toml")
    if values["schema_version"] != RELEASE_SCHEMA:
        abort("unsupported release.toml schema")
    if values["status"] not in {"unreleased", "released"}:
        abort("release.toml status is invalid")
    semver(values["version"], "release.toml version")
    if values["channel"] != "stable" or values["repository_url"] != REPOSITORY:
        abort("release.toml channel/repository drift")
    if values["license"] != "Unlicense" or values["license_path"] != "UNLICENSE":
        abort("release.toml license drift")
    if values["status"] == "released":
        release_date(values["release_date"], "release.toml release date")
    return values


def command_release_verify(args: argparse.Namespace) -> None:
    raw = read_bounded(args.file, 16 * 1024, "target release.toml")
    actual_digest = hashlib.sha256(raw).hexdigest()
    if actual_digest != args.sha256:
        abort("target release.toml does not match manifestSha256")
    release = parse_release_toml(args.file)
    if release["status"] != "released":
        abort("target release.toml is not a released v1 manifest")
    if release["version"] != args.version or release["release_date"] != args.release_date:
        abort("target release.toml version/date drift from release channel")
    license_path = args.file.parent / "UNLICENSE"
    read_bounded(license_path, 64 * 1024, "target UNLICENSE")
    sys.stdout.write(f"target release manifest verified: {release['version']}\n")


def command_github_release_verify(args: argparse.Namespace) -> None:
    value = parse_json_file(args.file, 2 * 1024 * 1024, "GitHub Release response")
    if not isinstance(value, dict):
        abort("GitHub Release response must be an object")
    if value.get("tag_name") != args.tag:
        abort("GitHub Release tag does not match the channel")
    if value.get("html_url") != args.url:
        abort("GitHub Release URL does not match the canonical channel URL")
    if value.get("draft") is not False or value.get("prerelease") is not False:
        abort("stable channel requires a published non-prerelease GitHub Release")
    published = value.get("published_at")
    if not isinstance(published, str) or not published.startswith(f"{args.release_date}T"):
        abort("GitHub Release publication date differs from the channel")
    sys.stdout.write(f"canonical GitHub Release verified: {args.tag}\n")


def command_source_info(args: argparse.Namespace) -> None:
    release = parse_release_toml(args.file)
    args.output.mkdir(mode=0o700, parents=False, exist_ok=False)
    write_field(args.output, "version", release["version"])
    write_field(args.output, "status", release["status"])


def load_lock(path: Path) -> dict[str, Any]:
    required_keys = {
        "schemaVersion",
        "installationId",
        "engine",
        "selection",
        "dlcs",
        "history",
        "lockDigest",
    }
    parsed = parse_json_file(path, 4 * 1024 * 1024, "installation lock")
    optional_keys = {"retainedDlcs"} if isinstance(parsed, dict) and "retainedDlcs" in parsed else set()
    value = exact_keys(parsed, required_keys | optional_keys, "installation lock")
    if value["schemaVersion"] != "open-soverign-blog-lock/1":
        abort("unsupported installation lock schema")
    if not isinstance(value["engine"], dict):
        abort("installation lock engine must be an object")
    engine = exact_keys(
        value["engine"],
        {"version", "configSchemaVersion", "databaseSchemaVersion", "pluginApi", "source", "artifactSha256"}
        if "artifactSha256" in value["engine"]
        else {"version", "configSchemaVersion", "databaseSchemaVersion", "pluginApi", "source"},
        "installation lock engine",
    )
    semver(engine["version"], "installation lock engine.version")
    selection = exact_keys(value["selection"], {"admin_auth", "style", "cache"}, "installation lock selection")
    if selection["cache"] not in {"none", "redis_standalone", "redis_managed"}:
        abort("installation lock selection.cache is invalid")
    validate_retained_dlcs(value)
    if not isinstance(value["lockDigest"], str) or not SHA256_PATTERN.fullmatch(value["lockDigest"]):
        abort("installation lock digest is invalid")
    if expected_lock_digest(value) != value["lockDigest"]:
        abort("installation lock canonical digest does not match")
    return value


def validate_retained_dlcs(lock: dict[str, Any]) -> None:
    active = lock.get("dlcs")
    if not isinstance(active, list) or len(active) > 256:
        abort("installation lock dlcs must be an array of at most 256 records")
    active_ids: set[str] = set()
    for index, record in enumerate(active):
        plugin_id = record.get("id") if isinstance(record, dict) else None
        if (
            not isinstance(plugin_id, str)
            or len(plugin_id) > 190
            or PLUGIN_ID_PATTERN.fullmatch(plugin_id) is None
        ):
            abort(f"installation lock dlcs[{index}].id is invalid")
        if plugin_id in active_ids:
            abort("installation lock active DLC ids must be unique")
        active_ids.add(plugin_id)
    retained = lock.get("retainedDlcs", [])
    if not isinstance(retained, list) or len(retained) > 256:
        abort("installation lock retainedDlcs must be an array of at most 256 records")
    previous_id: str | None = None
    for index, raw_record in enumerate(retained):
        label = f"installation lock retainedDlcs[{index}]"
        if not isinstance(raw_record, dict):
            abort(f"{label} must be an object")
        required = {"id", "removedVersion"}
        optional = {"stateVersion", "appliedMigrations"}
        record = exact_keys(raw_record, required | (set(raw_record) & optional), label)
        plugin_id = record["id"]
        if (
            not isinstance(plugin_id, str)
            or len(plugin_id) > 190
            or PLUGIN_ID_PATTERN.fullmatch(plugin_id) is None
        ):
            abort(f"{label}.id is invalid")
        if previous_id is not None and previous_id >= plugin_id:
            abort("installation lock retainedDlcs must be uniquely sorted by id")
        if plugin_id in active_ids:
            abort("installation lock active and retained DLC records cannot share an id")
        previous_id = plugin_id
        removed_version = record["removedVersion"]
        if not isinstance(removed_version, str) or SEMVER_ANY_PATTERN.fullmatch(removed_version) is None:
            abort(f"{label}.removedVersion is not SemVer")
        if "stateVersion" in record:
            state_version = record["stateVersion"]
            if (
                not isinstance(state_version, int)
                or isinstance(state_version, bool)
                or not (1 <= state_version <= (1 << 64) - 1)
            ):
                abort(f"{label}.stateVersion must be a positive u64")
        migrations = record.get("appliedMigrations", [])
        if not isinstance(migrations, list):
            abort(f"{label}.appliedMigrations must be an array")
        if any(
            not isinstance(migration, str)
            or QUALIFIED_NAME_PATTERN.fullmatch(migration) is None
            for migration in migrations
        ):
            abort(f"{label}.appliedMigrations is invalid")
        if len(migrations) != len(set(migrations)):
            abort(f"{label}.appliedMigrations must be unique")


def expected_lock_digest(lock: dict[str, Any]) -> str:
    payload = dict(lock)
    payload["lockDigest"] = ""
    canonical = json.dumps(
        payload, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode()
    return hashlib.sha256(canonical).hexdigest()


def command_lock_info(args: argparse.Namespace) -> None:
    lock = load_lock(args.file)
    args.output.mkdir(mode=0o700, parents=False, exist_ok=False)
    write_field(args.output, "version", lock["engine"]["version"])
    write_field(args.output, "digest", lock["lockDigest"])
    write_field(args.output, "cache", lock["selection"]["cache"])


def command_promote_lock(args: argparse.Namespace) -> None:
    before = load_lock(args.target)
    after = load_lock(args.source)
    if before["installationId"] != after["installationId"]:
        abort("candidate and live locks belong to different installations")
    if before["engine"]["version"] != args.from_version:
        abort("live lock changed after the update plan")
    if after["engine"]["version"] != args.to_version:
        abort("candidate lock does not contain the target engine version")
    # A target reconciliation may legitimately change exact DLC resolutions and
    # append DLC history. Structural operator selections and identity may not
    # change as a side effect of an engine update.
    for key in ("schemaVersion", "installationId", "selection"):
        if before[key] != after[key]:
            abort(f"engine update unexpectedly changed lock field {key}")
    source_descriptor, source_info = open_regular(args.source, "candidate installation lock")
    try:
        payload = read_descriptor(source_descriptor, source_info.st_size, "candidate installation lock")
    finally:
        os.close(source_descriptor)
    target_descriptor, target_info = open_regular(args.target, "live installation lock")
    os.close(target_descriptor)
    atomic_replace(args.target, payload, stat.S_IMODE(target_info.st_mode))
    sys.stdout.write("candidate installation lock promoted atomically\n")


def command_compose_plan_verify(args: argparse.Namespace) -> None:
    """Fail before downtime if target Compose could touch the live data pair."""
    config = parse_json_file(args.file, 4 * 1024 * 1024, "rendered candidate Compose plan")
    if not isinstance(config, dict):
        abort("rendered candidate Compose plan must be an object")
    services = config.get("services")
    volumes = config.get("volumes")
    if not isinstance(services, dict) or not isinstance(volumes, dict):
        abort("rendered candidate Compose plan lacks services or volumes")
    active_profile = None if args.active_profile == "none" else args.active_profile

    def is_active(name: str, value: Any) -> bool:
        if not isinstance(value, dict):
            abort(f"candidate Compose service {name} must be an object")
        profiles = value.get("profiles", [])
        if not isinstance(profiles, list) or not all(isinstance(item, str) for item in profiles):
            abort(f"candidate Compose service {name} has an invalid profile list")
        return not profiles or (active_profile is not None and active_profile in profiles)

    data_definition = volumes.get("osb-data")
    if not isinstance(data_definition, dict) or data_definition.get("name") != args.data_volume:
        abort("candidate Compose osb-data does not resolve to the staged volume")
    for volume_name, definition in volumes.items():
        if not isinstance(definition, dict):
            abort(f"candidate Compose volume {volume_name} must be an object")
        if definition.get("driver") not in (None, "local"):
            abort(f"candidate Compose volume {volume_name} uses a non-local driver")
        if definition.get("driver_opts") not in (None, {}):
            abort(f"candidate Compose volume {volume_name} uses forbidden driver options")
        if definition.get("external") is True:
            abort(f"candidate Compose volume {volume_name} must not be externally aliased")
        if definition.get("name") == args.forbidden_data_volume:
            abort("candidate Compose still declares the last-known-good data volume")

    def service(name: str) -> dict[str, Any]:
        value = services.get(name)
        if not isinstance(value, dict):
            abort(f"candidate Compose lacks required service {name}")
        if not is_active(name, value):
            abort(f"candidate Compose required service {name} is not active")
        if value.get("image") != args.image:
            abort(f"candidate Compose service {name} does not use the immutable target image")
        mounts = value.get("volumes")
        if not isinstance(mounts, list):
            abort(f"candidate Compose service {name} lacks a volume list")
        data_mounts = [mount for mount in mounts if isinstance(mount, dict) and mount.get("target") == "/data"]
        if len(data_mounts) != 1:
            abort(f"candidate Compose service {name} must have exactly one /data mount")
        data_mount = data_mounts[0]
        if data_mount.get("type") != "volume" or data_mount.get("source") != "osb-data":
            abort(f"candidate Compose service {name} /data does not use declared osb-data")
        if data_mount.get("read_only") is True:
            abort(f"candidate Compose service {name} unexpectedly makes candidate /data read-only")
        return value

    blog = service("blog")
    storage_init = service("osb-storage-init")
    if blog.get("read_only") is not True:
        abort("candidate blog root filesystem must remain read-only")
    if blog.get("privileged") is True:
        abort("candidate blog service must not be privileged")

    expected_binds = {
        "/backups": (str(args.backup_root), False),
        "/config/config.toml": (str(args.control_root / "config.toml"), True),
        "/config/custom.css": (str(args.control_root / "custom.css"), True),
        "/config/osb.install.toml": (str(args.control_root / "osb.install.toml"), True),
        "/config/osb.lock.json": (str(args.control_root / "osb.lock.json"), True),
    }
    mounts = blog["volumes"]
    expected_blog_targets = {"/data", *expected_binds}
    actual_blog_targets = {
        mount.get("target") for mount in mounts if isinstance(mount, dict)
    }
    if actual_blog_targets != expected_blog_targets or len(mounts) != len(expected_blog_targets):
        abort("candidate blog contains an unexpected or duplicate mount")
    for target, (expected_source, expected_read_only) in expected_binds.items():
        matches = [mount for mount in mounts if isinstance(mount, dict) and mount.get("target") == target]
        if len(matches) != 1:
            abort(f"candidate blog must have exactly one {target} mount")
        mount = matches[0]
        if mount.get("type") != "bind" or mount.get("source") != expected_source:
            abort(f"candidate blog {target} bind does not use its staged protected source")
        if bool(mount.get("read_only", False)) != expected_read_only:
            abort(f"candidate blog {target} bind has the wrong read-only policy")

    storage_mounts = storage_init["volumes"]
    storage_targets = {
        mount.get("target") for mount in storage_mounts if isinstance(mount, dict)
    }
    if storage_targets != {"/data", "/backups"} or len(storage_mounts) != 2:
        abort("candidate storage initializer contains an unexpected or duplicate mount")
    storage_backups = [
        mount
        for mount in storage_mounts
        if isinstance(mount, dict) and mount.get("target") == "/backups"
    ]
    if (
        len(storage_backups) != 1
        or storage_backups[0].get("type") != "bind"
        or storage_backups[0].get("source") != str(args.backup_root)
        or storage_backups[0].get("read_only") is True
    ):
        abort("candidate storage initializer does not use the writable canonical backup root")

    source_root = args.source_root.resolve(strict=True)
    for name, value in services.items():
        if not is_active(name, value):
            continue
        if value.get("privileged") is True:
            abort(f"active candidate service {name} must not be privileged")
        if value.get("volumes_from") not in (None, []):
            abort(f"active candidate service {name} must not use volumes_from")
        if value.get("devices") not in (None, []):
            abort(f"active candidate service {name} must not expose host devices")
        if value.get("device_cgroup_rules") not in (None, []):
            abort(f"active candidate service {name} must not add host device rules")
        service_mounts = value.get("volumes", [])
        if not isinstance(service_mounts, list):
            abort(f"active candidate service {name} has an invalid volume list")
        for mount in service_mounts:
            if not isinstance(mount, dict):
                abort(f"active candidate service {name} has an invalid mount")
            mount_type = mount.get("type")
            mount_source = mount.get("source")
            if mount_type == "volume":
                definition = volumes.get(mount_source)
                if not isinstance(mount_source, str) or not isinstance(definition, dict):
                    abort(f"active candidate service {name} uses an undeclared named volume")
                if definition.get("name") == args.forbidden_data_volume:
                    abort(f"active candidate service {name} mounts the last-known-good data volume")
                continue
            if mount_type != "bind" or not isinstance(mount_source, str):
                abort(f"active candidate service {name} uses an unsupported mount type")
            if name in {"blog", "osb-storage-init"}:
                # Their complete bind allowlists were checked above.
                continue
            try:
                resolved_source = Path(mount_source).resolve(strict=True)
            except OSError as error:
                raise ContractError(
                    f"active candidate service {name} bind source cannot be resolved: {error}"
                ) from error
            if source_root not in resolved_source.parents or mount.get("read_only") is not True:
                abort(
                    f"active candidate service {name} bind must be a read-only file inside signed source"
                )
    sys.stdout.write("candidate Compose plan is isolated to the staged image, controls, and data volume\n")


def resolve_deployment(path: Path) -> Path:
    try:
        absolute = path.absolute()
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise ContractError(f"cannot resolve deployment directory: {error}") from error
    if absolute != resolved:
        abort("deployment directory itself must not be a symlink")
    if not resolved.is_dir() or resolved in {Path("/"), Path.home().resolve()}:
        abort("deployment directory must not be / or the home directory")
    return resolved


def open_regular(path: Path, label: str) -> tuple[int, os.stat_result]:
    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ContractError(f"cannot open {label}: {error}") from error
    info = os.fstat(descriptor)
    if not stat.S_ISREG(info.st_mode):
        os.close(descriptor)
        abort(f"{label} must be a regular file")
    if info.st_size > MAX_CONTROL_BYTES:
        os.close(descriptor)
        abort(f"{label} exceeds the control-file size limit")
    return descriptor, info


def read_descriptor(descriptor: int, expected_size: int, label: str) -> bytes:
    chunks: list[bytes] = []
    remaining = expected_size
    while remaining:
        chunk = os.read(descriptor, min(remaining, 1024 * 1024))
        if not chunk:
            abort(f"{label} changed while being read")
        chunks.append(chunk)
        remaining -= len(chunk)
    if os.read(descriptor, 1):
        abort(f"{label} grew while being read")
    return b"".join(chunks)


def write_new_private(path: Path, payload: bytes) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        view = memoryview(payload)
        while view:
            written = os.write(descriptor, view)
            view = view[written:]
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def fsync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def command_snapshot(args: argparse.Namespace) -> None:
    deployment = resolve_deployment(args.deployment)
    output_parent = args.output.parent.resolve(strict=True)
    if deployment not in output_parent.parents and output_parent != deployment:
        abort("control snapshot must remain inside the deployment directory")
    args.output.mkdir(mode=0o700, exist_ok=False)
    records: dict[str, Any] = {}
    try:
        for name in (*CONTROL_REQUIRED, *CONTROL_OPTIONAL):
            source = deployment / name
            if not source.exists() and not source.is_symlink():
                if name in CONTROL_REQUIRED:
                    abort(f"required control file {name} is missing")
                records[name] = {"present": False}
                continue
            descriptor, info = open_regular(source, f"control file {name}")
            try:
                payload = read_descriptor(descriptor, info.st_size, f"control file {name}")
            finally:
                os.close(descriptor)
            write_new_private(args.output / name, payload)
            records[name] = {
                "present": True,
                "mode": stat.S_IMODE(info.st_mode),
                "sha256": hashlib.sha256(payload).hexdigest(),
                "size": len(payload),
            }
        manifest = {"schemaVersion": SNAPSHOT_SCHEMA, "files": records}
        write_new_private(args.output / "manifest.json", (json.dumps(manifest, sort_keys=True, indent=2) + "\n").encode())
        fsync_directory(args.output)
    except BaseException:
        # Leave an incomplete private directory as evidence; it is never used
        # for restore without a complete, valid manifest.
        raise
    sys.stdout.write(f"protected control snapshot created: {args.output}\n")


def validate_snapshot(snapshot: Path) -> dict[str, dict[str, Any]]:
    manifest = exact_keys(
        parse_json_file(snapshot / "manifest.json", 128 * 1024, "snapshot manifest"),
        {"schemaVersion", "files"},
        "snapshot manifest",
    )
    if manifest["schemaVersion"] != SNAPSHOT_SCHEMA:
        abort("unsupported control snapshot schema")
    expected_names = set((*CONTROL_REQUIRED, *CONTROL_OPTIONAL))
    records = exact_keys(manifest["files"], expected_names, "snapshot files")
    for name, record in records.items():
        if not isinstance(record, dict) or not isinstance(record.get("present"), bool):
            abort(f"snapshot record {name} is invalid")
        if not record["present"]:
            exact_keys(record, {"present"}, f"snapshot record {name}")
            continue
        exact_keys(record, {"present", "mode", "sha256", "size"}, f"snapshot record {name}")
        if not isinstance(record["mode"], int) or record["mode"] < 0 or record["mode"] > 0o777:
            abort(f"snapshot mode for {name} is invalid")
        if not isinstance(record["size"], int) or not (0 <= record["size"] <= MAX_CONTROL_BYTES):
            abort(f"snapshot size for {name} is invalid")
        if not isinstance(record["sha256"], str) or not SHA256_PATTERN.fullmatch(record["sha256"]):
            abort(f"snapshot digest for {name} is invalid")
    return records


def atomic_replace(path: Path, payload: bytes, mode: int) -> None:
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.osb-update-", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        os.fchmod(descriptor, mode & 0o777)
        view = memoryview(payload)
        while view:
            written = os.write(descriptor, view)
            view = view[written:]
        os.fsync(descriptor)
        os.close(descriptor)
        descriptor = -1
        os.replace(temporary, path)
        fsync_directory(path.parent)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def command_restore(args: argparse.Namespace) -> None:
    deployment = resolve_deployment(args.deployment)
    if args.snapshot.is_symlink():
        abort("control snapshot directory must not be a symlink")
    snapshot = args.snapshot.resolve(strict=True)
    if deployment not in snapshot.parents:
        abort("control snapshot is outside the deployment directory")
    records = validate_snapshot(snapshot)
    for name in (*CONTROL_REQUIRED, *CONTROL_OPTIONAL):
        record = records[name]
        if not record["present"]:
            target = deployment / name
            if not target.exists() and not target.is_symlink():
                continue
            descriptor, _ = open_regular(target, f"unexpected live control file {name}")
            os.close(descriptor)
            evidence = snapshot.parent / "rollback-unexpected-controls"
            if evidence.exists() or evidence.is_symlink():
                if evidence.is_symlink() or not evidence.is_dir():
                    abort("rollback unexpected-control evidence path is unsafe")
            else:
                evidence.mkdir(mode=0o700)
            destination = evidence / name
            if destination.exists() or destination.is_symlink():
                abort(f"rollback evidence already contains unexpected control {name}")
            os.replace(target, destination)
            fsync_directory(evidence)
            fsync_directory(deployment)
            continue
        payload = read_bounded(snapshot / name, MAX_CONTROL_BYTES, f"snapshot file {name}")
        if len(payload) != record["size"] or hashlib.sha256(payload).hexdigest() != record["sha256"]:
            abort(f"snapshot file {name} failed integrity verification")
        target = deployment / name
        if target.is_symlink() or (target.exists() and not target.is_file()):
            abort(f"refusing to replace unsafe control target {name}")
        atomic_replace(target, payload, record["mode"])
    sys.stdout.write("protected control files restored\n")


def command_snapshot_matches(args: argparse.Namespace) -> None:
    deployment = resolve_deployment(args.deployment)
    if args.snapshot.is_symlink():
        abort("control snapshot directory must not be a symlink")
    snapshot = args.snapshot.resolve(strict=True)
    if deployment not in snapshot.parents:
        abort("control snapshot is outside the deployment directory")
    records = validate_snapshot(snapshot)
    for name in (*CONTROL_REQUIRED, *CONTROL_OPTIONAL):
        record = records[name]
        target = deployment / name
        exists = target.exists() or target.is_symlink()
        if bool(record["present"]) != exists:
            abort(f"live control presence changed after snapshot: {name}")
        if not record["present"]:
            continue
        descriptor, info = open_regular(target, f"live control file {name}")
        try:
            payload = read_descriptor(descriptor, info.st_size, f"live control file {name}")
        finally:
            os.close(descriptor)
        if (
            len(payload) != record["size"]
            or hashlib.sha256(payload).hexdigest() != record["sha256"]
            or stat.S_IMODE(info.st_mode) != record["mode"]
        ):
            abort(f"live control changed after snapshot: {name}")
    sys.stdout.write("live controls still match the protected snapshot\n")


def command_env_rewrite(args: argparse.Namespace) -> None:
    descriptor, info = open_regular(args.source, "environment file")
    try:
        raw = read_descriptor(descriptor, info.st_size, "environment file")
    finally:
        os.close(descriptor)
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ContractError("environment file is not UTF-8") from error
    if "\x00" in text:
        abort("environment file contains NUL")
    updates: dict[str, str] = {}
    for assignment in args.set:
        if "=" not in assignment:
            abort("--set requires KEY=VALUE")
        key, value = assignment.split("=", 1)
        if not ENV_KEY_PATTERN.fullmatch(key) or not ENV_VALUE_PATTERN.fullmatch(value):
            abort(f"unsafe environment override {key}")
        if key in updates:
            abort(f"duplicate override {key}")
        updates[key] = value
    seen: set[str] = set()
    output_lines: list[str] = []
    assignment_pattern = re.compile(r"^([A-Z][A-Z0-9_]*)=")
    for line in text.splitlines(keepends=True):
        match = assignment_pattern.match(line)
        if match is None or match.group(1) not in updates:
            output_lines.append(line)
            continue
        key = match.group(1)
        if key in seen:
            abort(f"environment file repeats protected key {key}")
        ending = "\r\n" if line.endswith("\r\n") else "\n"
        output_lines.append(f"{key}={updates[key]}{ending}")
        seen.add(key)
    if output_lines and not output_lines[-1].endswith(("\n", "\r")):
        output_lines[-1] += "\n"
    for key, value in updates.items():
        if key not in seen:
            output_lines.append(f"{key}={value}\n")
    payload = "".join(output_lines).encode()
    if len(payload) > MAX_CONTROL_BYTES:
        abort("rewritten environment file exceeds size limit")
    args.output.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if args.output.exists() and args.output.is_symlink():
        abort("environment output must not be a symlink")
    atomic_replace(args.output, payload, stat.S_IMODE(info.st_mode))


def command_env_info(args: argparse.Namespace) -> None:
    descriptor, info = open_regular(args.file, "environment file")
    try:
        raw = read_descriptor(descriptor, info.st_size, "environment file")
    finally:
        os.close(descriptor)
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ContractError("environment file is not UTF-8") from error
    values: dict[str, str] = {}
    selected_keys = {
        "COMPOSE_PROJECT_NAME",
        "OSB_BACKUP_VOLUME",
        "OSB_DATA_VOLUME",
        "OSB_DELIVERY_ONLY",
        "OSB_INSTALL_LOCK_DIGEST",
    }
    for line in text.splitlines():
        match = re.fullmatch(r"([A-Z][A-Z0-9_]*)=([^\x00\r\n]*)", line)
        if match is None:
            continue
        key, value = match.groups()
        if key in selected_keys:
            if key in values:
                abort(f"environment file repeats {key}")
            values[key] = value
    project = values.get("COMPOSE_PROJECT_NAME")
    if project is None or not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,62}", project):
        abort("bootstrap environment must contain a safe COMPOSE_PROJECT_NAME")
    data_volume = values.get("OSB_DATA_VOLUME")
    if data_volume is None or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", data_volume):
        abort("bootstrap environment must contain a safe OSB_DATA_VOLUME")
    lock_digest = values.get("OSB_INSTALL_LOCK_DIGEST")
    if lock_digest is None or not SHA256_PATTERN.fullmatch(lock_digest):
        abort("bootstrap environment must contain the canonical OSB_INSTALL_LOCK_DIGEST")
    backup_raw = values.get("OSB_BACKUP_VOLUME")
    if backup_raw is None:
        abort("bootstrap environment must contain OSB_BACKUP_VOLUME")
    if backup_raw.startswith("'") and backup_raw.endswith("'") and len(backup_raw) >= 2:
        backup_value = backup_raw[1:-1]
        if "'" in backup_value:
            abort("OSB_BACKUP_VOLUME contains an unsupported quote")
    else:
        backup_value = backup_raw
        if not re.fullmatch(r"[A-Za-z0-9_./:+-]+", backup_value):
            abort("OSB_BACKUP_VOLUME must be an absolute path or one simple quoted path")
    backup_path = Path(backup_value)
    if not backup_path.is_absolute():
        abort("tracked updates require an absolute OSB_BACKUP_VOLUME")
    try:
        backup_path = backup_path.resolve(strict=True)
    except OSError as error:
        raise ContractError(f"cannot resolve OSB_BACKUP_VOLUME: {error}") from error
    if not backup_path.is_dir() or backup_path == Path("/"):
        abort("OSB_BACKUP_VOLUME must resolve to a dedicated directory")
    if ":" in str(backup_path):
        abort("OSB_BACKUP_VOLUME cannot contain ':' because Docker bind syntax would be ambiguous")
    delivery = values.get("OSB_DELIVERY_ONLY")
    if delivery not in {"true", "false"}:
        abort("OSB_DELIVERY_ONLY must be explicit true or false")
    args.output.mkdir(mode=0o700, parents=False, exist_ok=False)
    write_field(args.output, "compose-project", project)
    write_field(args.output, "backup-volume", str(backup_path))
    write_field(args.output, "data-volume", data_volume)
    write_field(args.output, "delivery-only", delivery)
    write_field(args.output, "lock-digest", lock_digest)


def read_health_json(path: Path, label: str) -> Any:
    return parse_json_file(path, 2 * 1024 * 1024, label)


def command_health(args: argparse.Namespace) -> None:
    live = read_health_json(args.live, "livez response")
    ready = read_health_json(args.ready, "readyz response")
    health = read_health_json(args.health, "healthz response")
    feed = read_health_json(args.feed, "feed response")
    if not isinstance(live, dict) or live.get("status") != "alive" or live.get("version") != args.expected:
        abort("livez did not report the exact target version")
    if not isinstance(ready, dict) or ready.get("status") != "ready" or ready.get("version") != args.expected:
        abort("readyz did not report ready at the exact target version")
    if not isinstance(health, dict) or health.get("status") != "ok" or health.get("version") != args.expected:
        abort("healthz did not report ok at the exact target version")
    if not isinstance(feed, (dict, list)):
        abort("public feed response is not JSON")


def command_self_test(_: argparse.Namespace) -> None:
    assert semver("1.2.3") == (1, 2, 3)
    for invalid in ("1.2", "01.2.3", "1.2.3-rc.1", "1.2.3+build"):
        try:
            semver(invalid)
        except ContractError:
            pass
        else:
            raise AssertionError(f"accepted invalid stable version {invalid}")
    repository_example = Path(__file__).resolve().parent.parent / "osb.lock.example.json"
    if repository_example.is_file():
        example_lock = load_lock(repository_example)
        assert example_lock["engine"]["version"] == "0.1.0"
        assert example_lock["selection"]["cache"] == "redis_managed"
    with tempfile.TemporaryDirectory(prefix="osb-update-support-test-") as raw:
        root = Path(raw)
        if repository_example.is_file():
            legacy_lock_info = root / "legacy-lock-info"
            command_lock_info(
                argparse.Namespace(file=repository_example, output=legacy_lock_info)
            )
            assert "retainedDlcs" not in example_lock
            assert (
                legacy_lock_info.joinpath("digest").read_text(encoding="utf-8").strip()
                == example_lock["lockDigest"]
            )

            retained_lock = json.loads(json.dumps(example_lock))
            removed = retained_lock["dlcs"].pop()
            retained_lock["retainedDlcs"] = [
                {
                    "id": removed["id"],
                    "removedVersion": removed["version"],
                    "stateVersion": 1,
                    "appliedMigrations": ["seo.initial_state"],
                }
            ]
            retained_lock["history"].append(
                {
                    "sequence": len(retained_lock["history"]) + 1,
                    "action": "removed",
                    "dlcId": removed["id"],
                    "fromVersion": removed["version"],
                    "engineVersion": retained_lock["engine"]["version"],
                }
            )
            retained_lock["lockDigest"] = expected_lock_digest(retained_lock)
            retained_path = root / "retained-lock.json"
            retained_path.write_text(
                json.dumps(retained_lock, indent=2) + "\n", encoding="utf-8"
            )
            retained_lock_info = root / "retained-lock-info"
            command_lock_info(
                argparse.Namespace(file=retained_path, output=retained_lock_info)
            )
            assert (
                retained_lock_info.joinpath("digest").read_text(encoding="utf-8").strip()
                == retained_lock["lockDigest"]
            )

            promoted_live = root / "promoted-live-lock.json"
            promoted_live.write_text(
                json.dumps(retained_lock, indent=2) + "\n", encoding="utf-8"
            )
            retained_candidate = json.loads(json.dumps(retained_lock))
            retained_candidate["engine"]["version"] = "0.2.0"
            retained_candidate["engine"]["source"] = "signed-test-release"
            retained_candidate["lockDigest"] = expected_lock_digest(retained_candidate)
            retained_candidate_path = root / "retained-candidate-lock.json"
            retained_candidate_path.write_text(
                json.dumps(retained_candidate, indent=2) + "\n", encoding="utf-8"
            )
            command_promote_lock(
                argparse.Namespace(
                    source=retained_candidate_path,
                    target=promoted_live,
                    from_version="0.1.0",
                    to_version="0.2.0",
                )
            )
            promoted = load_lock(promoted_live)
            assert promoted["engine"]["version"] == "0.2.0"
            assert promoted["retainedDlcs"] == retained_lock["retainedDlcs"]

            unknown_retained = json.loads(json.dumps(retained_lock))
            unknown_retained["retainedDlcs"][0]["unexpected"] = True
            unknown_retained["lockDigest"] = expected_lock_digest(unknown_retained)
            unknown_path = root / "unknown-retained-lock.json"
            unknown_path.write_text(
                json.dumps(unknown_retained, indent=2) + "\n", encoding="utf-8"
            )
            try:
                load_lock(unknown_path)
            except ContractError:
                pass
            else:
                raise AssertionError("retained DLC state accepted an unknown field")

            overlapping = json.loads(json.dumps(retained_lock))
            overlapping["dlcs"].append(removed)
            overlapping["lockDigest"] = expected_lock_digest(overlapping)
            overlapping_path = root / "overlapping-retained-lock.json"
            overlapping_path.write_text(
                json.dumps(overlapping, indent=2) + "\n", encoding="utf-8"
            )
            try:
                load_lock(overlapping_path)
            except ContractError:
                pass
            else:
                raise AssertionError("active and retained DLC ids were allowed to overlap")

            duplicate_migrations = json.loads(json.dumps(retained_lock))
            duplicate_migrations["retainedDlcs"][0]["appliedMigrations"] = [
                "seo.initial_state",
                "seo.initial_state",
            ]
            duplicate_migrations["lockDigest"] = expected_lock_digest(
                duplicate_migrations
            )
            duplicate_migrations_path = root / "duplicate-retained-migrations.json"
            duplicate_migrations_path.write_text(
                json.dumps(duplicate_migrations, indent=2) + "\n", encoding="utf-8"
            )
            try:
                load_lock(duplicate_migrations_path)
            except ContractError:
                pass
            else:
                raise AssertionError("duplicate retained DLC migrations were accepted")

        channel = root / "channel.json"
        channel.write_text(
            json.dumps(
                {
                    "schemaVersion": CHANNEL_SCHEMA,
                    "channel": "stable",
                    "repositoryUrl": REPOSITORY,
                    "latest": None,
                    "releases": [],
                }
            )
            + "\n",
            encoding="utf-8",
        )
        assert validate_channel(channel)["latest"] is None
        entries = []
        for version, commit in (("2.0.0", "2" * 40), ("1.4.0", "1" * 40)):
            entries.append(
                {
                    "version": version,
                    "tag": f"v{version}",
                    "releaseDate": "2026-07-19",
                    "sourceCommit": commit,
                    "releaseUrl": f"{REPOSITORY}/releases/tag/v{version}",
                    "manifestSha256": commit[0] * 64,
                }
            )
        channel.write_text(
            json.dumps(
                {
                    "schemaVersion": CHANNEL_SCHEMA,
                    "channel": "stable",
                    "repositoryUrl": REPOSITORY,
                    "latest": entries[0],
                    "releases": entries,
                }
            )
            + "\n",
            encoding="utf-8",
        )
        plan = root / "plan"
        command_channel_plan(
            argparse.Namespace(
                channel=channel,
                current="1.2.3",
                target=None,
                allow_major=False,
                output=plan,
            )
        )
        assert (plan / "target-version").read_text(encoding="utf-8").strip() == "1.4.0"
        major_plan = root / "major-plan"
        command_channel_plan(
            argparse.Namespace(
                channel=channel,
                current="1.4.0",
                target=None,
                allow_major=False,
                output=major_plan,
            )
        )
        assert (major_plan / "status").read_text(encoding="utf-8").strip() == "major_available"
        source = root / ".env"
        source.write_text("SECRET=do-not-print\nOSB_IMAGE=old\n", encoding="utf-8")
        output = root / "next.env"
        command_env_rewrite(
            argparse.Namespace(source=source, output=output, set=["OSB_IMAGE=sha256:abc", "OSB_DATA_VOLUME=v2"])
        )
        rendered = output.read_text(encoding="utf-8")
        assert "SECRET=do-not-print" in rendered
        assert "OSB_IMAGE=sha256:abc" in rendered
        assert "OSB_DATA_VOLUME=v2" in rendered
        deployment = root / "deployment"
        deployment.mkdir()
        for name in CONTROL_REQUIRED:
            (deployment / name).write_text(f"original-{name}\n", encoding="utf-8")
        backup_root = deployment / ".osb-backups"
        backup_root.mkdir()
        (deployment / ".env").write_text(
            "COMPOSE_PROJECT_NAME=osb-test\n"
            "OSB_DATA_VOLUME=osb-data-test\n"
            f"OSB_INSTALL_LOCK_DIGEST={'a' * 64}\n"
            f"OSB_BACKUP_VOLUME='{backup_root}'\n"
            "OSB_DELIVERY_ONLY=false\n",
            encoding="utf-8",
        )
        env_info = root / "env-info"
        command_env_info(argparse.Namespace(file=deployment / ".env", output=env_info))
        assert (env_info / "compose-project").read_text(encoding="utf-8").strip() == "osb-test"
        assert (env_info / "backup-volume").read_text(encoding="utf-8").strip() == str(backup_root)
        assert (env_info / "data-volume").read_text(encoding="utf-8").strip() == "osb-data-test"
        assert (env_info / "lock-digest").read_text(encoding="utf-8").strip() == "a" * 64
        image_id = f"sha256:{'b' * 64}"
        rendered_compose = root / "candidate-compose.json"
        rendered = {
            "services": {
                "blog": {
                    "image": image_id,
                    "read_only": True,
                    "volumes": [
                        {"type": "volume", "source": "osb-data", "target": "/data"},
                        {"type": "bind", "source": str(backup_root), "target": "/backups"},
                        *[
                            {
                                "type": "bind",
                                "source": str(deployment / source_name),
                                "target": target,
                                "read_only": True,
                            }
                            for source_name, target in (
                                ("config.toml", "/config/config.toml"),
                                ("custom.css", "/config/custom.css"),
                                ("osb.install.toml", "/config/osb.install.toml"),
                                ("osb.lock.json", "/config/osb.lock.json"),
                            )
                        ],
                    ],
                },
                "osb-storage-init": {
                    "image": image_id,
                    "volumes": [
                        {"type": "volume", "source": "osb-data", "target": "/data"},
                        {"type": "bind", "source": str(backup_root), "target": "/backups"},
                    ],
                },
            },
            "volumes": {"osb-data": {"name": "osb-data-candidate"}},
        }
        rendered_compose.write_text(json.dumps(rendered), encoding="utf-8")
        compose_arguments = argparse.Namespace(
            file=rendered_compose,
            image=image_id,
            data_volume="osb-data-candidate",
            forbidden_data_volume="osb-data-live",
            backup_root=backup_root,
            control_root=deployment,
            source_root=root,
            active_profile="redis-managed",
        )
        command_compose_plan_verify(compose_arguments)
        rendered["volumes"]["osb-data"]["name"] = "live-volume"
        rendered_compose.write_text(json.dumps(rendered), encoding="utf-8")
        try:
            command_compose_plan_verify(compose_arguments)
        except ContractError:
            pass
        else:
            raise AssertionError("candidate Compose verifier accepted the live data volume")
        rendered["volumes"]["osb-data"]["name"] = "osb-data-candidate"
        rendered["volumes"]["osb-data"]["driver_opts"] = {
            "type": "none",
            "o": "bind",
            "device": "/var/lib/docker/volumes/osb-data-live/_data",
        }
        rendered_compose.write_text(json.dumps(rendered), encoding="utf-8")
        try:
            command_compose_plan_verify(compose_arguments)
        except ContractError:
            pass
        else:
            raise AssertionError("candidate Compose verifier accepted a bind-aliased named volume")
        (deployment / "osb.intent.json").write_text("{}\n", encoding="utf-8")
        state = deployment / ".osb-update"
        state.mkdir(mode=0o700)
        snapshot = state / "test-snapshot"
        command_snapshot(argparse.Namespace(deployment=deployment, output=snapshot))
        command_snapshot_matches(argparse.Namespace(deployment=deployment, snapshot=snapshot))
        (deployment / "config.toml").write_text("changed\n", encoding="utf-8")
        try:
            command_snapshot_matches(argparse.Namespace(deployment=deployment, snapshot=snapshot))
        except ContractError:
            pass
        else:
            raise AssertionError("snapshot comparison accepted a changed live control")
        (deployment / "admin-access-key.txt").write_text("created-during-update\n", encoding="utf-8")
        command_restore(argparse.Namespace(deployment=deployment, snapshot=snapshot))
        assert (deployment / "config.toml").read_text(encoding="utf-8") == "original-config.toml\n"
        assert not (deployment / "admin-access-key.txt").exists()
        assert (
            state / "rollback-unexpected-controls" / "admin-access-key.txt"
        ).read_text(encoding="utf-8") == "created-during-update\n"
    sys.stdout.write("osb update support self-test: ok\n")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)

    plan = commands.add_parser("channel-plan")
    plan.add_argument("--channel", type=Path, required=True)
    plan.add_argument("--current", required=True)
    plan.add_argument("--target")
    plan.add_argument("--allow-major", action="store_true")
    plan.add_argument("--output", type=Path, required=True)
    plan.set_defaults(handler=command_channel_plan)

    release = commands.add_parser("release-verify")
    release.add_argument("--file", type=Path, required=True)
    release.add_argument("--version", required=True)
    release.add_argument("--release-date", required=True)
    release.add_argument("--sha256", required=True)
    release.set_defaults(handler=command_release_verify)

    github_release = commands.add_parser("github-release-verify")
    github_release.add_argument("--file", type=Path, required=True)
    github_release.add_argument("--tag", required=True)
    github_release.add_argument("--url", required=True)
    github_release.add_argument("--release-date", required=True)
    github_release.set_defaults(handler=command_github_release_verify)

    source = commands.add_parser("source-info")
    source.add_argument("--file", type=Path, required=True)
    source.add_argument("--output", type=Path, required=True)
    source.set_defaults(handler=command_source_info)

    lock = commands.add_parser("lock-info")
    lock.add_argument("--file", type=Path, required=True)
    lock.add_argument("--output", type=Path, required=True)
    lock.set_defaults(handler=command_lock_info)

    promote = commands.add_parser("promote-lock")
    promote.add_argument("--source", type=Path, required=True)
    promote.add_argument("--target", type=Path, required=True)
    promote.add_argument("--from-version", required=True)
    promote.add_argument("--to-version", required=True)
    promote.set_defaults(handler=command_promote_lock)

    compose_plan = commands.add_parser("compose-plan-verify")
    compose_plan.add_argument("--file", type=Path, required=True)
    compose_plan.add_argument("--image", required=True)
    compose_plan.add_argument("--data-volume", required=True)
    compose_plan.add_argument("--forbidden-data-volume", required=True)
    compose_plan.add_argument("--backup-root", type=Path, required=True)
    compose_plan.add_argument("--control-root", type=Path, required=True)
    compose_plan.add_argument("--source-root", type=Path, required=True)
    compose_plan.add_argument(
        "--active-profile", choices=("none", "redis-standalone", "redis-managed"), required=True
    )
    compose_plan.set_defaults(handler=command_compose_plan_verify)

    snapshot = commands.add_parser("snapshot")
    snapshot.add_argument("--deployment", type=Path, required=True)
    snapshot.add_argument("--output", type=Path, required=True)
    snapshot.set_defaults(handler=command_snapshot)

    restore = commands.add_parser("restore")
    restore.add_argument("--deployment", type=Path, required=True)
    restore.add_argument("--snapshot", type=Path, required=True)
    restore.set_defaults(handler=command_restore)

    snapshot_matches = commands.add_parser("snapshot-matches")
    snapshot_matches.add_argument("--deployment", type=Path, required=True)
    snapshot_matches.add_argument("--snapshot", type=Path, required=True)
    snapshot_matches.set_defaults(handler=command_snapshot_matches)

    env = commands.add_parser("env-rewrite")
    env.add_argument("--source", type=Path, required=True)
    env.add_argument("--output", type=Path, required=True)
    env.add_argument("--set", action="append", default=[], required=True)
    env.set_defaults(handler=command_env_rewrite)

    env_info = commands.add_parser("env-info")
    env_info.add_argument("--file", type=Path, required=True)
    env_info.add_argument("--output", type=Path, required=True)
    env_info.set_defaults(handler=command_env_info)

    health = commands.add_parser("health")
    health.add_argument("--expected", required=True)
    health.add_argument("--live", type=Path, required=True)
    health.add_argument("--ready", type=Path, required=True)
    health.add_argument("--health", type=Path, required=True)
    health.add_argument("--feed", type=Path, required=True)
    health.set_defaults(handler=command_health)

    self_test = commands.add_parser("self-test")
    self_test.set_defaults(handler=command_self_test)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        args.handler(args)
    except ContractError as error:
        sys.stderr.write(f"osb update contract error: {error}\n")
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
