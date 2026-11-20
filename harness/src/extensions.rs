//! Project JS extensions on QuickJS: a tools-only port of pi_agent_rust's extensions_js.
//!
//! Entries are `.venticode/extensions/*.js` and `.pi/extensions/*.js` in the project folder,
//! each default-exporting `function (pi)` like pi-mono extensions. Every operation starts a
//! fresh runtime, loads every entry and then lists tools or runs one tool, so nothing is
//! shared between calls.
//! shortcut: plain JS modules only (no TypeScript, no `import` of packages); add a transpiler
//! and module loader when extensions need them.

use crate::url_read::{MAX_CONTENT_BYTES, truncate};
use anyhow::{Context as _, Result, bail};
use rquickjs::{CatchResultExt, Context, Ctx, Function, Module, Promise, Runtime};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    path::Path,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, runtime::Handle};
use ventiharness_terminal::{files::Files, files::read_sandboxed_nofollow, process};

const DIRS: [&str; 2] = [".venticode/extensions", ".pi/extensions"];
const MAX_FILES: usize = 32;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const LOAD_BUDGET: Duration = Duration::from_secs(5);
const CALL_BUDGET: Duration = Duration::from_secs(120);
const EXEC_DEFAULT_MS: u64 = 60_000;
const EXEC_OUTPUT_BYTES: u64 = 1024 * 1024;

const PRELUDE: &str = r#"
const __tools = new Map();
const __say = (level) => (...a) => __log(level, a.map(String).join(' '));
globalThis.console = { log: __say(0), info: __say(0), debug: __say(0), warn: __say(1), error: __say(2) };
const __noop = () => {};
function __pi(ext) {
  return {
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
    async exec(command, args, options) {
      const r = JSON.parse(__exec(String(command), JSON.stringify((args || []).map(String)), JSON.stringify(options || {})));
      if (r.error) throw new Error(r.error);
      return r;
    },
  };
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
    const r = await t.spec.execute('venticode', JSON.parse(params), signal, __noop, { cwd, hasUI: false });
    if (typeof r === 'string') return JSON.stringify({ content: r, is_error: false });
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
    let (sources, mut errors) = discover(&files);
    let rt = Runtime::new()?;
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
        for (rel, source) in &sources {
            let loaded = (|| {
                let (module, done) =
                    Module::declare(ctx.clone(), rel.as_str(), source.as_str())?.eval()?;
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
    ctx.eval::<(), _>(PRELUDE)
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

/// Read entry files through the project sandbox (no symlinks), sorted per directory.
fn discover(files: &Files) -> (Vec<(String, String)>, Vec<Value>) {
    let (mut sources, mut errors) = (Vec::new(), Vec::new());
    for dir in DIRS {
        let Ok(listing) = files.list(dir) else {
            continue;
        };
        for item in listing["items"].as_array().into_iter().flatten() {
            let name = item["name"].as_str().unwrap_or_default();
            if item["directory"] == true || item["symlink"] == true || !name.ends_with(".js") {
                continue;
            }
            let rel = format!("{dir}/{name}");
            if sources.len() == MAX_FILES {
                errors.push(json!({"extension": rel, "message": format!("Skipped: more than {MAX_FILES} extensions")}));
                continue;
            }
            match read_sandboxed_nofollow(files.base_path(), Path::new(&rel), MAX_FILE_BYTES)
                .and_then(|bytes| {
                    Ok(String::from_utf8(
                        bytes.context("Extension file vanished")?,
                    )?)
                }) {
                Ok(source) => sources.push((rel, source)),
                Err(e) => errors.push(json!({"extension": rel, "message": e.to_string()})),
            }
        }
    }
    (sources, errors)
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
