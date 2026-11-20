// Kitesurf's upstream Bazel dist includes the options needed by the local workerd launcher.
import { readFile, readdir } from "node:fs/promises";
import { join, relative } from "node:path";
import { compileModuleRules, convertV4MiniflareOptions, Miniflare, testRegExps } from "miniflare";

const dist = "/app/engine";
const rules = compileModuleRules([
  { type: "CompiledWasm", include: ["**/*.wasm"] },
  { type: "Text", include: ["**/*.glue.js", "*.css", "**/*.css", "**/*.dat"] },
  { type: "Data", include: ["**/*.woff2", "**/*.bin"] },
]);
const worker = JSON.parse(await readFile(join(dist, "engine-mf.json"), "utf8"));
const modules = [{ type: "ESModule", path: join(dist, "index.js") }];
for (const entry of await readdir(dist, { recursive: true, withFileTypes: true })) {
  if (!entry.isFile()) continue;
  const path = join(entry.parentPath, entry.name);
  const match = rules.find(rule => testRegExps(rule.include, relative(dist, path)));
  if (match) modules.push({ type: match.type, path });
}

const engine = new Miniflare(convertV4MiniflareOptions({
  host: "0.0.0.0",
  port: 8787,
  cf: false,
  workers: [{
    ...worker,
    name: "kitesurf-engine",
    modulesRoot: dist,
    modules,
    assets: { ...worker.assets, directory: "/app/assets" },
    bindings: {
      ...worker.bindings,
      // The pod network policy limits access to explicitly provisioned fixtures.
      KITESURF_ALLOW_PRIVATE_NETWORK: "1",
      KITESURF_OTLP_ENDPOINT: "",
      KITESURF_INSPECTOR_BASE_URL: "",
      KITESURF_INSPECTOR_FALLBACK_URL: "",
      WEB_BOT_AUTH_SIGNATURE_AGENT: "",
    },
  }],
}));
await engine.ready;
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => void engine.dispose());
}
