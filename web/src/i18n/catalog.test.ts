import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const root = new URL("..", import.meta.url).pathname;
const sources = (dir: string): string[] => readdirSync(dir, { withFileTypes: true })
  .flatMap((entry) => entry.isDirectory() ? sources(join(dir, entry.name)) : /\.tsx?$/.test(entry.name) && !entry.name.endsWith(".test.ts") ? [join(dir, entry.name)] : []);

test("every Italian message id has an English translation and none is stale", () => {
  const en: Record<string, string> = JSON.parse(readFileSync(join(root, "i18n/en.json"), "utf8"));
  const used = new Set(sources(root).flatMap((file) => [...readFileSync(file, "utf8").matchAll(/\bt\("((?:[^"\\]|\\.)*)"/g)].map((m) => JSON.parse(`"${m[1]}"`))));
  assert.deepEqual([...used].filter((id) => !(id in en)), []);
  assert.deepEqual(Object.keys(en).filter((id) => !used.has(id)), []);
  for (const [id, text] of Object.entries(en)) assert.equal((text.match(/\{\d+\}/g) || []).sort().join(), (id.match(/\{\d+\}/g) || []).sort().join(), id);
});
