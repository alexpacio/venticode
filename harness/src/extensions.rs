//! Project pi extensions on QuickJS: a tools-only port of pi_agent_rust's extensions_js.
//!
//! Entries live in `.venticode/extensions/` and `.pi/extensions/` in the project folder, in
//! pi's layouts: single `*.ts`/`*.js` files, or package folders whose `package.json` names
//! entries in `pi.extensions` (else `main`, else `index.ts`/`index.js`). Each entry
//! default-exports `function (pi)`. TypeScript is type-stripped with swc; relative imports
//! resolve inside the project through the sandbox; bare imports resolve to the shims in
//! `extensions/shims.rs` or fail that extension only. Every operation starts a fresh runtime,
//! loads every entry and then lists tools or runs one tool, so nothing is shared between calls.
//! shortcut: only `registerTool` does anything; events, commands, UI and providers are inert
//! stubs because venticode has no hook points for them yet.

mod shims;
mod ts;

use crate::url_read::{MAX_CONTENT_BYTES, truncate};
use anyhow::{Context as _, Result, bail};
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{CatchResultExt, Context, Ctx, Function, Module, Promise, Runtime};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::Path,
    rc::Rc,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, runtime::Handle};
use ventiharness_terminal::{files::Files, files::read_sandboxed_nofollow, process};

const DIRS: [&str; 2] = [".venticode/extensions", ".pi/extensions"];
const MAX_FILES: usize = 128;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const SOURCE_EXTENSIONS: [&str; 6] = ["ts", "tsx", "mts", "js", "mjs", "cts"];
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const LOAD_BUDGET: Duration = Duration::from_secs(5);
const CALL_BUDGET: Duration = Duration::from_secs(120);
const EXEC_DEFAULT_MS: u64 = 60_000;
const EXEC_OUTPUT_BYTES: u64 = 1024 * 1024;

const PRELUDE: &str = r#"
const __noopTop = () => {};
globalThis.__stub = function stub(name, throwing) {
  const f = function () { if (throwing) throw new Error(`${name} is not supported in venticode`); return stub(name); };
  return new Proxy(f, {
    get: (t, k) => k === 'prototype' ? t.prototype
      : k === 'then' || k === Symbol.iterator || k === Symbol.asyncIterator ? undefined
      : k === Symbol.toPrimitive || k === 'toString' ? () => ''
      : stub(`${name}.${String(k)}`, throwing),
    construct: () => stub(name, throwing),
  });
};
const __withStubs = (obj, name) => new Proxy(obj, { get: (t, k) => (k in t ? t[k] : typeof k === 'symbol' ? undefined : __stub(`${name}.${k}`)) });
globalThis.queueMicrotask ??= (f) => { Promise.resolve().then(f); };
// shortcut: timers fire on the next microtask without delay and intervals never fire; enough
// for load-time code, add a real timer queue if tools depend on waiting.
globalThis.setTimeout ??= (f, _ms, ...a) => { Promise.resolve().then(() => f(...a)); return 0; };
globalThis.setImmediate ??= (f, ...a) => setTimeout(f, 0, ...a);
globalThis.setInterval ??= () => 0;
globalThis.clearTimeout ??= __noopTop; globalThis.clearInterval ??= __noopTop; globalThis.clearImmediate ??= __noopTop;
globalThis.TextEncoder ??= class { encode(s) { const b = unescape(encodeURIComponent(String(s ?? ''))); return Uint8Array.from(b, (c) => c.charCodeAt(0)); } };
globalThis.TextDecoder ??= class { decode(u) { if (!u) return ''; let s = ''; for (const c of new Uint8Array(u.buffer ?? u)) s += String.fromCharCode(c); try { return decodeURIComponent(escape(s)); } catch { return s; } } };
// shortcut: a utf8/base64/hex Buffer subset over Uint8Array; enough for encoding helpers.
globalThis.Buffer ??= (() => {
  const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  const wrap = (u) => Object.setPrototypeOf(u, Buf.prototype);
  class Buf extends Uint8Array {
    static from(v, enc) {
      if (typeof v !== 'string') return wrap(Uint8Array.from(v?.buffer ? new Uint8Array(v.buffer) : v ?? []));
      if (enc === 'base64' || enc === 'base64url') {
        const t = v.replace(/-/g, '+').replace(/_/g, '/').replace(/[^A-Za-z0-9+/]/g, '');
        const out = []; let bits = 0, acc = 0;
        for (const ch of t) { acc = (acc << 6) | B64.indexOf(ch); bits += 6; if (bits >= 8) { bits -= 8; out.push((acc >> bits) & 255); } }
        return wrap(Uint8Array.from(out));
      }
      if (enc === 'hex') return wrap(Uint8Array.from(v.match(/../g) ?? [], (h) => parseInt(h, 16)));
      return wrap(new TextEncoder().encode(v));
    }
    static alloc(n) { return wrap(new Uint8Array(n)); }
    static isBuffer(v) { return v instanceof Buf; }
    static byteLength(s) { return Buf.from(String(s)).length; }
    static concat(list) { const out = new Uint8Array(list.reduce((n, b) => n + b.length, 0)); let o = 0; for (const b of list) { out.set(b, o); o += b.length; } return wrap(out); }
    toString(enc) {
      if (enc === 'base64') { let s = ''; for (let i = 0; i < this.length; i += 3) { const n = (this[i] << 16) | ((this[i + 1] ?? 0) << 8) | (this[i + 2] ?? 0); s += B64[n >> 18 & 63] + B64[n >> 12 & 63] + (i + 1 < this.length ? B64[n >> 6 & 63] : '=') + (i + 2 < this.length ? B64[n & 63] : '='); } return s; }
      if (enc === 'hex') return [...this].map((b) => b.toString(16).padStart(2, '0')).join('');
      return new TextDecoder().decode(this);
    }
  }
  return Buf;
})();
// shortcut: URL parses absolute http(s)/file URLs and relative paths against a base only.
globalThis.URL ??= class URL {
  constructor(u, base) {
    u = String(u);
    if (base !== undefined && !/^[a-z][a-z0-9+.-]*:/i.test(u)) { const b = new URL(base); u = b.origin + (u.startsWith('/') ? u : b.pathname.replace(/[^/]*$/, '') + u); }
    const m = /^([a-z][a-z0-9+.-]*:)(?:\/\/([^/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/i.exec(u);
    if (!m) throw new TypeError(`Invalid URL: ${u}`);
    this.protocol = m[1].toLowerCase(); this.host = m[2] ?? ''; this.hostname = this.host.replace(/:\d+$/, '');
    this.port = (/:(\d+)$/.exec(this.host) ?? [])[1] ?? ''; this.pathname = m[3] || '/'; this.search = m[4] ?? ''; this.hash = m[5] ?? '';
    this.origin = this.host || this.protocol === 'file:' ? `${this.protocol}//${this.host}` : this.protocol;
    this.searchParams = new Map(this.search.slice(1).split('&').filter(Boolean).map((p) => p.split('=').map(decodeURIComponent)));
  }
  get href() { return `${this.origin}${this.pathname}${this.search}${this.hash}`; }
  toString() { return this.href; }
  toJSON() { return this.href; }
};
globalThis.structuredClone ??= (v) => JSON.parse(JSON.stringify(v));
globalThis.AbortController ??= class { constructor() { this.signal = { aborted: false, addEventListener: __noopTop, removeEventListener: __noopTop }; } abort() { this.signal.aborted = true; } };
const __out = { write: __noopTop, isTTY: false, columns: 80, rows: 24, on: __noopTop };
globalThis.process = {
  cwd: () => __host.cwd, env: {}, platform: __host.platform, arch: __host.arch, argv: [], execArgv: [], pid: 1,
  version: 'v22.0.0', versions: { node: '22.0.0' }, stdout: __out, stderr: __out,
  on: __noopTop, once: __noopTop, off: __noopTop, emitWarning: __noopTop,
  nextTick: (f, ...a) => queueMicrotask(() => f(...a)),
  exit: () => { throw new Error('process.exit is not supported in venticode'); },
};
const __fsCall = (op, p) => { const r = JSON.parse(__fsop(op, String(p).replace(/^file:\/\//, ''))); if (r.error) { const e = new Error(r.error); e.code = r.code; throw e; } return r.value; };
const __entry = (e) => ({ name: e.name, isFile: () => !e.directory, isDirectory: () => e.directory, isSymbolicLink: () => false });
const __stat = (p) => { const s = __fsCall('stat', p); return { size: s.bytes, mtimeMs: 0, mtime: new Date(0), isFile: () => !s.directory, isDirectory: () => s.directory, isSymbolicLink: () => false }; };
globalThis.__fs = {
  readFileSync: (p) => __fsCall('read', p),
  existsSync: (p) => { try { __fsCall('stat', p); return true; } catch { return false; } },
  readdirSync: (p, o) => __fsCall('list', p).map((e) => (o && o.withFileTypes ? __entry(e) : e.name)),
  statSync: __stat, lstatSync: __stat,
  accessSync: (p) => { __fsCall('stat', p); },
  realpathSync: (p) => p,
  __throwing: true,
};
const __async = (f) => (...a) => new Promise((res, rej) => { try { res(f(...a)); } catch (e) { rej(e); } });
globalThis.__fsPromises = {
  readFile: __async(__fs.readFileSync), readdir: __async(__fs.readdirSync), stat: __async(__stat), lstat: __async(__stat),
  access: __async(__fs.accessSync), realpath: __async((p) => p), __throwing: true,
};
const __tools = new Map();
const __say = (level) => (...a) => __log(level, a.map(String).join(' '));
globalThis.console = { log: __say(0), info: __say(0), debug: __say(0), warn: __say(1), error: __say(2) };
const __noop = () => {};
function __pi(ext) {
  return __withStubs({
    registerTool(spec) {
      if (!spec || typeof spec !== 'object') throw new Error('registerTool: spec must be an object');
      const name = String(spec.name || '').trim();
      if (!name) throw new Error('registerTool: spec.name is required');
      if (typeof spec.execute !== 'function') throw new Error('registerTool: spec.execute must be a function');
      const prev = __tools.get(name);
      if (prev && prev.ext !== ext) throw new Error(`registerTool: tool name collision: ${name} (${prev.ext}, ${ext})`);
      __tools.set(name, { ext, spec });
    },
    on: __noop, registerCommand: __noop, registerShortcut: __noop, registerFlag: __noop,
    registerMessageRenderer: __noop, registerProvider: __noop, getFlag: () => undefined,
    async exec(command, args, options) {
      const r = JSON.parse(__exec(String(command), JSON.stringify((args || []).map(String)), JSON.stringify(options || {})));
      if (r.error) throw new Error(r.error);
      return r;
    },
  }, 'pi');
}
async function __load(ext, init) {
  if (typeof init !== 'function') throw new Error('entry module must default-export a function');
  try { await init(__pi(ext)); }
  catch (e) { for (const [k, v] of __tools) if (v.ext === ext) __tools.delete(k); throw e; }
}
function __list() {
  return JSON.stringify([...__tools].sort(([a], [b]) => (a < b ? -1 : 1)).map(([name, { ext, spec }]) => ({
    name, description: spec.description ? String(spec.description) : '',
    parameters: spec.parameters || { type: 'object', properties: {} }, extension: ext,
  })));
}
async function __call(name, params, cwd) {
  const t = __tools.get(name);
  if (!t) return null;
  const signal = { aborted: false, addEventListener: __noop, removeEventListener: __noop };
  try {
    const ctx = __withStubs({ cwd, hasUI: false, signal }, 'ctx');
    const r = await t.spec.execute('venticode', JSON.parse(params), signal, __noop, ctx);
    if (typeof r === 'string') return JSON.stringify({ content: r, is_error: false });
    if (r === undefined || r === null) return JSON.stringify({ content: '', is_error: false });
    const parts = r && Array.isArray(r.content) ? r.content : [];
    return JSON.stringify({
      content: parts.filter((p) => p && p.type === 'text').map((p) => String(p.text ?? '')).join('\n'),
      is_error: !!(r && r.isError),
    });
  } catch (e) {
    return JSON.stringify({ content: String(e), is_error: true });
  }
}
"#;

/// `extension_list_tools` → `{"tools":[{name,description,parameters,extension}],"errors":[{extension,message}]}`.
pub async fn list(files: Arc<Files>) -> Result<Value> {
    let handle = Handle::current();
    tokio::task::spawn_blocking(move || run(&handle, files, None)).await?
}

/// `extension_call_tool` → `{"content": string, "is_error": bool}`.
pub async fn call(files: Arc<Files>, tool: String, parameters: Value) -> Result<Value> {
    let handle = Handle::current();
    tokio::task::spawn_blocking(move || run(&handle, files, Some((tool, parameters)))).await?
}

fn run(handle: &Handle, files: Arc<Files>, call: Option<(String, Value)>) -> Result<Value> {
    let (entries, mut errors) = discover(&files);
    let rt = Runtime::new()?;
    let sources = Rc::new(RefCell::new(HashMap::new()));
    rt.set_loader(
        Modules {
            files: files.clone(),
            sources: sources.clone(),
        },
        Modules {
            files: files.clone(),
            sources: sources.clone(),
        },
    );
    rt.set_memory_limit(MEMORY_LIMIT);
    rt.set_max_stack_size(1024 * 1024);
    let deadline = Rc::new(Cell::new(Instant::now() + LOAD_BUDGET));
    let timed_out = Rc::new(Cell::new(false));
    let (d, t) = (deadline.clone(), timed_out.clone());
    rt.set_interrupt_handler(Some(Box::new(move || {
        let hit = Instant::now() >= d.get();
        t.set(t.get() || hit);
        hit
    })));
    let ctx = Context::full(&rt)?;
    let cwd = files.base_path().to_string_lossy().into_owned();
    let stalled = Cell::new(false);
    let result = ctx.with(|ctx| -> Result<Value> {
        install(&ctx, handle.clone(), files.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let globals = ctx.globals();
        let load: Function = globals.get("__load").map_err(js)?;
        for rel in &entries {
            let source = match load_source(&files, rel) {
                Ok(Some(source)) => source,
                Ok(None) => {
                    errors.push(json!({"extension": rel, "message": "Extension file vanished"}));
                    continue;
                }
                Err(e) => {
                    errors.push(json!({"extension": rel, "message": e.to_string()}));
                    continue;
                }
            };
            sources.borrow_mut().insert(rel.clone(), source.clone());
            let loaded = (|| {
                let (module, done) = Module::declare(ctx.clone(), rel.as_str(), source)?.eval()?;
                done.finish::<()>()?;
                let init: rquickjs::Value = module.get("default")?;
                load.call::<_, Promise>((rel.as_str(), init))?
                    .finish::<()>()
            })()
            .catch(&ctx);
            if let Err(e) = loaded {
                stalled.set(stalled.get() || e.to_string().contains("would block"));
                let message = if timed_out.get() {
                    "Extension load timed out".into()
                } else {
                    e.to_string()
                };
                errors.push(json!({"extension": rel, "message": message}));
            }
        }
        let Some((tool, parameters)) = call else {
            let tools: String = globals
                .get::<_, Function>("__list")
                .map_err(js)?
                .call(())
                .map_err(js)?;
            return Ok(json!({"tools": serde_json::from_str::<Value>(&tools)?, "errors": errors}));
        };
        deadline.set(Instant::now() + CALL_BUDGET);
        let out = globals
            .get::<_, Function>("__call")
            .and_then(|f| {
                f.call::<_, Promise>((tool.as_str(), parameters.to_string(), cwd.as_str()))
            })
            .and_then(|p| p.finish::<Option<String>>())
            .catch(&ctx);
        if timed_out.get() {
            bail!(
                "Extension tool '{tool}' timed out after {}s",
                CALL_BUDGET.as_secs()
            );
        }
        let out = out.map_err(|e| {
            stalled.set(e.to_string().contains("would block"));
            anyhow::anyhow!("Extension tool '{tool}' failed: {e}")
        })?;
        let Some(out) = out else {
            bail!("Unknown extension tool '{tool}'");
        };
        let mut out: Value = serde_json::from_str(&out)?;
        if let Some(Value::String(content)) = out.get_mut("content") {
            truncate(content, MAX_CONTENT_BYTES);
        }
        Ok(out)
    });
    if timed_out.get() || stalled.get() {
        // shortcut: quickjs-ng's JS_FreeContext corrupts the heap when an interrupted or
        // never-settled module evaluation is freed (pi vendors a patch); leak this runtime instead.
        std::mem::forget(ctx);
        std::mem::forget(rt);
    }
    result
}

fn js(e: rquickjs::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

fn install<'js>(ctx: &Ctx<'js>, handle: Handle, files: Arc<Files>) -> rquickjs::Result<()> {
    let g = ctx.globals();
    let home = std::env::var("HOME").unwrap_or_default();
    let host = json!({"cwd": files.base_path(), "home": home, "platform": std::env::consts::OS,
        "arch": match std::env::consts::ARCH { "x86_64" => "x64", "aarch64" => "arm64", a => a }});
    ctx.eval::<(), _>(format!("globalThis.__host = {host};"))?;
    let fs_files = files.clone();
    g.set(
        "__fsop",
        Function::new(ctx.clone(), move |op: String, path: String| {
            match fs_op(&fs_files, &op, &path) {
                Ok(value) => json!({ "value": value }),
                Err(e) => json!({ "error": format!("{e}: {path}"), "code": "ENOENT" }),
            }
            .to_string()
        })?,
    )?;
    g.set(
        "__log",
        Function::new(ctx.clone(), |level: i32, message: String| match level {
            2 => tracing::error!(target: "extension", "{message}"),
            1 => tracing::warn!(target: "extension", "{message}"),
            _ => tracing::info!(target: "extension", "{message}"),
        })?,
    )?;
    g.set(
        "__exec",
        Function::new(
            ctx.clone(),
            move |command: String, args: String, options: String| {
                let result = serde_json::from_str::<Vec<String>>(&args)
                    .map_err(anyhow::Error::from)
                    .and_then(|args| {
                        let options = serde_json::from_str(&options).unwrap_or(Value::Null);
                        exec(&handle, &files, &command, &args, &options)
                    });
                result
                    .unwrap_or_else(|e| json!({"error": e.to_string()}))
                    .to_string()
            },
        )?,
    )?;
    ctx.eval::<(), _>(shims::TYPEBOX)?;
    ctx.eval::<(), _>(PRELUDE)?;
    // `require()` of a shimmed module returns the same object an import would.
    let mut table = String::from("globalThis.__shimTable = {");
    for canonical in shims::ALL {
        table.push_str(&format!(
            "{}: () => {{\n{}\nreturn __m;\n}},\n",
            Value::String((*canonical).to_owned()),
            shims::source(canonical)
        ));
    }
    table.push_str("};");
    ctx.eval::<(), _>(table)?;
    g.set(
        "__canonical",
        Function::new(ctx.clone(), |spec: String| {
            shims::canonical(&spec).map(str::to_owned)
        })?,
    )?;
    ctx.eval::<(), _>(
        "globalThis.require = (id) => { const c = __canonical(String(id)); if (!c) throw new Error(`unsupported module '${id}'`); const m = __shimTable[c](); return m.default && !('readFileSync' in m || 'join' in m) ? m.default : m; };",
    )
}

/// Project-relative path for a module or `node:fs` path; `None` outside the project.
fn project_path(base: &Path, path: &str) -> Option<String> {
    let rel = match Path::new(path).strip_prefix(base) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) if path.starts_with('/') => return None,
        Err(_) => path.to_owned(),
    };
    normalize(&rel)
}

/// Lexically normalize a `/`-separated relative path; `None` if it climbs above the root.
fn normalize(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            p => out.push(p),
        }
    }
    Some(out.join("/"))
}

fn read_text(files: &Files, rel: &str) -> Result<Option<String>> {
    if rel.is_empty() {
        return Ok(None);
    }
    match read_sandboxed_nofollow(files.base_path(), Path::new(rel), MAX_FILE_BYTES)? {
        Some(bytes) => Ok(Some(String::from_utf8(bytes)?)),
        None => Ok(None),
    }
}

/// Read a module and turn it into JS: TypeScript is stripped, JSON becomes a default export.
fn load_source(files: &Files, rel: &str) -> Result<Option<String>> {
    let Some(source) = read_text(files, rel)? else {
        return Ok(None);
    };
    let ext = rel.rsplit_once('.').map_or("", |(_, e)| e);
    let code = match ext {
        "ts" | "tsx" | "mts" | "cts" => ts::transpile(&source, rel).map_err(anyhow::Error::msg)?,
        "json" => format!(
            "export default {};",
            serde_json::from_str::<Value>(&source)?
        ),
        _ => source,
    };
    // Plugins find files next to themselves through import.meta; QuickJS leaves it empty.
    // shortcut: textual replacement; a string literal spelling `import.meta.url` changes too.
    let file = files.base_path().join(rel);
    let dir = file.parent().unwrap_or(&file);
    let quote = |v: String| Value::String(v).to_string();
    Ok(Some(
        code.replace(
            "import.meta.url",
            &quote(format!("file://{}", file.display())),
        )
        .replace("import.meta.filename", &quote(file.display().to_string()))
        .replace("import.meta.dirname", &quote(dir.display().to_string())),
    ))
}

/// Read-only `node:fs` host calls confined to the project folder.
fn fs_op(files: &Files, op: &str, path: &str) -> Result<Value> {
    let rel = project_path(files.base_path(), path).context("Path is outside the project")?;
    match op {
        "read" => Ok(json!(read_text(files, &rel)?.context("No such file")?)),
        "list" => Ok(files.list(if rel.is_empty() { "." } else { &rel })?["items"].clone()),
        "stat" => {
            if rel.is_empty() {
                return Ok(json!({"directory": true, "bytes": 0}));
            }
            let (parent, name) = rel.rsplit_once('/').unwrap_or((".", &rel));
            files.list(parent)?["items"]
                .as_array()
                .and_then(|items| items.iter().find(|i| i["name"] == name).cloned())
                .context("No such file")
        }
        _ => bail!("Unsupported fs operation"),
    }
}

/// Module resolution and loading for one runtime. Project modules are named by their
/// project-relative path; shims by `virtual:<shim>?<importer>` so each importer gets the
/// exports it names.
/// Words that cannot be `export const` names.
const RESERVED: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "let",
    "static",
    "await",
    "implements",
    "interface",
    "package",
    "private",
    "protected",
    "public",
];

struct Modules {
    files: Arc<Files>,
    sources: Rc<RefCell<HashMap<String, String>>>,
}

static IMPORTS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?:import|export)\s*(?:[\w$]+\s*,\s*)?\{([^}]*)\}\s*from\s*["']([^"']+)["']"#,
    )
    .expect("import regex")
});

/// Named imports a module takes from shim `canonical`.
fn imported_names(code: &str, canonical: &str) -> Vec<String> {
    let mut names: Vec<String> = IMPORTS
        .captures_iter(code)
        .filter(|c| shims::canonical(&c[2]) == Some(canonical))
        .flat_map(|c| {
            c[1].split(',')
                .filter_map(|n| {
                    let n = n.trim().trim_start_matches("type ").trim();
                    let n = n.split(" as ").next().unwrap_or(n).trim();
                    (!n.is_empty()
                        && n != "default"
                        && n.chars()
                            .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$'))
                    .then(|| n.to_owned())
                })
                .collect::<Vec<_>>()
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

impl Resolver for Modules {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if !name.starts_with('.') {
            return match shims::canonical(name) {
                Some(canonical) => Ok(format!("virtual:{canonical}?{base}")),
                None => Err(rquickjs::Error::new_resolving_message(
                    base,
                    name,
                    format!("unsupported module '{name}'"),
                )),
            };
        }
        let dir = base.rsplit_once('/').map_or("", |(dir, _)| dir);
        let Some(joined) = normalize(&format!("{dir}/{name}")) else {
            return Err(rquickjs::Error::new_resolving_message(
                base,
                name,
                "import escapes the project folder",
            ));
        };
        // TypeScript ESM imports `./x.js` for `./x.ts`.
        let stem = joined.strip_suffix(".js").unwrap_or(&joined);
        let candidates = [
            joined.clone(),
            format!("{stem}.ts"),
            format!("{joined}.ts"),
            format!("{joined}.js"),
            format!("{joined}.mjs"),
            format!("{joined}/index.ts"),
            format!("{joined}/index.js"),
        ];
        for candidate in candidates {
            if self.sources.borrow().contains_key(&candidate) {
                return Ok(candidate);
            }
            match load_source(&self.files, &candidate) {
                Ok(Some(code)) => {
                    self.sources.borrow_mut().insert(candidate.clone(), code);
                    return Ok(candidate);
                }
                Ok(None) => {}
                // Directories and unreadable candidates fall through to the next form.
                Err(e) if candidate == joined && !e.to_string().contains("directory") => {
                    return Err(rquickjs::Error::new_resolving_message(
                        base,
                        name,
                        e.to_string(),
                    ));
                }
                Err(_) => {}
            }
        }
        Err(rquickjs::Error::new_resolving_message(
            base,
            name,
            "module not found",
        ))
    }
}

impl Loader for Modules {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        if let Some(rest) = name.strip_prefix("virtual:") {
            let (canonical, importer) = rest.split_once('?').unwrap_or((rest, ""));
            let mut wanted = self
                .sources
                .borrow()
                .get(importer)
                .map(|code| imported_names(code, canonical))
                .unwrap_or_default();
            // `import * as fs from "node:fs"` needs every name the shim defines, not only `{ ... }` ones.
            let keys: Vec<String> = ctx.eval(format!(
                "Object.keys((() => {{\n{}\nreturn __m;\n}})())",
                shims::source(canonical)
            ))?;
            wanted.extend(keys.into_iter().filter(|k| {
                k != "default"
                    && !k.starts_with("__")
                    && !RESERVED.contains(&k.as_str())
                    && k.chars()
                        .next()
                        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
                    && k.chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
            }));
            wanted.sort();
            wanted.dedup();
            // The shim body gets its own scope so its locals cannot clash with the exports below.
            let mut code = format!(
                "const __m = (() => {{\n{}\nreturn __m;\n}})();\nconst __pick = (k) => (k in __m ? __m[k] : __stub(k, !!__m.__throwing));\n",
                shims::source(canonical)
            );
            for export in wanted {
                code.push_str(&format!("export const {export} = __pick('{export}');\n"));
            }
            code.push_str("export default (__m.default ?? __m);\n");
            return Module::declare(ctx.clone(), name, code);
        }
        let code = self
            .sources
            .borrow()
            .get(name)
            .cloned()
            .ok_or_else(|| rquickjs::Error::new_loading_message(name, "module not resolved"))?;
        Module::declare(ctx.clone(), name, code)
    }
}

/// `pi.exec`: run a binary directly (no shell) inside the project folder.
fn exec(
    handle: &Handle,
    files: &Files,
    command: &str,
    args: &[String],
    options: &Value,
) -> Result<Value> {
    let cwd = files.directory(options["cwd"].as_str().unwrap_or("."))?;
    let timeout = Duration::from_millis(
        options["timeout"]
            .as_u64()
            .unwrap_or(EXEC_DEFAULT_MS)
            .clamp(1, CALL_BUDGET.as_millis() as u64),
    );
    handle.block_on(async {
        let mut child = process::spawn(process::command(command, args, &cwd))?;
        child.stdin().take();
        let mut stdout = child.stdout().take().context("Missing stdout")?;
        let mut stderr = child.stderr().take().context("Missing stderr")?;
        let finished = tokio::time::timeout(timeout, async {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let (mut stdout, mut stderr) = (
                (&mut stdout).take(EXEC_OUTPUT_BYTES),
                (&mut stderr).take(EXEC_OUTPUT_BYTES),
            );
            let _ = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
            (out, err, child.wait().await)
        })
        .await;
        Ok(match finished {
            Ok((out, err, status)) => json!({
                "stdout": String::from_utf8_lossy(&out),
                "stderr": String::from_utf8_lossy(&err),
                "code": status?.code(),
                "killed": false,
            }),
            Err(_) => {
                let _ = child.start_kill();
                json!({"stdout": "", "stderr": "", "code": null, "killed": true})
            }
        })
    })
}

fn is_source(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        SOURCE_EXTENSIONS.contains(&ext)
            && !stem.ends_with(".d")
            && !stem.contains(".test")
            && !stem.contains(".spec")
    })
}

/// Source files directly inside `dir` (no symlinks), sorted.
fn source_files(files: &Files, dir: &str) -> Vec<String> {
    let Ok(listing) = files.list(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = listing["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["directory"] != true && i["symlink"] != true)
        .filter_map(|i| i["name"].as_str())
        .filter(|n| is_source(n))
        .map(|n| format!("{dir}/{n}"))
        .collect();
    out.sort();
    out
}

/// Entries of one package folder: `pi.extensions`, else a source `main`, else `index.ts|js`.
fn package_entries(files: &Files, pkg: &str) -> Vec<String> {
    let manifest = read_text(files, &format!("{pkg}/package.json"))
        .ok()
        .flatten()
        .and_then(|m| serde_json::from_str::<Value>(&m).ok())
        .unwrap_or_default();
    let exists = |rel: &str| {
        read_sandboxed_nofollow(files.base_path(), Path::new(rel), MAX_FILE_BYTES)
            .is_ok_and(|b| b.is_some())
    };
    if let Some(list) = manifest["pi"]["extensions"].as_array() {
        return list
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|e| normalize(&format!("{pkg}/{e}")))
            .filter(|e| e.starts_with(&format!("{pkg}/")))
            .flat_map(|e| {
                if is_source(&e) {
                    vec![e]
                } else {
                    source_files(files, &e)
                }
            })
            .collect();
    }
    if let Some(main) = manifest["main"]
        .as_str()
        .and_then(|m| normalize(&format!("{pkg}/{m}")))
        && is_source(&main)
        && exists(&main)
    {
        return vec![main];
    }
    ["index.ts", "index.js"]
        .iter()
        .map(|i| format!("{pkg}/{i}"))
        .find(|i| exists(i))
        .into_iter()
        .collect()
}

/// Entry modules through the project sandbox (no symlinks), sorted per directory.
fn discover(files: &Files) -> (Vec<String>, Vec<Value>) {
    let (mut entries, mut errors) = (Vec::new(), Vec::new());
    for dir in DIRS {
        let Ok(listing) = files.list(dir) else {
            continue;
        };
        let mut items: Vec<&Value> = listing["items"].as_array().into_iter().flatten().collect();
        items.sort_by_key(|i| i["name"].as_str().unwrap_or_default().to_owned());
        for item in items {
            let name = item["name"].as_str().unwrap_or_default();
            if item["symlink"] == true || name.starts_with('.') || name == "node_modules" {
                continue;
            }
            let rel = format!("{dir}/{name}");
            let found = if item["directory"] == true {
                package_entries(files, &rel)
            } else if is_source(name) {
                vec![rel]
            } else {
                continue;
            };
            for entry in found {
                if entries.len() == MAX_FILES {
                    errors.push(json!({"extension": entry, "message": format!("Skipped: more than {MAX_FILES} extensions")}));
                } else {
                    entries.push(entry);
                }
            }
        }
    }
    (entries, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(entries: &[(&str, &str)]) -> (tempfile::TempDir, Arc<Files>) {
        let dir = tempfile::tempdir().unwrap();
        for (path, source) in entries {
            let target = dir.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, source).unwrap();
        }
        let files = Arc::new(Files::open(dir.path()).unwrap());
        (dir, files)
    }

    const GOOD: &str = r#"
export default function (pi) {
  pi.on('session_start', () => {});
  pi.registerTool({ name: 'hello', description: 'Say hi', parameters: { type: 'object' },
    execute: (_id, params) => ({ content: [{ type: 'text', text: `hi ${params.who}` }] }) });
  pi.registerTool({ name: 'later', description: '', execute: async () => { await null; return 'done'; } });
  pi.registerTool({ name: 'oops', execute: async () => ({ content: [{ type: 'text', text: 'bad' }], isError: true }) });
  pi.registerTool({ name: 'spin', execute: () => { for (;;) {} } });
  pi.registerTool({ name: 'echo', execute: async (_id, p, _s, _u, ctx) => {
    const r = await pi.exec('sh', ['-c', 'pwd; echo out; echo err >&2; exit 3']);
    return `${r.code}|${r.stdout.trim().split('\n')[1]}|${r.stderr.trim()}|${r.stdout.startsWith(ctx.cwd)}`;
  } });
}
"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn lists_and_calls_extension_tools() {
        let (_dir, files) = project(&[
            (".venticode/extensions/a.js", GOOD),
            (
                ".venticode/extensions/broken.js",
                "export default function (pi) { throw new Error('boom') }",
            ),
            (
                ".pi/extensions/clash.js",
                "export default (pi) => pi.registerTool({ name: 'hello', execute: () => '' })",
            ),
            (".pi/extensions/notes.md", "ignored"),
        ]);
        let listed = list(files.clone()).await.unwrap();
        let names: Vec<_> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["echo", "hello", "later", "oops", "spin"]);
        assert_eq!(
            listed["tools"][1]["extension"],
            ".venticode/extensions/a.js"
        );
        assert_eq!(listed["tools"][1]["description"], "Say hi");
        let errors = listed["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0]["message"].as_str().unwrap().contains("boom"));
        assert!(
            errors[1]["message"]
                .as_str()
                .unwrap()
                .contains("collision: hello (.venticode/extensions/a.js, .pi/extensions/clash.js)")
        );

        let hello = call(files.clone(), "hello".into(), json!({"who":"you"}))
            .await
            .unwrap();
        assert_eq!(hello, json!({"content":"hi you","is_error":false}));
        let later = call(files.clone(), "later".into(), json!({}))
            .await
            .unwrap();
        assert_eq!(later, json!({"content":"done","is_error":false}));
        let oops = call(files.clone(), "oops".into(), json!({})).await.unwrap();
        assert_eq!(oops, json!({"content":"bad","is_error":true}));
        #[cfg(unix)]
        {
            let echo = call(files.clone(), "echo".into(), json!({})).await.unwrap();
            assert_eq!(echo["content"], "3|out|err|true");
        }
        assert!(
            call(files.clone(), "nope".into(), json!({}))
                .await
                .unwrap_err()
                .to_string()
                .contains("Unknown")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loads_typescript_packages_with_pi_shims() {
        let (_dir, files) = project(&[
            (
                ".pi/extensions/todo/package.json",
                r#"{"name":"todo","pi":{"extensions":["./src/main.ts"]}}"#,
            ),
            (
                ".pi/extensions/todo/src/main.ts",
                r#"import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { StringEnum } from "@mariozechner/pi-ai";
import { label } from "./util";
interface Args { item: string; mode: "add" | "done" }
export default function (pi: ExtensionAPI): void {
  pi.registerTool({
    name: "todo",
    description: "Track todos",
    parameters: Type.Object({ item: Type.String(), mode: StringEnum(["add", "done"] as const) }),
    async execute(_id: string, p: Args) { return { content: [{ type: "text", text: label(p) }] }; },
  });
}"#,
            ),
            (
                ".pi/extensions/todo/src/util.ts",
                "export const label = (p: { item: string; mode: string }): string => `${p.mode}:${p.item}`;",
            ),
            (
                ".pi/extensions/needs-net.ts",
                "import { request } from 'undici';
export default (pi) => pi.registerTool({ name: 'net', execute: () => request('x') });",
            ),
        ]);
        let listed = list(files.clone()).await.unwrap();
        assert_eq!(listed["tools"].as_array().unwrap().len(), 1, "{listed}");
        let todo = &listed["tools"][0];
        assert_eq!(todo["extension"], ".pi/extensions/todo/src/main.ts");
        assert_eq!(
            todo["parameters"]["properties"]["mode"]["enum"],
            json!(["add", "done"])
        );
        assert_eq!(todo["parameters"]["required"], json!(["item", "mode"]));
        let errors = listed["errors"].as_array().unwrap();
        assert!(
            errors[0]["message"].as_str().unwrap().contains("undici"),
            "{errors:?}"
        );
        let out = call(files, "todo".into(), json!({"item":"ship","mode":"add"}))
            .await
            .unwrap();
        assert_eq!(out["content"], "add:ship");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "waits for the 120s call budget"]
    async fn infinite_tool_loop_times_out() {
        let (_dir, files) = project(&[(".venticode/extensions/a.js", GOOD)]);
        let err = call(files, "spin".into(), json!({})).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn infinite_load_loop_times_out_without_hiding_others() {
        let (_dir, files) = project(&[
            (".venticode/extensions/a.js", GOOD),
            (".venticode/extensions/b.js", "for (;;) {}"),
        ]);
        let listed = list(files).await.unwrap();
        assert_eq!(listed["tools"].as_array().unwrap().len(), 5);
        assert_eq!(listed["errors"][0]["message"], "Extension load timed out");
    }
}
