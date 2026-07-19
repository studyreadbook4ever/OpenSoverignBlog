import { readFile, stat } from "node:fs/promises";
import { createHash } from "node:crypto";
import path from "node:path";
import process from "node:process";
import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { parse as parseToml } from "smol-toml";
import YAML from "yaml";

const root = process.cwd();
const expectedRepository = "https://github.com/studyreadbook4ever/OpenSoverignBlog";
const expectedDeveloper = "https://eff0rtchung.kr";
const expectedLicense = "Unlicense";
const releasePath = path.join(root, "release.toml");
const channelPath = path.join(root, "release-channel.json");
const channelSchemaPath = path.join(root, "schemas/release-channel.v1.schema.json");

const release = parseToml(await readFile(releasePath, "utf8"));
const requiredReleaseKeys = [
  "schema_version",
  "version",
  "status",
  "channel",
  "repository_url",
  "developer_url",
  "license",
  "license_path",
];
const optionalReleaseKeys = ["release_date"];
assertExactKeys(release, requiredReleaseKeys, optionalReleaseKeys, "release.toml");
assert(release.schema_version === "open-soverign-blog-release/1", "unsupported release.toml schema");
assert(validSemver(release.version), "release.toml version is not SemVer");
assert(["unreleased", "released"].includes(release.status), "release.toml status must be unreleased or released");
assert(release.channel === "stable", "only the stable release channel is supported");
assert(!release.version.includes("-") && !release.version.includes("+"), "stable release version cannot be a prerelease/build");
assert(release.repository_url === expectedRepository, "release repository URL drift");
assert(release.developer_url === expectedDeveloper, "release developer URL drift");
assert(release.license === expectedLicense, "release license must be Unlicense");
assert(release.license_path === "UNLICENSE", "release license path must be UNLICENSE");
if (release.status === "released") {
  assert(validDate(release.release_date), "a released build requires a real YYYY-MM-DD release_date");
} else {
  assert(!Object.hasOwn(release, "release_date"), "an unreleased build must not claim a release_date");
}
await assertRegularFile(path.join(root, release.license_path), "project Unlicense");

const channelBytes = await readFile(channelPath);
assert(channelBytes.length <= 64 * 1024, "release-channel.json exceeds the 64 KiB fetch limit");
const channel = JSON.parse(channelBytes.toString("utf8"));
const channelSchema = JSON.parse(await readFile(channelSchemaPath, "utf8"));
const ajv = new Ajv2020({ allErrors: true, strict: true });
addFormats(ajv);
const validateChannel = ajv.compile(channelSchema);
if (!validateChannel(channel)) {
  throw new Error(`release-channel.json: ${ajv.errorsText(validateChannel.errors, { separator: "\n" })}`);
}
assert(channel.repositoryUrl === release.repository_url, "release/channel repository URL drift");
assert(channel.channel === release.channel, "release/channel name drift");

const versions = new Set();
const tags = new Set();
const commits = new Set();
for (const [index, entry] of channel.releases.entries()) {
  assert(entry.tag === `v${entry.version}`, `release ${entry.version} tag must be v${entry.version}`);
  assert(
    entry.releaseUrl === `${expectedRepository}/releases/tag/${encodeURIComponent(entry.tag)}`,
    `release ${entry.version} URL is not canonical`,
  );
  assert(!versions.has(entry.version), `duplicate release version ${entry.version}`);
  assert(!tags.has(entry.tag), `duplicate release tag ${entry.tag}`);
  assert(!commits.has(entry.sourceCommit), `duplicate release commit ${entry.sourceCommit}`);
  versions.add(entry.version);
  tags.add(entry.tag);
  commits.add(entry.sourceCommit);
  if (index > 0) {
    assert(
      compareSemver(channel.releases[index - 1].version, entry.version) > 0,
      "release-channel releases must be strictly newest-first by SemVer precedence",
    );
  }
}
if (channel.releases.length === 0) {
  assert(channel.latest === null, "an empty channel must have latest=null");
} else {
  assert(
    JSON.stringify(channel.latest) === JSON.stringify(channel.releases[0]),
    "release-channel latest must equal releases[0]",
  );
}
if (release.status === "released") {
  const matching = channel.releases.find((entry) => entry.version === release.version);
  assert(matching, `released engine ${release.version} is absent from release-channel.json`);
  assert(matching.releaseDate === release.release_date, "release date drift between TOML and channel");
  const manifestDigest = createHash("sha256").update(await readFile(releasePath)).digest("hex");
  assert(matching.manifestSha256 === manifestDigest, "release.toml manifestSha256 drift");
}

const cargoRoot = parseToml(await readFile(path.join(root, "Cargo.toml"), "utf8"));
assert(cargoRoot.workspace?.package?.version === release.version, "Cargo workspace version drift");
assert(cargoRoot.workspace?.package?.license === release.license, "Cargo workspace license drift");
for (const member of cargoRoot.workspace?.members ?? []) {
  const manifestPath = path.join(root, member, "Cargo.toml");
  const manifest = parseToml(await readFile(manifestPath, "utf8"));
  const packageVersion = manifest.package?.version;
  assert(
    packageVersion === release.version || packageVersion?.workspace === true,
    `${member}/Cargo.toml package version drift`,
  );
  const packageLicense = manifest.package?.license;
  assert(
    packageLicense === release.license || packageLicense?.workspace === true,
    `${member}/Cargo.toml package license drift`,
  );
  for (const tableName of ["dependencies", "dev-dependencies", "build-dependencies"]) {
    for (const [name, dependency] of Object.entries(manifest[tableName] ?? {})) {
      if (!name.startsWith("osb-") || typeof dependency !== "object" || dependency === null) continue;
      if (!Object.hasOwn(dependency, "path")) continue;
      assert(
        dependency.version === `=${release.version}`,
        `${member}/Cargo.toml ${name} must pin =${release.version}`,
      );
    }
  }
}
const cargoLock = parseToml(await readFile(path.join(root, "Cargo.lock"), "utf8"));
for (const pkg of cargoLock.package ?? []) {
  if (pkg.source !== undefined || !String(pkg.name).startsWith("osb-")) continue;
  assert(pkg.version === release.version, `Cargo.lock ${pkg.name} version drift`);
}

const packagePaths = ["package.json", "apps/web/package.json", "packages/sdk/package.json"];
for (const relative of packagePaths) {
  const manifest = JSON.parse(await readFile(path.join(root, relative), "utf8"));
  assert(manifest.version === release.version, `${relative} version drift`);
  assert(manifest.license === release.license, `${relative} license drift`);
}
const packageLock = JSON.parse(await readFile(path.join(root, "package-lock.json"), "utf8"));
assert(packageLock.version === release.version, "package-lock.json root version drift");
for (const lockPath of ["", "apps/web", "packages/sdk"]) {
  const entry = packageLock.packages?.[lockPath];
  assert(entry?.version === release.version, `package-lock workspace ${lockPath || "<root>"} version drift`);
  assert(entry?.license === release.license, `package-lock workspace ${lockPath || "<root>"} license drift`);
}

const openApiText = await readFile(path.join(root, "openapi/openapi.yaml"), "utf8");
const openApi = YAML.parse(openApiText);
assert(openApi?.info?.version === release.version, "OpenAPI info.version drift");
assert(openApi?.info?.license?.identifier === release.license, "OpenAPI license identifier drift");

const inventory = JSON.parse(
  await readFile(path.join(root, "docs/legal/dependency-inventory.json"), "utf8"),
);
assert(inventory.project?.version === release.version, "dependency inventory version drift");
assert(inventory.project?.licenseExpression === release.license, "dependency inventory license drift");
const sbom = JSON.parse(await readFile(path.join(root, "docs/legal/application-sbom.cdx.json"), "utf8"));
const application = sbom.metadata?.component;
assert(application?.version === release.version, "application SBOM version drift");
assert(
  application?.["bom-ref"] === `pkg:generic/open-soverign-blog@${release.version}`,
  "application SBOM bom-ref drift",
);
assert(
  application?.licenses?.some((item) => item?.license?.id === release.license),
  "application SBOM license drift",
);

const dockerfile = await readFile(path.join(root, "Dockerfile"), "utf8");
assert(dockerfile.includes("ARG OSB_VERSION=development"), "Docker image lacks release version build arg");
assert(
  dockerfile.includes('org.opencontainers.image.version="${OSB_VERSION}"'),
  "Docker OCI version label is not release-driven",
);
assert(
  dockerfile.includes('org.opencontainers.image.licenses="Unlicense"'),
  "Docker OCI license label drift",
);
assert(
  dockerfile.includes("COPY AI2AI.md UNLICENSE") && dockerfile.includes("release.toml release-channel.json"),
  "release image omits release/license metadata",
);

process.stdout.write(
  `release contract ok: ${release.version} (${release.status}), ${channel.releases.length} published stable release(s)\n`,
);

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function assertExactKeys(value, required, optional, label) {
  assert(value && typeof value === "object" && !Array.isArray(value), `${label} must be an object`);
  const allowed = new Set([...required, ...optional]);
  for (const key of required) assert(Object.hasOwn(value, key), `${label} is missing ${key}`);
  for (const key of Object.keys(value)) assert(allowed.has(key), `${label} has unknown key ${key}`);
}

async function assertRegularFile(file, label) {
  const metadata = await stat(file);
  assert(metadata.isFile(), `${label} must be a regular file`);
}

function validDate(value) {
  if (typeof value !== "string" || !/^\d{4}-\d{2}-\d{2}$/.test(value)) return false;
  const parsed = new Date(`${value}T00:00:00Z`);
  return !Number.isNaN(parsed.valueOf()) && parsed.toISOString().slice(0, 10) === value;
}

function validSemver(value) {
  try {
    parseSemver(value);
    return true;
  } catch {
    return false;
  }
}

function compareSemver(left, right) {
  const a = parseSemver(left);
  const b = parseSemver(right);
  for (const key of ["major", "minor", "patch"]) {
    if (a[key] !== b[key]) return a[key] > b[key] ? 1 : -1;
  }
  if (a.pre.length === 0 || b.pre.length === 0) return a.pre.length === b.pre.length ? 0 : a.pre.length === 0 ? 1 : -1;
  for (let index = 0; index < Math.max(a.pre.length, b.pre.length); index += 1) {
    const av = a.pre[index];
    const bv = b.pre[index];
    if (av === undefined || bv === undefined) return av === bv ? 0 : av === undefined ? -1 : 1;
    if (av === bv) continue;
    const an = /^\d+$/.test(av);
    const bn = /^\d+$/.test(bv);
    if (an && bn) return BigInt(av) > BigInt(bv) ? 1 : -1;
    if (an !== bn) return an ? -1 : 1;
    return av > bv ? 1 : -1;
  }
  return 0;
}

function parseSemver(value) {
  const match = String(value).match(
    /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/,
  );
  if (!match) throw new Error(`invalid SemVer ${value}`);
  return {
    major: BigInt(match[1]),
    minor: BigInt(match[2]),
    patch: BigInt(match[3]),
    pre: match[4] ? match[4].split(".") : [],
  };
}
