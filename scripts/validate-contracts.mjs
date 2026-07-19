import { readFile, readdir } from "node:fs/promises";
import { createHash } from "node:crypto";
import path from "node:path";
import process from "node:process";
import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { parse as parseToml } from "smol-toml";
import YAML from "yaml";

const root = process.cwd();
const ajv = new Ajv2020({ allErrors: true, strict: true });
addFormats(ajv);

const openApiPath = "openapi/openapi.yaml";
const openApiText = await readFile(path.join(root, openApiPath), "utf8");
const openApiDocument = YAML.parseDocument(openApiText, { uniqueKeys: true });
if (openApiDocument.errors.length > 0) {
  throw new Error(
    `${openApiPath}: YAML parse failed:\n${openApiDocument.errors.map(String).join("\n")}`,
  );
}
const openApi = openApiDocument.toJS();
if (openApi?.openapi !== "3.1.0" || typeof openApi.paths !== "object") {
  throw new Error(`${openApiPath}: expected an OpenAPI 3.1.0 document with paths`);
}

const httpMethods = new Set(["get", "put", "post", "delete", "options", "head", "patch", "trace"]);
const documentedRoutes = new Map();
const operationIds = new Set();
for (const [route, pathItem] of Object.entries(openApi.paths)) {
  if (!route.startsWith("/") || typeof pathItem !== "object" || pathItem === null) {
    throw new Error(`${openApiPath}: invalid Path Item ${route}`);
  }
  for (const [method, operation] of Object.entries(pathItem)) {
    if (!httpMethods.has(method)) continue;
    if (operation?.["x-osb-route-source"] !== true) {
      throw new Error(`${openApiPath}: ${method.toUpperCase()} ${route} lacks x-osb-route-source`);
    }
    if (typeof operation.operationId !== "string" || operationIds.has(operation.operationId)) {
      throw new Error(`${openApiPath}: missing or duplicate operationId at ${method.toUpperCase()} ${route}`);
    }
    operationIds.add(operation.operationId);
    documentedRoutes.set(`${method.toUpperCase()} ${route}`, operation);
  }
}

const resolveLocalReference = (reference) => {
  if (!reference.startsWith("#/")) return undefined;
  return reference
    .slice(2)
    .split("/")
    .map((part) => part.replaceAll("~1", "/").replaceAll("~0", "~"))
    .reduce((value, part) => value?.[part], openApi);
};
const visitReferences = (value, location = "#") => {
  if (Array.isArray(value)) {
    value.forEach((item, index) => visitReferences(item, `${location}/${index}`));
    return;
  }
  if (typeof value !== "object" || value === null) return;
  if (typeof value.$ref === "string" && value.$ref.startsWith("#/") && !resolveLocalReference(value.$ref)) {
    throw new Error(`${openApiPath}: unresolved local reference ${value.$ref} at ${location}`);
  }
  for (const [key, child] of Object.entries(value)) visitReferences(child, `${location}/${key}`);
};
visitReferences(openApi);

const discoveryComponentNames = [
  "A2AProtocolStatus",
  "DiscoveryCacheDependency",
  "DiscoveryDependencies",
  "DiscoveryDocument",
  "DiscoveryEndpoint",
  "DiscoveryOperatorIntent",
  "ModuleDescriptor",
];
const discoveryDefinitions = Object.fromEntries(
  discoveryComponentNames.map((name) => {
    const schema = openApi.components?.schemas?.[name];
    if (!schema) throw new Error(`${openApiPath}: missing discovery schema ${name}`);
    return [name, schema];
  }),
);
const discoverySchemaBundle = JSON.parse(
  JSON.stringify({
    $ref: "#/$defs/DiscoveryDocument",
    $defs: discoveryDefinitions,
  }).replaceAll("#/components/schemas/", "#/$defs/"),
);
const validateDiscoveryDocument = ajv.compile(discoverySchemaBundle);
const discoveryFixturePath = "openapi/fixtures/discovery-document.json";
const discoveryFixture = JSON.parse(
  await readFile(path.join(root, discoveryFixturePath), "utf8"),
);
if (!validateDiscoveryDocument(discoveryFixture)) {
  throw new Error(
    `${discoveryFixturePath}: ${ajv.errorsText(validateDiscoveryDocument.errors, { separator: "\n" })}`,
  );
}
const rootRelativeDiscovery = structuredClone(discoveryFixture);
rootRelativeDiscovery.endpoints.feed.href = "/api/v1/feed";
if (validateDiscoveryDocument(rootRelativeDiscovery)) {
  throw new Error(`${openApiPath}: discovery endpoint hrefs must be absolute public URLs`);
}
process.stdout.write(`discovery fixture ok: ${discoveryFixturePath}\n`);

const serverSourceDirectory = path.join(root, "apps/server/src");
const serverSources = await Promise.all(
  (await readdir(serverSourceDirectory))
    .filter((name) => name.endsWith(".rs"))
    .sort()
    .map((name) => readFile(path.join(serverSourceDirectory, name), "utf8")),
);
const serverSource = serverSources.join("\n");
const isContractRoute = (route) =>
  route === "/healthz" ||
  route === "/robots.txt" ||
  route === "/sitemap.xml" ||
  route === "/agent.txt" ||
  route === "/agents.txt" ||
  route === "/llms.txt" ||
  route.startsWith("/api/v1/") ||
  route.startsWith("/.well-known/") ||
  route.startsWith("/media/") ||
  route.startsWith("/openapi/");
const implementedRoutes = new Set();
const routePattern =
  /\.route\(\s*"([^"]+)"\s*,\s*(get|put|post|delete|options|head|patch)\s*\(/g;
for (const match of serverSource.matchAll(routePattern)) {
  if (isContractRoute(match[1])) implementedRoutes.add(`${match[2].toUpperCase()} ${match[1]}`);
}

const missingFromContract = [...implementedRoutes].filter((route) => !documentedRoutes.has(route));
const missingFromServer = [...documentedRoutes.keys()].filter((route) => !implementedRoutes.has(route));
if (missingFromContract.length > 0 || missingFromServer.length > 0) {
  throw new Error(
    `${openApiPath}: server route drift` +
      `\nmissing from contract: ${missingFromContract.join(", ") || "none"}` +
      `\nmissing from server: ${missingFromServer.join(", ") || "none"}`,
  );
}

const ownerRoutes = new Set([
  "GET /api/v1/admin/documents",
  "GET /api/v1/admin/documents/{id}",
  "GET /api/v1/admin/documents/{id}/revisions",
  "POST /api/v1/posts",
  "POST /api/v1/documents/{id}/revisions",
  "POST /api/v1/documents/{id}/publish",
  "POST /api/v1/ai2ai/proposals",
  "POST /api/v1/assets",
  "POST /api/v1/code-runner/runs",
  "GET /api/v1/code-runner/runs/{id}",
]);
const mcpContentRoutes = new Set([
  "GET /api/v1/admin/documents",
  "GET /api/v1/admin/documents/{id}",
  "GET /api/v1/admin/documents/{id}/revisions",
  "POST /api/v1/posts",
  "POST /api/v1/documents/{id}/revisions",
  "POST /api/v1/documents/{id}/publish",
]);

const securityAlternatives = (operation) =>
  Array.isArray(operation.security) ? operation.security : [];
const mentionsSecurityScheme = (operation, scheme) =>
  securityAlternatives(operation).some(
    (requirement) => typeof requirement === "object"
      && requirement !== null
      && Object.hasOwn(requirement, scheme),
  );
const hasEmptyScopeAlternative = (operation, scheme) =>
  securityAlternatives(operation).some((requirement) => {
    if (typeof requirement !== "object" || requirement === null) return false;
    const keys = Object.keys(requirement);
    return keys.length === 1
      && keys[0] === scheme
      && Array.isArray(requirement[scheme])
      && requirement[scheme].length === 0;
  });

for (const route of ownerRoutes) {
  const operation = documentedRoutes.get(route);
  const alternatives = securityAlternatives(operation);
  const expectsMcpBearer = mcpContentRoutes.has(route);
  if (
    alternatives.length !== (expectsMcpBearer ? 2 : 1)
    || !hasEmptyScopeAlternative(operation, "SessionCookie")
    || hasEmptyScopeAlternative(operation, "McpBearer") !== expectsMcpBearer
    || mentionsSecurityScheme(operation, "OwnerBearer")
  ) {
    throw new Error(
      `${openApiPath}: ${route} has incorrect SessionCookie/McpBearer alternatives`,
    );
  }
}
for (const [route, operation] of documentedRoutes) {
  if (mentionsSecurityScheme(operation, "OwnerBearer")) {
    throw new Error(`${openApiPath}: removed OwnerBearer security appears at ${route}`);
  }
}
for (const [route, operation] of documentedRoutes) {
  const expectsMcpBearer = mcpContentRoutes.has(route);
  const hasMcpBearerAlternative = hasEmptyScopeAlternative(operation, "McpBearer");
  const mentionsMcpBearer = mentionsSecurityScheme(operation, "McpBearer");
  if (
    (expectsMcpBearer && !hasMcpBearerAlternative)
    || (!expectsMcpBearer && mentionsMcpBearer)
  ) {
    throw new Error(`${openApiPath}: McpBearer content-scope drift at ${route}`);
  }
}
if (!openApi.components?.securitySchemes?.McpBearer) {
  throw new Error(`${openApiPath}: missing McpBearer security scheme`);
}
if (openApi.components?.securitySchemes?.OwnerBearer) {
  throw new Error(`${openApiPath}: removed OwnerBearer security scheme is still declared`);
}
const sessionRoutes = new Set([
  ...ownerRoutes,
  "GET /api/v1/session",
  "POST /api/v1/auth/logout",
  "POST /api/v1/blogs",
  "GET /api/v1/admin/home/pins",
  "PUT /api/v1/admin/home/pins",
  "GET /api/v1/studio/documents",
  "GET /api/v1/studio/documents/{id}",
  "POST /api/v1/studio/documents",
  "POST /api/v1/studio/documents/{id}/revisions",
  "POST /api/v1/studio/documents/{id}/publish",
  "POST /api/v1/studio/preview",
  "POST /api/v1/studio/assets",
  "GET /api/v1/studio/settings",
  "PUT /api/v1/studio/settings",
  "GET /api/v1/studio/collaborators",
  "POST /api/v1/studio/collaborators",
  "DELETE /api/v1/studio/collaborators/{userId}",
  "POST /api/v1/posts/{id}/comments",
]);
for (const [route, operation] of documentedRoutes) {
  const expectsSessionCookie = sessionRoutes.has(route);
  const hasSessionCookieAlternative = hasEmptyScopeAlternative(operation, "SessionCookie");
  const mentionsSessionCookie = mentionsSecurityScheme(operation, "SessionCookie");
  if (
    (expectsSessionCookie && !hasSessionCookieAlternative)
    || (!expectsSessionCookie && mentionsSessionCookie)
  ) {
    throw new Error(`${openApiPath}: SessionCookie security drift at ${route}`);
  }
}
if (!serverSource.includes('absolute_public_url(&state.seo_policy, "/openapi/openapi.yaml")')) {
  throw new Error("AI2AI discovery does not link to /openapi/openapi.yaml");
}
process.stdout.write(
  `openapi ok: ${openApiPath} (${documentedRoutes.size} operations, route/auth drift checked)\n`,
);

const schemaDirectories = ["schemas", "providers"];
const schemaPaths = [];
for (const directory of schemaDirectories) {
  for (const name of await readdir(path.join(root, directory))) {
    if (name.endsWith(".schema.json")) schemaPaths.push(path.join(directory, name));
  }
}

const loadedSchemas = [];
for (const relative of schemaPaths.sort()) {
  const schema = JSON.parse(await readFile(path.join(root, relative), "utf8"));
  ajv.addSchema(schema);
  loadedSchemas.push({ relative, schema });
}
for (const { relative, schema } of loadedSchemas) {
  if (!ajv.getSchema(schema.$id)) throw new Error(`schema did not compile: ${relative}`);
  process.stdout.write(`schema ok: ${relative}\n`);
}

const installationIntentSchemaId =
  "urn:open-soverign-blog:schemas:installation-intent:v1";
const installationLockSchemaId =
  "urn:open-soverign-blog:schemas:installation-lock:v1";
const validateInstallationIntent = ajv.getSchema(installationIntentSchemaId);
const validateInstallationLock = ajv.getSchema(installationLockSchemaId);
if (!validateInstallationIntent || !validateInstallationLock) {
  throw new Error("installation intent/lock schemas were not loaded");
}

const installationIntentPath = "osb.install.example.toml";
const installationLockPath = "osb.lock.example.json";
const installationIntent = parseToml(
  await readFile(path.join(root, installationIntentPath), "utf8"),
);
const installationLock = JSON.parse(
  await readFile(path.join(root, installationLockPath), "utf8"),
);
if (!validateInstallationIntent(installationIntent)) {
  throw new Error(
    `${installationIntentPath}: ${ajv.errorsText(validateInstallationIntent.errors, {
      separator: "\n",
    })}`,
  );
}
if (!validateInstallationLock(installationLock)) {
  throw new Error(
    `${installationLockPath}: ${ajv.errorsText(validateInstallationLock.errors, {
      separator: "\n",
    })}`,
  );
}

const canonicalJson = (value) => {
  if (Array.isArray(value)) return value.map(canonicalJson);
  if (typeof value !== "object" || value === null) return value;
  return Object.fromEntries(
    Object.entries(value)
      .sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
      .map(([key, child]) => [key, canonicalJson(child)]),
  );
};
const lockPayload = structuredClone(installationLock);
lockPayload.lockDigest = "";
const expectedLockDigest = createHash("sha256")
  .update(JSON.stringify(canonicalJson(lockPayload)))
  .digest("hex");
if (expectedLockDigest !== installationLock.lockDigest) {
  throw new Error(
    `${installationLockPath}: lockDigest does not cover the canonical lock payload`,
  );
}
if (
  installationIntent.installation_id !== installationLock.installationId
  || JSON.stringify(canonicalJson(installationIntent.selection))
    !== JSON.stringify(canonicalJson(installationLock.selection))
) {
  throw new Error("installation example intent and lock identity/selection differ");
}

const requestedDlcs = new Map();
for (const request of installationIntent.dlcs ?? []) {
  if (requestedDlcs.has(request.id)) {
    throw new Error(`${installationIntentPath}: duplicate DLC id ${request.id}`);
  }
  requestedDlcs.set(request.id, {
    version: request.version,
    enabled: request.enabled ?? true,
  });
}
const installedDlcs = new Map();
let previousDlcId;
for (const installed of installationLock.dlcs) {
  if (previousDlcId !== undefined && previousDlcId >= installed.id) {
    throw new Error(`${installationLockPath}: DLC records are not strictly sorted by id`);
  }
  previousDlcId = installed.id;
  installedDlcs.set(installed.id, {
    version: installed.requestedVersion,
    enabled: installed.enabled,
  });
  if (
    installed.approvedCapabilities?.some(
      (capability, index, values) => index > 0 && values[index - 1] >= capability,
    )
  ) {
    throw new Error(
      `${installationLockPath}: approved capabilities for ${installed.id} are not sorted`,
    );
  }
  if (installed.sourceKind === "bundled") {
    const bundledBytes = await readFile(path.join(root, installed.source));
    const bundledDigest = createHash("sha256").update(bundledBytes).digest("hex");
    if (bundledDigest !== installed.manifestSha256) {
      throw new Error(
        `${installationLockPath}: bundled manifest digest differs for ${installed.id}`,
      );
    }
  }
}
if (JSON.stringify([...requestedDlcs]) !== JSON.stringify([...installedDlcs])) {
  throw new Error("installation example requested and exact installed DLC sets differ");
}
for (const [index, record] of (installationLock.history ?? []).entries()) {
  if (record.sequence !== index + 1) {
    throw new Error(`${installationLockPath}: DLC history sequence is not contiguous`);
  }
  if (
    ["enabled", "disabled"].includes(record.action)
    && record.fromVersion !== record.toVersion
  ) {
    throw new Error(
      `${installationLockPath}: ${record.action} history must retain one exact version`,
    );
  }
}

const invalidStyleIntent = structuredClone(installationIntent);
invalidStyleIntent.selection.style = { kind: "none", id: "paper" };
if (validateInstallationIntent(invalidStyleIntent)) {
  throw new Error("installation intent schema accepted fields forbidden by style kind none");
}
if (installationLock.dlcs.length > 0) {
  const invalidFileLock = structuredClone(installationLock);
  invalidFileLock.dlcs[0].sourceKind = "file";
  delete invalidFileLock.dlcs[0].artifactSha256;
  if (validateInstallationLock(invalidFileLock)) {
    throw new Error("installation lock schema accepted a file DLC without artifactSha256");
  }
}
process.stdout.write(
  `installation examples ok: ${installationIntentPath} + ${installationLockPath}\n`,
);

const pluginManifestSchemaId = "urn:ai-native-publishing:schemas:plugin-manifest:v1";
const validatePluginManifest = ajv.getSchema(pluginManifestSchemaId);
if (!validatePluginManifest) throw new Error("plugin manifest schema was not loaded");

const pluginManifestPaths = [];
for (const directory of await readdir(path.join(root, "plugins/official"), {
  withFileTypes: true,
})) {
  if (!directory.isDirectory()) continue;
  pluginManifestPaths.push(path.join("plugins/official", directory.name, "plugin.toml"));
}
pluginManifestPaths.push("crates/plugin-api/tests/fixtures/rich-plugin.toml");

for (const relative of pluginManifestPaths.sort()) {
  let value;
  try {
    value = parseToml(await readFile(path.join(root, relative), "utf8"));
  } catch (error) {
    process.stderr.write(`${relative}: TOML parse failed: ${String(error)}\n`);
    process.exitCode = 1;
    continue;
  }
  if (!validatePluginManifest(value)) {
    process.stderr.write(
      `${relative}: ${ajv.errorsText(validatePluginManifest.errors, { separator: "\n" })}\n`,
    );
    process.exitCode = 1;
  } else {
    process.stdout.write(`plugin manifest ok: ${relative}\n`);
  }
}

const providerSchema = JSON.parse(
  await readFile(path.join(root, "providers/provider.schema.json"), "utf8"),
);
const validateProvider = ajv.getSchema(providerSchema.$id) ?? ajv.compile(providerSchema);
const providerDocuments = new Map();
for (const name of (await readdir(path.join(root, "providers"))).sort()) {
  if (!name.endsWith(".yaml") && !name.endsWith(".yml")) continue;
  const value = YAML.parse(await readFile(path.join(root, "providers", name), "utf8"));
  if (!validateProvider(value)) {
    process.stderr.write(`${name}: ${ajv.errorsText(validateProvider.errors, { separator: "\n" })}\n`);
    process.exitCode = 1;
  } else {
    process.stdout.write(`provider ok: providers/${name}\n`);
    providerDocuments.set(`providers/${name}`, value);
  }
}

const indexSchema = JSON.parse(
  await readFile(path.join(root, "providers/index.schema.json"), "utf8"),
);
const validateIndex = ajv.getSchema(indexSchema.$id) ?? ajv.compile(indexSchema);
const providerIndex = JSON.parse(
  await readFile(path.join(root, "providers/index.json"), "utf8"),
);
if (!validateIndex(providerIndex)) {
  process.stderr.write(
    `providers/index.json: ${ajv.errorsText(validateIndex.errors, { separator: "\n" })}\n`,
  );
  process.exitCode = 1;
} else {
  const indexedPaths = new Set();
  const indexedIds = new Set();
  for (const entry of providerIndex.entries) {
    if (indexedPaths.has(entry.path)) {
      throw new Error(`duplicate provider path in index: ${entry.path}`);
    }
    if (indexedIds.has(entry.id)) {
      throw new Error(`duplicate provider id in index: ${entry.id}`);
    }
    indexedPaths.add(entry.path);
    indexedIds.add(entry.id);

    const provider = providerDocuments.get(entry.path);
    if (!provider) throw new Error(`indexed provider file does not exist: ${entry.path}`);
    for (const key of ["id", "adapterStatus", "lastVerified"]) {
      if (provider[key] !== entry[key]) {
        throw new Error(`index mismatch for ${entry.path}: ${key}`);
      }
    }
    if (Boolean(provider.example) !== entry.example) {
      throw new Error(`index mismatch for ${entry.path}: example`);
    }
  }

  for (const providerPath of providerDocuments.keys()) {
    if (!indexedPaths.has(providerPath)) {
      throw new Error(`provider file missing from index: ${providerPath}`);
    }
  }
  process.stdout.write("provider index ok: providers/index.json\n");
}
