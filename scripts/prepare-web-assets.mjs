import { cp, mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

// npm executes workspace lifecycle scripts with the workspace as cwd. Resolve
// from this script instead so the hoisted dependency and public directory are
// found identically from the root, a workspace, CI, and a container build.
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const source = path.join(root, "node_modules", "katex", "dist");
const destination = path.join(root, "apps", "web", "public", "vendor", "katex");

await mkdir(destination, { recursive: true });
await cp(path.join(source, "katex.min.js"), path.join(destination, "katex.min.js"));
await cp(path.join(source, "katex.min.css"), path.join(destination, "katex.min.css"));
await cp(path.join(source, "fonts"), path.join(destination, "fonts"), {
  recursive: true,
  force: true,
});

process.stdout.write("prepared self-hosted KaTeX assets\n");
