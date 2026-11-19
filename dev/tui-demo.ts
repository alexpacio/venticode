// Serve only the compiled browser demo and its same-origin synthetic API.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { resolve, extname, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { createMock } from "./mock.ts";
const port = Number(process.env.VENTICODE_DEMO_PORT || 4287);
const origin = "http://127.0.0.1:" + port;
const root = fileURLToPath(new URL("../web/dist/", import.meta.url));
const api = createMock({ origin, portalOrigin: origin });
const mime: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".woff2": "font/woff2",
  ".svg": "image/svg+xml",
  ".txt": "text/plain; charset=utf-8",
  ".css": "text/css",
  ".wasm": "application/wasm",
};
createServer(async (req, res) => {
  if (req.headers.host !== new URL(origin).host) {
    res.writeHead(403).end();
    return;
  }
  const path = new URL(req.url || "/", origin).pathname;
  if (path.startsWith("/api/") || path === "/__dev/login") {
    await api(req, res);
    return;
  }
  const file = resolve(root, "." + (extname(path) ? path : "/index.html"));
  if (!file.startsWith(resolve(root) + sep) || !mime[extname(file)]) {
    res.writeHead(404).end();
    return;
  }
  try {
    const bytes = await readFile(file);
    res
      .writeHead(200, {
        "Content-Type": mime[extname(file)],
        "Cache-Control": "no-store",
      })
      .end(bytes);
  } catch {
    res.writeHead(404).end();
  }
}).listen(port, "127.0.0.1", () => console.log(origin));
