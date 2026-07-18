import { readFile, readdir } from "node:fs/promises";
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
for (const [route, operation] of documentedRoutes) {
  const usesOwnerBearer = Boolean(
    operation.security?.some((requirement) => Object.hasOwn(requirement, "OwnerBearer")),
  );
  if (usesOwnerBearer !== ownerRoutes.has(route)) {
    throw new Error(`${openApiPath}: OwnerBearer security drift at ${route}`);
  }
}
const sessionRoutes = new Set([
  "GET /api/v1/session",
  "POST /api/v1/auth/logout",
  "POST /api/v1/blogs",
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
  const usesSessionCookie = Boolean(
    operation.security?.some((requirement) => Object.hasOwn(requirement, "SessionCookie")),
  );
  if (usesSessionCookie !== sessionRoutes.has(route)) {
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
