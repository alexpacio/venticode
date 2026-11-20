//! Virtual modules for bare imports, modelled on pi_agent_rust's shims.
//!
//! Each shim defines `const __m = {...}`; the loader appends one `export const`
//! per name the importing module asks for, so names a shim lacks become inert
//! stubs (`__stub`, from the prelude) instead of link errors.
//! shortcut: stubs are inert or throw on call; implement a name when a popular
//! extension needs it to work, not just to load.

/// Canonical shim for a bare specifier, or `None` when unsupported.
pub fn canonical(spec: &str) -> Option<&'static str> {
    let spec = spec.strip_prefix("node:").unwrap_or(spec);
    let pkg = |name: &str| spec == name || spec.starts_with(&format!("{name}/"));
    Some(match spec {
        "typebox/compile" | "typebox/compiler" | "@sinclair/typebox/compiler" => "typebox-compile",
        "typebox/value" | "@sinclair/typebox/value" => "typebox-value",
        "path" | "path/posix" => "path",
        "os" => "os",
        "fs" => "fs",
        "fs/promises" => "fs-promises",
        "process" => "process",
        "child_process" => "child_process",
        "url" => "url",
        "crypto" => "crypto",
        "util" => "util",
        "events" => "events",
        "module" => "module",
        "assert" | "assert/strict" => "assert",
        // Loadable, but any use throws: plugins often import these for one optional feature.
        "readline" | "readline/promises" | "net" | "tls" | "http" | "https" | "http2" | "zlib"
        | "stream" | "stream/promises" | "tty" | "worker_threads" | "dns" | "vm" | "v8"
        | "perf_hooks" | "string_decoder" | "timers" | "timers/promises" | "buffer"
        | "async_hooks" | "cluster" | "dgram" | "inspector" => "node-unsupported",
        _ if pkg("typebox") || pkg("@sinclair/typebox") => "typebox",
        _ if pkg("@mariozechner/pi-ai")
            || pkg("@earendil-works/pi-ai")
            || pkg("@mariozechner/pi-agent-core")
            || pkg("@earendil-works/pi-agent-core") =>
        {
            "pi-ai"
        }
        _ if pkg("@mariozechner/pi-coding-agent") || pkg("@earendil-works/pi-coding-agent") => {
            "pi-coding-agent"
        }
        _ if pkg("@mariozechner/pi-tui") || pkg("@earendil-works/pi-tui") => "pi-tui",
        _ => return None,
    })
}

/// Every canonical shim name, for `require()`.
pub const ALL: &[&str] = &[
    "typebox",
    "typebox-compile",
    "typebox-value",
    "pi-ai",
    "pi-coding-agent",
    "pi-tui",
    "path",
    "os",
    "fs",
    "fs-promises",
    "process",
    "child_process",
    "url",
    "crypto",
    "util",
    "events",
    "module",
    "assert",
    "node-unsupported",
];

/// Shim body defining `__m`.
pub fn source(canonical: &str) -> &'static str {
    match canonical {
        "typebox" => "const __m = globalThis.__typebox;",
        "typebox-compile" => {
            "const __m = { TypeCompiler: { Compile: () => ({ Check: () => true, Errors: () => [] }) }, Compile: () => ({ Check: () => true, Errors: () => [] }) };"
        }
        "typebox-value" => {
            "const __m = { Value: new Proxy({ Check: () => true, Errors: () => [], Default: (_s, v) => v, Clean: (_s, v) => v, Convert: (_s, v) => v, Cast: (_s, v) => v, Parse: (_s, v) => v }, { get: (t, k) => (k in t ? t[k] : (_s, v) => v) }) };"
        }
        "pi-ai" => PI_AI,
        "pi-coding-agent" => PI_CODING_AGENT,
        "pi-tui" => {
            "const __m = { visibleWidth: (s) => String(s ?? '').length, truncateToWidth: (s, w) => String(s ?? '').slice(0, w), wrapTextWithAnsi: (s) => String(s ?? '').split('\\n') };"
        }
        "path" => PATH,
        "os" => {
            "const __m = { homedir: () => __host.home, tmpdir: () => '/tmp', platform: () => __host.platform, arch: () => __host.arch, EOL: '\\n', hostname: () => 'localhost', cpus: () => [], type: () => 'Linux', release: () => '', userInfo: () => ({ username: 'user', homedir: __host.home }) };"
        }
        "fs" => FS,
        "fs-promises" => "const __m = globalThis.__fsPromises;",
        "process" => {
            "const __m = Object.assign({}, globalThis.process, { default: globalThis.process });"
        }
        "child_process" => CHILD_PROCESS,
        "url" => {
            "const __m = { fileURLToPath: (u) => String(u).replace(/^file:\\/\\//, ''), pathToFileURL: (p) => ({ href: 'file://' + p, toString() { return this.href; } }), URL: globalThis.URL };"
        }
        "crypto" => {
            "const __m = { randomUUID: () => 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (c) => { const r = (Math.random() * 16) | 0; return (c === 'x' ? r : (r & 3) | 8).toString(16); }) };"
        }
        "util" => {
            "const __m = { promisify: (f) => (...a) => new Promise((res, rej) => f(...a, (e, v) => (e ? rej(e) : res(v)))), inspect: (v) => { try { return JSON.stringify(v); } catch { return String(v); } }, format: (...a) => a.map(String).join(' '), isDeepStrictEqual: (a, b) => JSON.stringify(a) === JSON.stringify(b) };"
        }
        "events" => {
            "class EventEmitter { constructor() { this.__h = {}; } on(e, f) { (this.__h[e] ||= []).push(f); return this; } addListener(e, f) { return this.on(e, f); } once(e, f) { const g = (...a) => { this.off(e, g); f(...a); }; return this.on(e, g); } off(e, f) { this.__h[e] = (this.__h[e] || []).filter((x) => x !== f); return this; } removeListener(e, f) { return this.off(e, f); } removeAllListeners(e) { if (e) delete this.__h[e]; else this.__h = {}; return this; } emit(e, ...a) { const l = this.__h[e] || []; l.slice().forEach((f) => f(...a)); return l.length > 0; } listenerCount(e) { return (this.__h[e] || []).length; } setMaxListeners() { return this; } }\nconst __m = { EventEmitter, default: EventEmitter };"
        }
        "assert" => {
            "const ok = (v, m) => { if (!v) throw new Error(m || 'Assertion failed'); };\nconst equal = (a, b, m) => ok(a === b, m || `${a} !== ${b}`);\nconst deepEqual = (a, b, m) => ok(JSON.stringify(a) === JSON.stringify(b), m || 'Values are not deeply equal');\nconst __m = Object.assign(ok, { ok, equal, strictEqual: equal, deepEqual, deepStrictEqual: deepEqual, notEqual: (a, b, m) => ok(a !== b, m), fail: (m) => ok(false, m), default: ok });"
        }
        "node-unsupported" => {
            "const __m = { __throwing: true, Buffer: globalThis.Buffer, default: { __throwing: true } };"
        }
        "module" => {
            "const __m = { createRequire: () => (id) => { throw new Error(`require('${id}') is not supported`); } };"
        }
        _ => "const __m = {};",
    }
}

/// Script run in the prelude: defines `globalThis.__typebox`.
pub const TYPEBOX: &str = r#"
(() => {
const OPT = Symbol.for('TypeBox.Optional');
const o = (schema, opts) => Object.assign(schema, opts || {});
const required = (props) => Object.keys(props).filter((k) => !(props[k] && props[k][OPT]));
const object = (properties, opts) => {
  const out = o({ type: 'object', properties: properties || {} }, opts);
  const req = required(properties || {});
  if (req.length) out.required = req;
  return out;
};
const values = (e) => Object.keys(e).filter((k) => Number.isNaN(Number(k))).map((k) => e[k]);
const T = {
  Object: object,
  String: (opts) => o({ type: 'string' }, opts),
  Number: (opts) => o({ type: 'number' }, opts),
  Integer: (opts) => o({ type: 'integer' }, opts),
  Boolean: (opts) => o({ type: 'boolean' }, opts),
  Null: (opts) => o({ type: 'null' }, opts),
  Any: (opts) => o({}, opts),
  Unknown: (opts) => o({}, opts),
  Array: (items, opts) => o({ type: 'array', items }, opts),
  Optional: (s) => { const c = { ...s }; Object.defineProperty(c, OPT, { value: true }); return c; },
  Readonly: (s) => s,
  Union: (anyOf, opts) => o({ anyOf }, opts),
  Intersect: (allOf, opts) => o({ allOf }, opts),
  Literal: (v, opts) => o({ const: v, type: v === null ? 'null' : typeof v }, opts),
  Enum: (e, opts) => o({ enum: values(e) }, opts),
  Record: (_k, v, opts) => o({ type: 'object', additionalProperties: v }, opts),
  Tuple: (items, opts) => o({ type: 'array', items, minItems: items.length, maxItems: items.length }, opts),
  Partial: (s) => { const c = { ...s }; delete c.required; return c; },
  Required: (s) => ({ ...s, required: Object.keys(s.properties || {}) }),
  Pick: (s, keys) => object(Object.fromEntries(keys.map((k) => [k, s.properties[k]]))),
  Omit: (s, keys) => object(Object.fromEntries(Object.entries(s.properties || {}).filter(([k]) => !keys.includes(k)))),
  Unsafe: (s) => s,
  Date: (opts) => o({ type: 'string', format: 'date-time' }, opts),
  Ref: (s) => (typeof s === 'string' ? { $ref: s } : s),
};
const Type = new Proxy(T, { get: (t, k) => (k in t ? t[k] : (...a) => o({}, a.find((x) => x && typeof x === 'object' && !Array.isArray(x)))) });
globalThis.__typebox = { Type, Kind: Symbol.for('TypeBox.Kind'), OptionalKind: OPT, FormatRegistry: { Set() {}, Has: () => false }, default: Type };
})();
"#;

const PI_AI: &str = r#"
const StringEnum = (values, opts) => Object.assign({ type: 'string', enum: [...values] }, opts || {});
const __m = { Type: globalThis.__typebox.Type, StringEnum };
"#;

const PI_CODING_AGENT: &str = r#"
const DEFAULT_MAX_LINES = 2000;
const DEFAULT_MAX_BYTES = 50 * 1024;
const bytes = (s) => { let n = 0; for (const ch of String(s)) { const c = ch.codePointAt(0); n += c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4; } return n; };
const formatSize = (b) => { b = Number(b ?? 0); return b < 1024 ? `${b}B` : b < 1048576 ? `${(b / 1024).toFixed(1)}KB` : `${(b / 1048576).toFixed(1)}MB`; };
function cut(text, opts, fromEnd) {
  const raw = String(text ?? '');
  const maxLines = Number(opts?.maxLines ?? DEFAULT_MAX_LINES), maxBytes = Number(opts?.maxBytes ?? DEFAULT_MAX_BYTES);
  const lines = raw.split('\n');
  const seq = fromEnd ? [...lines].reverse() : lines;
  const out = []; let size = 0; let by = null;
  for (const line of seq) {
    if (out.length >= maxLines) { by = 'lines'; break; }
    const add = bytes(line) + (out.length ? 1 : 0);
    if (size + add > maxBytes) { by = 'bytes'; break; }
    out.push(line); size += add;
  }
  if (fromEnd) out.reverse();
  const content = out.join('\n');
  return { content, truncated: by != null, truncatedBy: by, totalLines: lines.length, totalBytes: bytes(raw), outputLines: out.length, outputBytes: bytes(content), lastLinePartial: false, firstLineExceedsLimit: false, maxLines, maxBytes };
}
const parseFrontmatter = (text) => {
  const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?/.exec(String(text ?? ''));
  if (!m) return { frontmatter: {}, body: String(text ?? '') };
  const frontmatter = {};
  for (const line of m[1].split('\n')) { const i = line.indexOf(':'); if (i > 0) frontmatter[line.slice(0, i).trim()] = line.slice(i + 1).trim(); }
  return { frontmatter, body: String(text).slice(m[0].length) };
};
const __m = {
  VERSION: '0.0.0', DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES, formatSize,
  truncateHead: (t, o) => cut(t, o, false), truncateTail: (t, o) => cut(t, o, true),
  truncateLine: (line, max = 500) => { const s = String(line ?? ''); return s.length > max ? { text: s.slice(0, max) + '... [truncated]', wasTruncated: true } : { text: s, wasTruncated: false }; },
  parseFrontmatter, stripFrontmatter: (t) => parseFrontmatter(t).body,
  isToolCallEventType: (name, event) => event?.toolName === name,
  getAgentDir: () => __host.home + '/.pi/agent',
  defineTool: (tool) => tool,
  Type: globalThis.__typebox.Type,
};
"#;

const PATH: &str = r#"
const normalize = (p) => {
  const abs = p.startsWith('/');
  const out = [];
  for (const s of p.split('/')) {
    if (!s || s === '.') continue;
    if (s === '..') { if (out.length && out[out.length - 1] !== '..') out.pop(); else if (!abs) out.push('..'); }
    else out.push(s);
  }
  const r = (abs ? '/' : '') + out.join('/');
  return r || (abs ? '/' : '.');
};
const join = (...a) => normalize(a.filter((x) => x !== '').join('/') || '.');
const resolve = (...a) => { let r = ''; for (const p of a) r = String(p).startsWith('/') ? String(p) : r ? `${r}/${p}` : String(p); return normalize(r.startsWith('/') ? r : `${globalThis.process.cwd()}/${r}`); };
const dirname = (p) => { const n = normalize(p); const i = n.lastIndexOf('/'); return i < 0 ? '.' : i === 0 ? '/' : n.slice(0, i); };
const basename = (p, ext) => { const b = normalize(p).split('/').pop(); return ext && b.endsWith(ext) ? b.slice(0, -ext.length) : b; };
const extname = (p) => { const b = basename(p); const i = b.lastIndexOf('.'); return i > 0 ? b.slice(i) : ''; };
const relative = (from, to) => { const f = resolve(from).split('/').filter(Boolean), t = resolve(to).split('/').filter(Boolean); let i = 0; while (i < f.length && f[i] === t[i]) i++; return [...f.slice(i).map(() => '..'), ...t.slice(i)].join('/'); };
const parse = (p) => { const base = basename(p), ext = extname(p); return { root: p.startsWith('/') ? '/' : '', dir: dirname(p), base, ext, name: ext ? base.slice(0, -ext.length) : base }; };
const path = { sep: '/', delimiter: ':', normalize, join, resolve, dirname, basename, extname, relative, parse, isAbsolute: (p) => String(p).startsWith('/'), format: (o) => `${o.dir ? o.dir + '/' : ''}${o.base || (o.name || '') + (o.ext || '')}`, toNamespacedPath: (p) => p };
path.posix = path;
const __m = { ...path, default: path };
"#;

const FS: &str = r#"
const fs = globalThis.__fs;
const __m = { ...fs, promises: globalThis.__fsPromises, constants: { F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1 }, default: fs };
"#;

const CHILD_PROCESS: &str = r#"
const run = (cmd, args, opts) => {
  const r = JSON.parse(__exec(String(cmd), JSON.stringify((args || []).map(String)), JSON.stringify(opts || {})));
  if (r.error) throw new Error(r.error);
  return r;
};
const fail = (r, cmd) => { const e = new Error(`Command failed: ${cmd}\n${r.stderr}`); e.status = r.code; e.stdout = r.stdout; e.stderr = r.stderr; return e; };
const execFileSync = (cmd, args, opts) => { const r = run(cmd, args, opts); if (r.code !== 0) throw fail(r, cmd); return r.stdout; };
const execSync = (cmd, opts) => execFileSync('sh', ['-c', String(cmd)], opts);
const spawnSync = (cmd, args, opts) => { const r = run(cmd, args, opts); return { status: r.code, stdout: r.stdout, stderr: r.stderr, signal: r.killed ? 'SIGKILL' : null }; };
const execFile = (cmd, args, opts, cb) => { cb = [args, opts, cb].find((f) => typeof f === 'function'); Promise.resolve().then(() => { const r = run(cmd, Array.isArray(args) ? args : [], typeof opts === 'object' ? opts : {}); cb && cb(r.code === 0 ? null : fail(r, cmd), r.stdout, r.stderr); }); };
const exec = (cmd, opts, cb) => execFile('sh', ['-c', String(cmd)], opts, cb);
const __m = { execFileSync, execSync, spawnSync, execFile, exec };
"#;

#[cfg(test)]
mod tests {
    #[test]
    fn aliases_resolve_to_shims() {
        for (spec, want) in [
            ("@earendil-works/pi-coding-agent", Some("pi-coding-agent")),
            ("@mariozechner/pi-ai/compat", Some("pi-ai")),
            ("node:fs/promises", Some("fs-promises")),
            ("typebox/value", Some("typebox-value")),
            ("@sinclair/typebox", Some("typebox")),
            ("zod", None),
            ("node:net", Some("node-unsupported")),
            ("left-pad", None),
        ] {
            assert_eq!(super::canonical(spec), want, "{spec}");
        }
    }
}
