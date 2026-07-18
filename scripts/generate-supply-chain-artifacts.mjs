import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const checkOnly = process.argv.slice(2).includes("--check");
const unexpectedArguments = process.argv.slice(2).filter((value) => value !== "--check");
if (unexpectedArguments.length > 0) {
  throw new Error(`unknown arguments: ${unexpectedArguments.join(", ")}`);
}

const outputPaths = {
  inventory: path.join(repositoryRoot, "docs/legal/dependency-inventory.json"),
  sbom: path.join(repositoryRoot, "docs/legal/application-sbom.cdx.json"),
  licenses: path.join(repositoryRoot, "THIRD_PARTY_LICENSES.txt"),
};
const noticesPath = path.join(repositoryRoot, "THIRD_PARTY_NOTICES.md");

const reviewedNpmLicenses = new Set([
  "(MPL-2.0 OR Apache-2.0)",
  "Apache-2.0",
  "BSD-3-Clause",
  "CC-BY-4.0",
  "ISC",
  "MIT",
  "Unlicense",
]);
const npmLicenseChoices = new Map([["dompurify", "Apache-2.0"]]);
const distributedBrowserPackages = new Set([
  "dompurify",
  "katex",
  "react",
  "react-dom",
  "scheduler",
]);

const cargoMetadata = JSON.parse(
  execFileSync(
    "cargo",
    ["metadata", "--locked", "--format-version", "1", "--all-features"],
    {
      cwd: repositoryRoot,
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      stdio: ["ignore", "pipe", "inherit"],
    },
  ),
);
const cargoLock = parseCargoLock(
  fs.readFileSync(path.join(repositoryRoot, "Cargo.lock"), "utf8"),
);
const npmLock = JSON.parse(
  fs.readFileSync(path.join(repositoryRoot, "package-lock.json"), "utf8"),
);

const licenseTexts = new Map();
const rust = cargoMetadata.packages
  .filter((pkg) => pkg.source?.startsWith("registry+"))
  .map((pkg) => {
    if (!pkg.license) throw new Error(`Cargo package ${pkg.name}@${pkg.version} lacks license metadata`);
    const locked = cargoLock.get(cargoKey(pkg.name, pkg.version, pkg.source));
    if (!locked) throw new Error(`Cargo package ${pkg.name}@${pkg.version} is absent from Cargo.lock`);
    const files = findLicenseFiles(path.dirname(pkg.manifest_path), pkg.license_file);
    registerLicenseTexts(licenseTexts, `cargo:${pkg.name}@${pkg.version}`, pkg.license, files);
    return {
      name: pkg.name,
      version: pkg.version,
      licenseExpression: pkg.license,
      source: pkg.source,
      checksum: locked.checksum,
      hasPackagedLicenseText: files.length > 0,
    };
  })
  .sort(compareComponents);

const npmByIdentity = new Map();
for (const [lockPath, pkg] of Object.entries(npmLock.packages ?? {})) {
  if (!lockPath.includes("node_modules/") || pkg.link) continue;
  const name = pkg.name ?? npmNameFromLockPath(lockPath);
  if (!name || !pkg.version) throw new Error(`cannot identify npm package at ${lockPath}`);
  if (!pkg.license) throw new Error(`npm package ${name}@${pkg.version} lacks license metadata`);
  if (!reviewedNpmLicenses.has(pkg.license)) {
    throw new Error(`npm package ${name}@${pkg.version} uses unreviewed license ${pkg.license}`);
  }
  const identity = `${name}\0${pkg.version}\0${pkg.integrity ?? pkg.resolved ?? ""}`;
  const existing = npmByIdentity.get(identity);
  if (existing) {
    existing.lockPaths.push(lockPath);
    existing.development &&= Boolean(pkg.dev);
    existing.optional ||= Boolean(pkg.optional);
    continue;
  }
  const installedDirectory = path.join(repositoryRoot, lockPath);
  const files = fs.existsSync(installedDirectory) ? findLicenseFiles(installedDirectory) : [];
  registerLicenseTexts(licenseTexts, `npm:${name}@${pkg.version}`, pkg.license, files);
  npmByIdentity.set(identity, {
    name,
    version: pkg.version,
    licenseExpression: pkg.license,
    selectedLicense: npmLicenseChoices.get(name) ?? null,
    resolved: pkg.resolved ?? null,
    integrity: pkg.integrity ?? null,
    development: Boolean(pkg.dev),
    optional: Boolean(pkg.optional),
    lockPaths: [lockPath],
    hasPackagedLicenseText: files.length > 0,
  });
}
const npm = [...npmByIdentity.values()]
  .map((pkg) => ({ ...pkg, lockPaths: pkg.lockPaths.sort() }))
  .sort(compareComponents);

for (const name of distributedBrowserPackages) {
  const packages = npm.filter((pkg) => pkg.name === name);
  if (packages.length === 0 || packages.some((pkg) => !pkg.hasPackagedLicenseText)) {
    throw new Error(`distributed browser package ${name} lacks a collected license file`);
  }
}

verifyMplNotice(rust, npm, fs.readFileSync(noticesPath, "utf8"));

const inventory = {
  schemaVersion: "open-soverign-blog-dependency-inventory/1",
  project: {
    name: "OpenSoverignBlog",
    version: "0.1.0",
    licenseExpression: "Unlicense",
  },
  scope: {
    cargo: "Complete resolved third-party Cargo graph from cargo metadata --locked --all-features.",
    npm: "Every non-workspace third-party package entry in package-lock.json.",
    container: "Application dependencies only; Debian/base-image packages require an image SBOM.",
  },
  rust,
  npm,
};

const sbom = {
  $schema: "http://cyclonedx.org/schema/bom-1.5.schema.json",
  bomFormat: "CycloneDX",
  specVersion: "1.5",
  version: 1,
  metadata: {
    component: {
      type: "application",
      "bom-ref": "pkg:generic/open-soverign-blog@0.1.0",
      name: "OpenSoverignBlog",
      version: "0.1.0",
      licenses: [{ license: { id: "Unlicense" } }],
    },
    properties: [
      { name: "osb:scope", value: "application-dependencies" },
      { name: "osb:container-base-included", value: "false" },
    ],
  },
  components: [
    ...rust.map(cargoSbomComponent),
    ...npm.map(npmSbomComponent),
  ].sort((left, right) => left["bom-ref"].localeCompare(right["bom-ref"])),
};

const outputs = new Map([
  [outputPaths.inventory, `${JSON.stringify(inventory, null, 2)}\n`],
  [outputPaths.sbom, `${JSON.stringify(sbom, null, 2)}\n`],
  [outputPaths.licenses, renderLicenseTexts(licenseTexts, rust, npm)],
]);

let mismatch = false;
for (const [file, expected] of outputs) {
  if (checkOnly) {
    const actual = fs.existsSync(file) ? fs.readFileSync(file, "utf8") : null;
    if (actual !== expected) {
      console.error(`supply-chain artifact is stale: ${path.relative(repositoryRoot, file)}`);
      mismatch = true;
    }
  } else {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, expected);
    console.log(`wrote ${path.relative(repositoryRoot, file)}`);
  }
}
if (mismatch) {
  console.error("run `npm run supply-chain:generate` and review the resulting license changes");
  process.exitCode = 1;
} else if (checkOnly) {
  console.log(`supply-chain artifacts match ${rust.length} Cargo and ${npm.length} npm packages`);
}

function parseCargoLock(source) {
  const packages = new Map();
  for (const section of source.split(/\n\[\[package\]\]\n/).slice(1)) {
    const name = tomlString(section, "name");
    const version = tomlString(section, "version");
    const packageSource = tomlString(section, "source");
    const checksum = tomlString(section, "checksum");
    if (name && version && packageSource) {
      packages.set(cargoKey(name, version, packageSource), { checksum: checksum ?? null });
    }
  }
  return packages;
}

function tomlString(section, key) {
  const match = section.match(new RegExp(`^${key} = "([^"]*)"$`, "m"));
  return match?.[1] ?? null;
}

function cargoKey(name, version, source) {
  return `${name}\0${version}\0${source}`;
}

function npmNameFromLockPath(lockPath) {
  const marker = "node_modules/";
  const remainder = lockPath.slice(lockPath.lastIndexOf(marker) + marker.length);
  const parts = remainder.split("/");
  return parts[0]?.startsWith("@") ? parts.slice(0, 2).join("/") : parts[0];
}

function compareComponents(left, right) {
  return left.name.localeCompare(right.name) || left.version.localeCompare(right.version);
}

function findLicenseFiles(directory, explicitFile = null) {
  const files = new Set();
  if (explicitFile && fs.existsSync(explicitFile) && fs.statSync(explicitFile).isFile()) {
    files.add(path.resolve(explicitFile));
  }
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    if (
      entry.isFile() &&
      /^(licen[cs]e|copying|notice|authors)([._-].*)?$/i.test(entry.name)
    ) {
      files.add(path.join(directory, entry.name));
    }
  }
  return [...files].sort((left, right) => path.basename(left).localeCompare(path.basename(right)));
}

function registerLicenseTexts(collection, component, expression, files) {
  for (const file of files) {
    const content = normalizeText(fs.readFileSync(file, "utf8"));
    const digest = createHash("sha256").update(content).digest("hex");
    const record = collection.get(digest) ?? {
      digest,
      components: new Set(),
      fileNames: new Set(),
      content,
    };
    record.components.add(`${component} [${expression}]`);
    record.fileNames.add(path.basename(file));
    collection.set(digest, record);
  }
}

function normalizeText(value) {
  return `${value
    .replaceAll("\r\n", "\n")
    .replace(/[ \t]+$/gm, "")
    .trimEnd()}\n`;
}

function verifyMplNotice(rustPackages, npmPackages, notices) {
  for (const pkg of rustPackages.filter((candidate) => candidate.licenseExpression === "MPL-2.0")) {
    const marker = `\`${pkg.name}@${pkg.version}\``;
    if (!notices.includes(marker)) throw new Error(`MPL-only dependency is missing from notices: ${marker}`);
  }
  for (const [name, selected] of npmLicenseChoices) {
    for (const pkg of npmPackages.filter((candidate) => candidate.name === name)) {
      const marker = `\`${pkg.name}@${pkg.version}\``;
      if (!notices.includes(marker) || !notices.includes(`elects the ${selected} option`)) {
        throw new Error(`npm license choice is missing from notices: ${marker} -> ${selected}`);
      }
    }
  }
}

function cargoSbomComponent(pkg) {
  return {
    type: "library",
    "bom-ref": cargoPurl(pkg.name, pkg.version),
    name: pkg.name,
    version: pkg.version,
    licenses: [{ expression: normalizedSpdxExpression(pkg.licenseExpression) }],
    purl: cargoPurl(pkg.name, pkg.version),
    ...(pkg.checksum
      ? { hashes: [{ alg: "SHA-256", content: pkg.checksum.toUpperCase() }] }
      : {}),
    properties: [{ name: "osb:ecosystem", value: "cargo" }],
  };
}

function npmSbomComponent(pkg) {
  const hashes = integrityHashes(pkg.integrity);
  return {
    type: "library",
    "bom-ref": npmPurl(pkg.name, pkg.version),
    name: pkg.name,
    version: pkg.version,
    licenses: [{ expression: normalizedSpdxExpression(pkg.licenseExpression) }],
    purl: npmPurl(pkg.name, pkg.version),
    ...(hashes.length > 0 ? { hashes } : {}),
    ...(pkg.resolved?.startsWith("https://")
      ? { externalReferences: [{ type: "distribution", url: pkg.resolved }] }
      : {}),
    properties: [
      { name: "osb:ecosystem", value: "npm" },
      { name: "osb:development", value: String(pkg.development) },
      { name: "osb:optional", value: String(pkg.optional) },
      ...(pkg.selectedLicense
        ? [{ name: "osb:selected-license", value: pkg.selectedLicense }]
        : []),
    ],
  };
}

function normalizedSpdxExpression(value) {
  // A few older Cargo packages publish this historical non-SPDX shorthand.
  // Preserve their declaration in the inventory, but emit valid SPDX in CDX.
  return value === "MIT/Apache-2.0" ? "MIT OR Apache-2.0" : value;
}

function cargoPurl(name, version) {
  return `pkg:cargo/${encodeURIComponent(name)}@${encodeURIComponent(version)}`;
}

function npmPurl(name, version) {
  if (name.startsWith("@")) {
    const [scope, packageName] = name.split("/");
    return `pkg:npm/${encodeURIComponent(scope)}/${encodeURIComponent(packageName)}@${encodeURIComponent(version)}`;
  }
  return `pkg:npm/${encodeURIComponent(name)}@${encodeURIComponent(version)}`;
}

function integrityHashes(integrity) {
  if (!integrity) return [];
  const algorithmNames = new Map([
    ["sha256", "SHA-256"],
    ["sha384", "SHA-384"],
    ["sha512", "SHA-512"],
  ]);
  return integrity
    .split(/\s+/)
    .map((entry) => entry.match(/^([a-z0-9]+)-(.+)$/i))
    .filter(Boolean)
    .map((match) => ({
      alg: algorithmNames.get(match[1].toLowerCase()),
      content: Buffer.from(match[2], "base64").toString("hex").toUpperCase(),
    }))
    .filter((hash) => hash.alg);
}

function renderLicenseTexts(texts, rustPackages, npmPackages) {
  const records = [...texts.values()].sort((left, right) => left.digest.localeCompare(right.digest));
  const missingNpmText = npmPackages
    .filter((pkg) => !pkg.hasPackagedLicenseText)
    .map((pkg) => `npm:${pkg.name}@${pkg.version}`)
    .sort();
  const missingRustText = rustPackages
    .filter((pkg) => !pkg.hasPackagedLicenseText)
    .map((pkg) => `cargo:${pkg.name}@${pkg.version}`)
    .sort();
  const lines = [
    "OpenSoverignBlog third-party license texts",
    "",
    "GENERATED FILE. Run: npm run supply-chain:generate",
    "Source: locked Cargo package archives and the Linux npm ci installation.",
    `Inventory: ${rustPackages.length} Cargo packages; ${npmPackages.length} npm packages.`,
    "",
    "Some optional npm packages are not installed on Linux; some native packages",
    "do not carry a standalone license file. Components without collected text are",
    "listed below, and their metadata and sources remain in the JSON inventory.",
    ...(missingNpmText.length > 0
      ? ["", "Lockfile components without a Linux-installed license file:", ...missingNpmText.map((value) => `- ${value}`)]
      : []),
    ...(missingRustText.length > 0
      ? ["", "Cargo package archives without a standalone license/notice file:", ...missingRustText.map((value) => `- ${value}`)]
      : []),
    "",
  ];
  for (const record of records) {
    lines.push("=".repeat(80));
    lines.push(`SHA-256: ${record.digest}`);
    lines.push(`Packaged file names: ${[...record.fileNames].sort().join(", ")}`);
    lines.push("Components:");
    lines.push(...[...record.components].sort().map((value) => `- ${value}`));
    lines.push("-".repeat(80));
    lines.push(record.content.trimEnd());
    lines.push("");
  }
  return `${lines.join("\n").trimEnd()}\n`;
}
