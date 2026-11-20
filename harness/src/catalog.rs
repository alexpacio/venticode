//! Showcase installs: list and install pi skills and plugins (extensions)
//! from a public GitHub repository, the way `pi install git:...` and
//! `npx skills add owner/repo` do, without git or node on the host.
//!
//! One `codeload.github.com` tarball per `repo@ref` is downloaded, kept in
//! memory for a few minutes, and scanned:
//! - skill: a directory holding `SKILL.md` with a description;
//! - plugin: a pi package (`package.json` with `pi.extensions`, or a `main`
//!   entry that imports pi), a directory with `index.ts|js` inside an
//!   `extensions/` folder, or a single `.ts|.js` file there that imports pi.
//!
//! Skills install into the project's `.pi/skills/<name>` or the user's
//! `~/.pi/agent/skills/<name>`; plugins only into the project's
//! `.pi/extensions/` (the extension runtime loads project plugins only).
//! Installing replaces an earlier install of the same name.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use ventiharness_terminal::files::Files;

const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_UNPACKED_BYTES: usize = 160 * 1024 * 1024;
// shortcut: larger files (demo videos, GIFs) are dropped to fit the harness write limit;
// stream big files if a skill ever needs one.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_INSTALL_FILES: usize = 2_000;
const CACHE_TTL: Duration = Duration::from_secs(600);
const CACHE_ENTRIES: usize = 3;

/// Repository files by path relative to the repository root.
type Tree = BTreeMap<String, Vec<u8>>;

type Cache = Mutex<HashMap<String, (Instant, Arc<Tree>)>>;

fn cache() -> &'static Cache {
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |s: Option<&str>, extra: &str| {
        s.is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 100
                && s != "."
                && s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || extra.contains(c))
        })
    };
    ok(parts.next(), "") && ok(parts.next(), "._") && parts.next().is_none()
}

fn valid_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 100
        && !r.contains("..")
        && !r.starts_with('/')
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
}

async fn tree(repo: &str, git_ref: &str) -> Result<Arc<Tree>> {
    ensure!(
        valid_repo(repo),
        "Invalid repository '{repo}' (expected owner/name)"
    );
    ensure!(valid_ref(git_ref), "Invalid git ref '{git_ref}'");
    let key = format!("{repo}@{git_ref}");
    {
        let mut cache = cache().lock().unwrap();
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if let Some((_, tree)) = cache.get(&key) {
            return Ok(tree.clone());
        }
    }
    // Fixed public host: the repository and ref are validated path segments only.
    let url = format!("https://codeload.github.com/{repo}/tar.gz/{git_ref}");
    let client = reqwest_mcp::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut response = client.get(&url).send().await?;
    ensure!(
        response.status().is_success(),
        "Download of {repo}@{git_ref} failed with HTTP {}",
        response.status().as_u16()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= MAX_DOWNLOAD_BYTES,
            "Repository archive is larger than {} MiB",
            MAX_DOWNLOAD_BYTES >> 20
        );
        bytes.extend_from_slice(&chunk);
    }
    let tree = Arc::new(tokio::task::spawn_blocking(move || unpack(&bytes)).await??);
    let mut cache = cache().lock().unwrap();
    if cache.len() >= CACHE_ENTRIES
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(key, (Instant::now(), tree.clone()));
    Ok(tree)
}

/// Regular files of a GitHub tarball, without its `<repo>-<ref>/` prefix.
fn unpack(gz: &[u8]) -> Result<Tree> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(gz));
    let mut tree = Tree::new();
    let mut total = 0usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() || entry.size() > MAX_FILE_BYTES {
            continue;
        }
        let path = entry.path()?.to_string_lossy().replace('\\', "/");
        let Some((_, rel)) = path.split_once('/') else {
            continue;
        };
        if rel.is_empty()
            || rel.split('/').any(|c| {
                c.is_empty() || c == "." || c == ".." || c == ".git" || c == "node_modules"
            })
        {
            continue;
        }
        total += entry.size() as usize;
        ensure!(
            total <= MAX_UNPACKED_BYTES,
            "Repository is too large to scan"
        );
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut data)?;
        tree.insert(rel.to_owned(), data);
    }
    Ok(tree)
}

fn text(tree: &Tree, path: &str) -> Option<String> {
    tree.get(path)
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn base(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn join(dir: &str, rel: &str) -> String {
    let rel = rel.trim_start_matches("./");
    if dir.is_empty() {
        rel.to_owned()
    } else {
        format!("{dir}/{rel}")
    }
}

fn is_code(path: &str) -> bool {
    (path.ends_with(".ts")
        || path.ends_with(".js")
        || path.ends_with(".mts")
        || path.ends_with(".mjs"))
        && !path.ends_with(".d.ts")
        && !path.contains(".test.")
        && !path.contains(".spec.")
}

/// A pi extension imports pi's API or registers through the `pi` object.
fn looks_like_extension(source: &str) -> bool {
    source.contains("pi-coding-agent")
        || source.contains("ExtensionAPI")
        || source.contains("registerTool(")
        || source.contains("registerCommand(")
}

/// First line of the leading comment, as a description.
fn comment_description(source: &str) -> String {
    source
        .lines()
        .map(|l| {
            l.trim()
                .trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_start_matches("//")
                .trim_start_matches('*')
                .trim_end_matches("*/")
                .trim()
        })
        .find(|l| {
            !l.is_empty() && !l.starts_with("import") && !l.starts_with('@') && !l.starts_with("#!")
        })
        .filter(|l| !l.contains(['{', '=', ';']))
        .map(|l| l.chars().take(200).collect())
        .unwrap_or_default()
}

/// Scan a repository for skills and plugins.
fn scan(tree: &Tree) -> Vec<Value> {
    let mut items = Vec::new();
    for (path, bytes) in tree {
        if base(path) == "SKILL.md" && !parent(path).is_empty() {
            let dir = parent(path);
            if let Some((name, description)) =
                crate::skills::describe(&String::from_utf8_lossy(bytes), base(dir))
            {
                items
                    .push(json!({"kind":"skill","name":name,"description":description,"path":dir}));
            }
        }
    }
    // Plugins: packages first; their files are not listed again.
    let mut package_dirs: Vec<String> = Vec::new();
    for path in tree.keys().filter(|p| base(p) == "package.json") {
        let dir = parent(path);
        let Some(pkg) = text(tree, path).and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        let entries: Vec<String> = match pkg["pi"]["extensions"].as_array() {
            Some(list) => list
                .iter()
                .filter_map(Value::as_str)
                .map(|e| join(dir, e))
                .collect(),
            None => pkg["main"]
                .as_str()
                .map(|m| join(dir, m))
                .into_iter()
                .chain(["index.ts", "index.js"].map(|i| join(dir, i)))
                .filter(|e| text(tree, e).is_some_and(|s| looks_like_extension(&s)))
                .take(1)
                .collect(),
        };
        let sources: Vec<String> = entries.iter().filter_map(|e| text(tree, e)).collect();
        if sources.is_empty() {
            continue;
        }
        let name = pkg["name"]
            .as_str()
            .map_or(base(dir), |n| base(n))
            .to_owned();
        let name = if name.is_empty() {
            "extension".to_owned()
        } else {
            name
        };
        let description = pkg["description"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| comment_description(&sources[0]));
        items.push(plugin(&name, &description, dir, &sources));
        package_dirs.push(format!("{dir}/"));
    }
    let in_package = |p: &str| {
        package_dirs
            .iter()
            .any(|d| d == "/" || p.starts_with(d.as_str()))
    };
    for (path, bytes) in tree {
        if in_package(path) || !is_code(path) {
            continue;
        }
        let dir = parent(path);
        let file = base(path);
        let stem = file.split('.').next().unwrap_or(file);
        // `extensions/<name>.ts` or `extensions/<name>/index.ts`.
        let (name, root) = if base(dir) == "extensions" {
            (stem, path.as_str())
        } else if base(parent(dir)) == "extensions" && stem == "index" {
            (base(dir), dir)
        } else {
            continue;
        };
        if name.starts_with('_') {
            continue;
        }
        let source = String::from_utf8_lossy(bytes);
        if !looks_like_extension(&source) {
            continue;
        }
        items.push(plugin(
            name,
            &comment_description(&source),
            root,
            &[source.into_owned()],
        ));
    }
    items
}

fn plugin(name: &str, description: &str, path: &str, sources: &[String]) -> Value {
    let tools = sources.iter().any(|s| s.contains("registerTool"));
    let outside = sources
        .iter()
        .any(|s| s.contains("from \"../") || s.contains("from '../"));
    json!({"kind":"plugin","name":name,"description":description,"path":path,"tools":tools,"outside_imports":outside})
}

fn safe_name(name: &str) -> Result<String> {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches(['.', '-']).to_owned();
    ensure!(!name.is_empty() && name.len() <= 100, "Invalid name");
    Ok(name)
}

/// Where an install writes: `(sandboxed project Files, relative dir)` or an absolute user dir.
enum Target {
    Project(Arc<Files>, String),
    User(PathBuf),
}

fn user_skills_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".pi").join("agent")))
        .map(|d| d.join("skills"))
        .context("No home directory for user skills")
}

fn target(files: &Arc<Files>, kind: &str, scope: &str, name: &str) -> Result<Target> {
    Ok(match (kind, scope) {
        ("skill", "project") => Target::Project(files.clone(), format!(".pi/skills/{name}")),
        ("skill", "user") => Target::User(user_skills_dir()?.join(name)),
        ("plugin", "project") => Target::Project(files.clone(), format!(".pi/extensions/{name}")),
        ("plugin", _) => bail!("Plugins install into the project only"),
        _ => bail!("Unknown kind or scope"),
    })
}

fn remove_project_tree(files: &Files, rel: &str) -> Result<()> {
    let listing = files.list(rel)?;
    for item in listing["items"].as_array().into_iter().flatten() {
        let child = format!("{rel}/{}", item["name"].as_str().unwrap_or_default());
        if item["directory"] == true && item["symlink"] != true {
            remove_project_tree(files, &child)?;
        } else {
            files.remove(&child)?;
        }
    }
    files.remove(rel)
}

fn exists(target: &Target) -> bool {
    match target {
        Target::Project(files, rel) => {
            files.list(rel).is_ok() || files.read_range(rel, Some(1), Some(1)).is_ok()
        }
        Target::User(dir) => std::fs::symlink_metadata(dir).is_ok(),
    }
}

fn clear(target: &Target) -> Result<()> {
    match target {
        Target::Project(files, rel) => {
            if files.list(rel).is_ok() {
                remove_project_tree(files, rel)?;
            }
            for single in [format!("{rel}.ts"), format!("{rel}.js")] {
                let _ = files.remove(&single);
            }
            Ok(())
        }
        Target::User(dir) => {
            if std::fs::symlink_metadata(dir)
                .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
            {
                std::fs::remove_dir_all(dir)?;
            }
            Ok(())
        }
    }
}

fn write(target: &Target, rel: &str, bytes: &[u8]) -> Result<()> {
    match target {
        Target::Project(files, dir) => {
            let path = join(dir, rel);
            files.mkdir(parent(&path))?;
            files.write(&path, bytes)
        }
        Target::User(dir) => {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().context("bad path")?)?;
            Ok(std::fs::write(path, bytes)?)
        }
    }
}

/// `catalog_list`: skills and plugins in `repo@ref`.
pub async fn list(repo: &str, git_ref: &str) -> Result<Value> {
    let tree = tree(repo, git_ref).await?;
    let items = scan(&tree);
    Ok(json!({"repo": repo, "ref": git_ref, "items": items}))
}

/// `catalog_install`: copy one listed item into the project or user directory.
pub async fn install(files: Arc<Files>, args: &Value) -> Result<Value> {
    let s = |k: &str| args[k].as_str().with_context(|| format!("Missing {k}"));
    let (repo, git_ref, kind, path, scope) =
        (s("repo")?, s("ref")?, s("kind")?, s("path")?, s("scope")?);
    let tree = tree(repo, git_ref).await?;
    let item = scan(&tree)
        .into_iter()
        .find(|i| i["kind"] == kind && i["path"] == path)
        .with_context(|| format!("No {kind} at '{path}' in {repo}"))?;
    let name = safe_name(item["name"].as_str().unwrap_or_default())?;
    let target = target(&files, kind, scope, &name)?;
    let replaced = exists(&target);
    // Single-file plugins keep their file name next to other extensions; directories are copied whole.
    let copies: Vec<(String, Vec<u8>)> = if tree.contains_key(path) {
        let ext = path.rsplit_once('.').map_or("ts", |(_, e)| e);
        vec![(format!("index.{ext}"), tree[path].clone())]
    } else {
        // A package at the repository root installs the whole repository, like `pi install git:`.
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        tree.iter()
            .filter_map(|(p, b)| {
                p.strip_prefix(&prefix)
                    .map(|rel| (rel.to_owned(), b.clone()))
            })
            .collect()
    };
    ensure!(!copies.is_empty(), "Nothing to install");
    ensure!(
        copies.len() <= MAX_INSTALL_FILES,
        "Too many files to install"
    );
    let count = copies.len();
    let (target, written) = tokio::task::spawn_blocking(move || -> Result<(Target, usize)> {
        clear(&target)?;
        for (rel, bytes) in &copies {
            write(&target, rel, bytes)?;
        }
        Ok((target, count))
    })
    .await??;
    let installed = match &target {
        Target::Project(_, rel) => rel.clone(),
        Target::User(dir) => dir.display().to_string(),
    };
    Ok(
        json!({"kind": kind, "name": name, "scope": scope, "installed": installed, "files": written, "replaced": replaced}),
    )
}

/// `catalog_remove`: delete an installed skill or plugin by name.
pub async fn remove(files: Arc<Files>, args: &Value) -> Result<Value> {
    let s = |k: &str| args[k].as_str().with_context(|| format!("Missing {k}"));
    let name = safe_name(s("name")?)?;
    let target = target(&files, s("kind")?, s("scope")?, &name)?;
    tokio::task::spawn_blocking(move || {
        let existed = exists(&target);
        clear(&target)?;
        Ok(json!({"removed": existed}))
    })
    .await?
}

/// `catalog_installed`: plugins installed in the project (skills come from `skill_list`).
pub fn installed(files: &Files) -> Value {
    let mut plugins = Vec::new();
    for dir in [".pi/extensions", ".venticode/extensions"] {
        let Ok(listing) = files.list(dir) else {
            continue;
        };
        for item in listing["items"].as_array().into_iter().flatten() {
            let name = item["name"].as_str().unwrap_or_default();
            if item["symlink"] != true && (item["directory"] == true || is_code(name)) {
                plugins.push(json!({"name": name.split('.').next().unwrap_or(name), "path": format!("{dir}/{name}")}));
            }
        }
    }
    json!({"plugins": plugins})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> Tree {
        files
            .iter()
            .map(|(p, c)| ((*p).to_owned(), c.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn scans_skills_packages_and_extension_files() {
        let t = tree(&[
            (
                "skills/eng/tdd/SKILL.md",
                "---\nname: tdd\ndescription: >\n  Red green\n  refactor.\n---\nBody",
            ),
            ("skills/eng/tdd/ref.md", "x"),
            ("skills/nodesc/SKILL.md", "---\nname: x\n---\n"),
            (
                "pkgs/paste/package.json",
                r#"{"name":"@me/pi-paste","description":"Paste","pi":{"extensions":["index.ts"]}}"#,
            ),
            (
                "pkgs/paste/index.ts",
                "import type { ExtensionAPI } from '@earendil-works/pi-coding-agent';\nexport default (pi: ExtensionAPI) => pi.registerCommand('paste', {});",
            ),
            (
                "apply/package.json",
                r#"{"name":"apply-patch","main":"index.ts"}"#,
            ),
            (
                "apply/index.ts",
                "export default function (pi) { pi.registerTool({name:'apply_patch'}) }",
            ),
            (
                "other/package.json",
                r#"{"name":"not-pi","main":"index.js"}"#,
            ),
            ("other/index.js", "module.exports = 1"),
            (
                "extensions/todos.ts",
                "/** Todo list tool */\nimport { Type } from 'typebox';\nexport default function (pi) { pi.registerTool({}) }",
            ),
            (
                "extensions/_shared/util.ts",
                "export const x = 'pi-coding-agent';",
            ),
            (
                "extensions/anycopy/index.ts",
                "import { x } from '../_shared/util';\nexport default (pi) => pi.on('x', () => {});\n// ExtensionAPI",
            ),
            ("extensions/anycopy/state.ts", "export {}"),
            ("extensions/notes.test.ts", "ExtensionAPI"),
            ("src/lib.ts", "ExtensionAPI"),
        ]);
        let items = scan(&t);
        let find = |kind: &str, name: &str| {
            items
                .iter()
                .find(|i| i["kind"] == kind && i["name"] == name)
                .cloned()
        };
        assert_eq!(
            find("skill", "tdd").unwrap()["description"],
            "Red green refactor."
        );
        assert!(find("skill", "x").is_none());
        let paste = find("plugin", "pi-paste").unwrap();
        assert_eq!(
            (paste["path"].as_str(), paste["tools"].as_bool()),
            (Some("pkgs/paste"), Some(false))
        );
        assert_eq!(find("plugin", "apply-patch").unwrap()["tools"], true);
        assert!(find("plugin", "not-pi").is_none());
        let todos = find("plugin", "todos").unwrap();
        assert_eq!(
            (todos["path"].as_str(), todos["description"].as_str()),
            (Some("extensions/todos.ts"), Some("Todo list tool"))
        );
        let anycopy = find("plugin", "anycopy").unwrap();
        assert_eq!(
            (
                anycopy["path"].as_str(),
                anycopy["outside_imports"].as_bool()
            ),
            (Some("extensions/anycopy"), Some(true))
        );
        assert!(
            find("plugin", "notes").is_none()
                && find("plugin", "lib").is_none()
                && find("plugin", "_shared").is_none()
        );
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn repo_and_ref_validation_blocks_path_tricks() {
        assert!(
            valid_repo("mattpocock/skills")
                && valid_repo("w-winter/dot314")
                && valid_repo("a/b.c_d")
        );
        for bad in [
            "", "a", "a/b/c", "../x", "a/..", "a b/c", "a/b?x=1", "a/b#c", "a/.",
        ] {
            assert!(!valid_repo(bad), "{bad}");
        }
        assert!(valid_ref("main") && valid_ref("v1.2.0") && valid_ref("release/2026"));
        for bad in ["", "../x", "/main", "a..b", "a b", "a?b"] {
            assert!(!valid_ref(bad), "{bad}");
        }
        assert_eq!(safe_name("@scope/../x y").unwrap(), "scope-..-x-y");
        assert!(safe_name("..").is_err());
    }

    #[tokio::test]
    async fn installs_into_project_and_replaces_previous_copy() {
        let dir = tempfile::tempdir().unwrap();
        let files = Arc::new(Files::open(dir.path()).unwrap());
        let mut t = tree(&[
            ("skills/tdd/SKILL.md", "---\ndescription: d\n---\nv1"),
            ("skills/tdd/scripts/run.sh", "echo"),
            (
                "extensions/todos.ts",
                "export default function (pi) { pi.registerTool({}) } // ExtensionAPI",
            ),
        ]);
        cache()
            .lock()
            .unwrap()
            .insert("o/r@main".into(), (Instant::now(), Arc::new(t.clone())));
        let args =
            json!({"repo":"o/r","ref":"main","kind":"skill","path":"skills/tdd","scope":"project"});
        let out = install(files.clone(), &args).await.unwrap();
        assert_eq!(
            (out["files"].as_u64(), out["replaced"].as_bool()),
            (Some(2), Some(false))
        );
        assert!(dir.path().join(".pi/skills/tdd/scripts/run.sh").is_file());
        t.remove("skills/tdd/scripts/run.sh");
        cache()
            .lock()
            .unwrap()
            .insert("o/r@main".into(), (Instant::now(), Arc::new(t)));
        let out = install(files.clone(), &args).await.unwrap();
        assert_eq!(
            (out["files"].as_u64(), out["replaced"].as_bool()),
            (Some(1), Some(true))
        );
        assert!(!dir.path().join(".pi/skills/tdd/scripts").exists());
        let plugin = json!({"repo":"o/r","ref":"main","kind":"plugin","path":"extensions/todos.ts","scope":"project"});
        install(files.clone(), &plugin).await.unwrap();
        assert!(dir.path().join(".pi/extensions/todos/index.ts").is_file());
        assert_eq!(installed(&files)["plugins"][0]["name"], "todos");
        let user = json!({"repo":"o/r","ref":"main","kind":"plugin","path":"extensions/todos.ts","scope":"user"});
        assert!(install(files.clone(), &user).await.is_err());
        let removed = remove(
            files.clone(),
            &json!({"kind":"skill","name":"tdd","scope":"project"}),
        )
        .await
        .unwrap();
        assert_eq!(removed["removed"], true);
        assert!(!dir.path().join(".pi/skills/tdd").exists());
    }

    /// Live check of the showcase sources: `cargo test -p venticode-harness catalog -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn showcase_sources_list_and_install() {
        let sources = std::env::var("CATALOG_SOURCES").unwrap_or_default();
        for spec in sources.split_whitespace() {
            let (repo, git_ref) = spec.split_once('@').unwrap_or((spec, "main"));
            let listed = list(repo, git_ref).await.unwrap();
            let items = listed["items"].as_array().unwrap();
            let skills = items.iter().filter(|i| i["kind"] == "skill").count();
            let plugins = items.iter().filter(|i| i["kind"] == "plugin").count();
            let tools = items.iter().filter(|i| i["tools"] == true).count();
            println!("{repo}@{git_ref}: {skills} skills, {plugins} plugins ({tools} with tools)");
            assert!(!items.is_empty(), "{repo} has nothing installable");
            let dir = tempfile::tempdir().unwrap();
            let files = Arc::new(Files::open(dir.path()).unwrap());
            for item in items {
                let args = json!({"repo":repo,"ref":git_ref,"kind":item["kind"],"path":item["path"],"scope":"project"});
                install(files.clone(), &args)
                    .await
                    .unwrap_or_else(|e| panic!("{repo} {}: {e}", item["path"]));
            }
            let installed = crate::skills::list(dir.path());
            let found = installed["skills"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| {
                    s["location"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with(&dir.path().display().to_string())
                })
                .count();
            println!("  installed and rediscovered {found}/{skills} skills");
            if plugins > 0 {
                let loaded = crate::extensions::list(files.clone()).await.unwrap();
                let tools = loaded["tools"].as_array().unwrap();
                let errors = loaded["errors"].as_array().unwrap();
                let mut with_tools: Vec<&str> = tools
                    .iter()
                    .filter_map(|t| t["extension"].as_str())
                    .collect();
                with_tools.sort();
                with_tools.dedup();
                println!(
                    "  plugins: {} tools from {} entries, {} entries failed",
                    tools.len(),
                    with_tools.len(),
                    errors.len()
                );
                for e in errors {
                    let message = e["message"]
                        .as_str()
                        .unwrap_or_default()
                        .lines()
                        .next()
                        .unwrap_or_default();
                    println!(
                        "    FAIL {}: {message}",
                        e["extension"].as_str().unwrap_or_default()
                    );
                }
            }
        }
    }
}
