use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

pub const FILE_LIMIT: usize = 4 * 1024 * 1024;
pub const INLINE_TEXT_LIMIT: usize = 512 * 1024;
pub const RANGE_FILE_LIMIT: usize = 8 * 1024 * 1024;
const IGNORE_FILE_LIMIT: usize = 64 * 1024;

const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    "dist",
    ".next",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub target: String,
    pub replacement: String,
    #[serde(default)]
    pub replace_all: bool,
}

enum FilesInner {
    Sandboxed { dir: Dir, path: PathBuf },
    Yolo { base: PathBuf },
}

pub struct Files {
    inner: FilesInner,
}

pub fn relative(path: &str) -> Result<&Path> {
    let value = Path::new(path);
    ensure!(
        !path.contains(['\\', '\0'])
            && !value.is_absolute()
            && value
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
        "Percorso fuori dalla cartella concessa / Path outside granted directory"
    );
    Ok(value)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn glob_match_segment(pat: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);
    while ti < text.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

fn glob_match_parts(pat: &[&str], parts: &[&str]) -> bool {
    if pat.is_empty() {
        return parts.is_empty();
    }
    if pat[0] == "**" {
        if glob_match_parts(&pat[1..], parts) {
            return true;
        }
        if !parts.is_empty() {
            return glob_match_parts(pat, &parts[1..]);
        }
        return false;
    }
    if parts.is_empty() {
        return false;
    }
    glob_match_segment(pat[0].as_bytes(), parts[0].as_bytes())
        && glob_match_parts(&pat[1..], &parts[1..])
}

pub fn glob_matches(pattern: &str, rel_path: &str) -> bool {
    let pat = pattern.trim().trim_start_matches("./").trim_matches('/');
    if pat.is_empty() {
        return true;
    }
    if !pat.contains('/') {
        let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path);
        return glob_match_segment(pat.as_bytes(), file_name.as_bytes());
    }
    let pat_parts: Vec<&str> = pat.split('/').filter(|s| !s.is_empty()).collect();
    let path_parts: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    glob_match_parts(&pat_parts, &path_parts)
}

#[derive(Default)]
struct IgnoreRules(Vec<(PathBuf, Gitignore)>);

impl IgnoreRules {
    fn for_sandbox(root: &Dir, start: &Dir, rel_base: &Path) -> Self {
        let mut rules = Self::default();
        rules.push(Path::new(""), Self::read_matcher(root));
        // Explicit subdirectory searches still inherit rules from the grant's root.
        let ancestors = rel_base.ancestors().skip(1).collect::<Vec<_>>();
        for ancestor in ancestors.into_iter().rev() {
            if !ancestor.as_os_str().is_empty()
                && let Ok(dir) = root.open_dir(ancestor)
            {
                rules.push(ancestor, Self::read_matcher(&dir));
            }
        }
        if !rel_base.as_os_str().is_empty() {
            rules.push(rel_base, Self::read_matcher(start));
        }
        rules
    }

    fn read_matcher(dir: &Dir) -> Option<Gitignore> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = dir.open_with(".gitignore", &options).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > IGNORE_FILE_LIMIT as u64 {
            return None;
        }
        let mut raw = Vec::new();
        file.take((IGNORE_FILE_LIMIT + 1) as u64)
            .read_to_end(&mut raw)
            .ok()?;
        if raw.len() > IGNORE_FILE_LIMIT {
            return None;
        }
        let text = std::str::from_utf8(&raw).ok()?;
        let mut builder = GitignoreBuilder::new(".");
        for line in text.trim_start_matches('\u{feff}').lines() {
            let _ = builder.add_line(None, line);
        }
        builder.build().ok()
    }

    fn read_host_matcher(path: &Path) -> Option<Gitignore> {
        let dir = Dir::open_ambient_dir(path, cap_std::ambient_authority()).ok()?;
        Self::read_matcher(&dir)
    }

    fn push(&mut self, prefix: &Path, matcher: Option<Gitignore>) {
        if let Some(matcher) = matcher.filter(|matcher| !matcher.is_empty()) {
            self.0.push((prefix.to_owned(), matcher));
        }
    }

    fn within<T>(
        &mut self,
        prefix: &str,
        matcher: Option<Gitignore>,
        visit: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let depth = self.0.len();
        self.push(Path::new(prefix), matcher);
        let result = visit(self);
        self.0.truncate(depth);
        result
    }

    fn is_ignored(&self, rel_path: &str, name: &str, is_dir: bool) -> bool {
        if is_dir && DEFAULT_IGNORED_DIRS.contains(&name) {
            return true;
        }
        if name.starts_with(".ventiharness-") && name.ends_with(".tmp") {
            return true;
        }
        for (prefix, matcher) in self.0.iter().rev() {
            if let Ok(path) = Path::new(rel_path).strip_prefix(prefix) {
                let matched = matcher.matched(path, is_dir);
                if !matched.is_none() {
                    return matched.is_ignore();
                }
            }
        }
        false
    }
}

fn yolo_resolve(base: &Path, path: &str) -> Result<PathBuf> {
    ensure!(!path.contains('\0'), "Invalid path containing NUL byte");
    let p = Path::new(path);
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    })
}

fn read_host_regular_file(target: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(target)?;
    ensure!(file.metadata()?.is_file(), "Regular file required");
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        if limit == FILE_LIMIT {
            "File exceeds 4 MiB"
        } else {
            "File exceeds 8 MiB"
        }
    );
    Ok(bytes)
}

fn write_host_atomic(target: &Path, bytes: &[u8], limit: usize) -> Result<()> {
    ensure!(
        bytes.len() <= limit,
        if limit == FILE_LIMIT {
            "File exceeds 4 MiB"
        } else {
            "File exceeds 8 MiB"
        }
    );
    let name = target.file_name().context("File name required")?;
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp_path = parent.join(format!(".ventiharness-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temp_path)?;
        match std::fs::symlink_metadata(target) {
            Ok(metadata) if metadata.is_file() => {
                file.set_permissions(metadata.permissions())?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        let dest = parent.join(name);
        std::fs::rename(&temp_path, dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

impl Files {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.canonicalize()?;
        ensure!(
            path.is_dir() && path.parent().is_some(),
            "Invalid project root"
        );
        let dir = Dir::open_ambient_dir(&path, cap_std::ambient_authority())?;
        Ok(Self {
            inner: FilesInner::Sandboxed { dir, path },
        })
    }

    pub fn open_yolo(base: &Path) -> Result<Self> {
        let canonical = base.canonicalize()?;
        ensure!(canonical.is_dir(), "Invalid working directory");
        Ok(Self {
            inner: FilesInner::Yolo { base: canonical },
        })
    }

    pub fn base_path(&self) -> &Path {
        match &self.inner {
            FilesInner::Sandboxed { path, .. } => path,
            FilesInner::Yolo { base } => base,
        }
    }

    pub fn directory(&self, path: &str) -> Result<PathBuf> {
        match &self.inner {
            FilesInner::Sandboxed { dir, path: root } => {
                let relative = relative(path)?;
                dir.open_dir(relative)?;
                let value = root.join(relative).canonicalize()?;
                ensure!(value.starts_with(root), "Directory outside granted root");
                Ok(value)
            }
            FilesInner::Yolo { base } => {
                let target = yolo_resolve(base, path)?.canonicalize()?;
                ensure!(target.is_dir(), "Directory required");
                Ok(target)
            }
        }
    }

    fn open_regular_file(&self, path: &str, limit: usize) -> Result<Vec<u8>> {
        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => {
                let rel = relative(path)?;
                let mut options = OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use cap_std::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NONBLOCK);
                }
                let file = dir.open_with(rel, &options)?;
                ensure!(file.metadata()?.is_file(), "Regular file required");
                let mut bytes = Vec::new();
                file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= limit,
                    if limit == FILE_LIMIT {
                        "File exceeds 4 MiB"
                    } else {
                        "File exceeds 8 MiB"
                    }
                );
                Ok(bytes)
            }
            FilesInner::Yolo { base } => read_host_regular_file(&yolo_resolve(base, path)?, limit),
        }
    }

    pub fn list(&self, path: &str) -> Result<Value> {
        let mut items = Vec::new();
        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => {
                for entry in dir.open_dir(relative(path)?)?.entries()? {
                    ensure!(
                        items.len() < 4096,
                        "Too many entries; choose a narrower directory"
                    );
                    let entry = entry?;
                    let metadata = entry.metadata()?;
                    items.push(json!({
                        "name": entry.file_name().to_string_lossy(),
                        "directory": metadata.is_dir(),
                        "symlink": entry.file_type()?.is_symlink(),
                        "bytes": metadata.len()
                    }));
                }
            }
            FilesInner::Yolo { base } => {
                for entry in std::fs::read_dir(yolo_resolve(base, path)?)? {
                    ensure!(
                        items.len() < 4096,
                        "Too many entries; choose a narrower directory"
                    );
                    let entry = entry?;
                    let ft = entry.file_type()?;
                    let metadata = entry.metadata().unwrap_or_else(|_| {
                        std::fs::symlink_metadata(entry.path()).expect("entry metadata")
                    });
                    items.push(json!({
                        "name": entry.file_name().to_string_lossy(),
                        "directory": metadata.is_dir(),
                        "symlink": ft.is_symlink(),
                        "bytes": metadata.len()
                    }));
                }
            }
        }
        items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({"items":items}))
    }

    pub fn read(&self, path: &str) -> Result<Value> {
        self.read_range(path, None, None)
    }

    pub fn read_range(
        &self,
        path: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<Value> {
        let has_range = start_line.is_some() || end_line.is_some();
        let limit = if has_range {
            RANGE_FILE_LIMIT
        } else {
            FILE_LIMIT
        };
        let bytes = self.open_regular_file(path, limit)?;
        let digest = sha256_hex(&bytes);
        match String::from_utf8(bytes) {
            Ok(full) => {
                let segments: Vec<&str> = if full.is_empty() {
                    Vec::new()
                } else {
                    full.split_inclusive('\n').collect()
                };
                let total_lines = segments.len();
                if !has_range && full.len() <= INLINE_TEXT_LIMIT {
                    let end = total_lines;
                    let start = if total_lines == 0 { 0 } else { 1 };
                    return Ok(json!({
                        "encoding": "utf8",
                        "content": full,
                        "sha256": digest,
                        "total_lines": total_lines,
                        "start_line": start,
                        "end_line": end,
                        "truncated": false
                    }));
                }
                let req_start = start_line.unwrap_or(1).max(1);
                let req_end = end_line.unwrap_or(usize::MAX).max(req_start);
                if total_lines == 0 || req_start > total_lines {
                    return Ok(json!({
                        "encoding": "utf8",
                        "content": "",
                        "sha256": digest,
                        "total_lines": total_lines,
                        "start_line": req_start,
                        "end_line": 0,
                        "truncated": false
                    }));
                }
                let capped_end = req_end.min(total_lines);
                let mut out = String::new();
                let mut actual_end = req_start - 1;
                let mut truncated = false;
                for (idx, line) in segments[(req_start - 1)..capped_end].iter().enumerate() {
                    if out.len() + line.len() > INLINE_TEXT_LIMIT {
                        ensure!(
                            idx != 0,
                            "Line {req_start} exceeds the {INLINE_TEXT_LIMIT}-byte inline read limit"
                        );
                        truncated = true;
                        break;
                    }
                    out.push_str(line);
                    actual_end = req_start + idx;
                }
                Ok(json!({
                    "encoding": "utf8",
                    "content": out,
                    "sha256": digest,
                    "total_lines": total_lines,
                    "start_line": req_start,
                    "end_line": actual_end,
                    "truncated": truncated
                }))
            }
            Err(error) => {
                let raw = error.as_bytes();
                ensure!(
                    !has_range,
                    "Line-range read requires UTF-8 text file / Lettura per righe richiede file UTF-8"
                );
                Ok(json!({
                    "encoding": "base64",
                    "content": STANDARD.encode(raw),
                    "bytes": raw.len(),
                    "sha256": digest
                }))
            }
        }
    }

    fn write_atomic_target(&self, path: &str, bytes: &[u8], limit: usize) -> Result<()> {
        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => {
                let rel = relative(path)?;
                ensure!(
                    bytes.len() <= limit,
                    if limit == FILE_LIMIT {
                        "File exceeds 4 MiB"
                    } else {
                        "File exceeds 8 MiB"
                    }
                );
                let name = rel.file_name().context("File name required")?;
                let parent = dir.open_dir(
                    rel.parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(Path::new(".")),
                )?;
                let temporary = format!(".ventiharness-{}.tmp", Uuid::new_v4());
                let result = (|| -> Result<()> {
                    let mut options = OpenOptions::new();
                    options.write(true).create_new(true);
                    let mut file = parent.open_with(&temporary, &options)?;
                    match parent.symlink_metadata(name) {
                        Ok(metadata) if metadata.is_file() => {
                            file.set_permissions(metadata.permissions())?
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    file.write_all(bytes)?;
                    file.sync_all()?;
                    drop(file);
                    parent.rename(&temporary, &parent, name)?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = parent.remove_file(&temporary);
                }
                result
            }
            FilesInner::Yolo { base } => {
                write_host_atomic(&yolo_resolve(base, path)?, bytes, limit)
            }
        }
    }

    pub fn write(&self, path: &str, bytes: &[u8]) -> Result<()> {
        self.write_atomic_target(path, bytes, FILE_LIMIT)
    }

    pub fn edit(&self, path: &str, edits: &[Edit], expected_sha256: Option<&str>) -> Result<Value> {
        ensure!(!edits.is_empty(), "At least one edit is required");
        let bytes = self.open_regular_file(path, RANGE_FILE_LIMIT)?;
        let current_sha = sha256_hex(&bytes);
        if let Some(expected) = expected_sha256 {
            ensure!(
                current_sha.eq_ignore_ascii_case(expected.trim()),
                "SHA-256 mismatch: file changed on disk (expected {}, actual {})",
                expected.trim(),
                current_sha
            );
        }
        let mut content =
            String::from_utf8(bytes).context("edit_file requires a valid UTF-8 text file")?;
        let mut total_replacements = 0usize;
        for (idx, item) in edits.iter().enumerate() {
            ensure!(
                !item.target.is_empty(),
                "Edit #{} target must not be empty",
                idx + 1
            );
            let mut target = Cow::Borrowed(item.target.as_str());
            let mut count = content.matches(target.as_ref()).count();
            if count == 0
                && item.target.contains('\n')
                && !item.target.contains('\r')
                && content
                    .split("\r\n")
                    .all(|part| !part.contains(['\r', '\n']))
            {
                target = Cow::Owned(item.target.replace('\n', "\r\n"));
                count = content.matches(target.as_ref()).count();
            }
            if count == 0 {
                bail!("Edit #{} target not found in {}", idx + 1, path);
            }
            if !item.replace_all && count > 1 {
                bail!(
                    "Edit #{} target matched {} occurrences in {}; provide more context or set replace_all=true",
                    idx + 1,
                    count,
                    path
                );
            }
            let replacement = if matches!(target, Cow::Owned(_)) {
                Cow::Owned(item.replacement.replace("\r\n", "\n").replace('\n', "\r\n"))
            } else {
                Cow::Borrowed(item.replacement.as_str())
            };
            content = content.replace(target.as_ref(), replacement.as_ref());
            total_replacements += count;
        }
        self.write_atomic_target(path, content.as_bytes(), RANGE_FILE_LIMIT)?;
        let new_sha = sha256_hex(content.as_bytes());
        Ok(json!({
            "written": true,
            "sha256": new_sha,
            "replacements": total_replacements
        }))
    }

    pub fn find_files(
        &self,
        path: &str,
        pattern: Option<&str>,
        max_results: usize,
    ) -> Result<Value> {
        let max_results = max_results.clamp(1, 2000);
        let mut items = Vec::new();
        let mut truncated = false;
        let mut visited_dirs = 0usize;
        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => {
                let rel_base = relative(path)?;
                let start_dir = dir.open_dir(rel_base)?;
                let base_prefix = rel_base
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("/");
                let mut ignore = IgnoreRules::for_sandbox(dir, &start_dir, Path::new(&base_prefix));

                #[allow(clippy::too_many_arguments)]
                fn walk_find(
                    dir: &Dir,
                    prefix: &str,
                    pattern: Option<&str>,
                    ignore: &mut IgnoreRules,
                    max_results: usize,
                    visited_dirs: &mut usize,
                    items: &mut Vec<Value>,
                    truncated: &mut bool,
                ) -> Result<()> {
                    *visited_dirs += 1;
                    if *visited_dirs > 4096 {
                        *truncated = true;
                        return Ok(());
                    }
                    let mut entries = Vec::new();
                    for entry in dir.entries()? {
                        let entry = entry?;
                        let ft = entry.file_type()?;
                        if ft.is_symlink() {
                            continue;
                        }
                        let name = entry.file_name().to_string_lossy().into_owned();
                        entries.push((name, entry, ft.is_dir(), ft.is_file()));
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    for (name, entry, is_dir, is_file) in entries {
                        let rel = if prefix.is_empty() {
                            name.clone()
                        } else {
                            format!("{prefix}/{name}")
                        };
                        if ignore.is_ignored(&rel, &name, is_dir) {
                            continue;
                        }
                        if is_dir {
                            if let Ok(sub) = dir.open_dir_nofollow(entry.file_name()) {
                                ignore.within(&rel, IgnoreRules::read_matcher(&sub), |ignore| {
                                    walk_find(
                                        &sub,
                                        &rel,
                                        pattern,
                                        ignore,
                                        max_results,
                                        visited_dirs,
                                        items,
                                        truncated,
                                    )
                                })?;
                                if *truncated {
                                    return Ok(());
                                }
                            }
                        } else if is_file && pattern.is_none_or(|p| glob_matches(p, &rel)) {
                            if items.len() >= max_results {
                                *truncated = true;
                                return Ok(());
                            }
                            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                            items.push(json!({"path": rel, "bytes": bytes}));
                        }
                    }
                    Ok(())
                }

                walk_find(
                    &start_dir,
                    &base_prefix,
                    pattern.filter(|p| !p.trim().is_empty()),
                    &mut ignore,
                    max_results,
                    &mut visited_dirs,
                    &mut items,
                    &mut truncated,
                )?;
            }
            FilesInner::Yolo { base } => {
                let start_dir = yolo_resolve(base, path)?;
                ensure!(start_dir.is_dir(), "Directory required");
                let mut ignore = IgnoreRules::default();
                ignore.push(Path::new(""), IgnoreRules::read_host_matcher(&start_dir));

                #[allow(clippy::too_many_arguments)]
                fn walk_find_host(
                    dir: &Path,
                    prefix: &str,
                    pattern: Option<&str>,
                    ignore: &mut IgnoreRules,
                    max_results: usize,
                    visited_dirs: &mut usize,
                    items: &mut Vec<Value>,
                    truncated: &mut bool,
                ) -> Result<()> {
                    *visited_dirs += 1;
                    if *visited_dirs > 4096 {
                        *truncated = true;
                        return Ok(());
                    }
                    let Ok(rd) = std::fs::read_dir(dir) else {
                        return Ok(());
                    };
                    let mut entries = Vec::new();
                    for entry in rd.flatten() {
                        let Ok(ft) = entry.file_type() else { continue };
                        if ft.is_symlink() {
                            continue;
                        }
                        let name = entry.file_name().to_string_lossy().into_owned();
                        entries.push((name, entry.path(), ft.is_dir(), ft.is_file()));
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    for (name, full_path, is_dir, is_file) in entries {
                        let rel = if prefix.is_empty() {
                            name.clone()
                        } else {
                            format!("{prefix}/{name}")
                        };
                        if ignore.is_ignored(&rel, &name, is_dir) {
                            continue;
                        }
                        if is_dir {
                            ignore.within(
                                &rel,
                                IgnoreRules::read_host_matcher(&full_path),
                                |ignore| {
                                    walk_find_host(
                                        &full_path,
                                        &rel,
                                        pattern,
                                        ignore,
                                        max_results,
                                        visited_dirs,
                                        items,
                                        truncated,
                                    )
                                },
                            )?;
                            if *truncated {
                                return Ok(());
                            }
                        } else if is_file && pattern.is_none_or(|p| glob_matches(p, &rel)) {
                            if items.len() >= max_results {
                                *truncated = true;
                                return Ok(());
                            }
                            let bytes = std::fs::metadata(&full_path).map(|m| m.len()).unwrap_or(0);
                            items.push(json!({"path": rel, "bytes": bytes}));
                        }
                    }
                    Ok(())
                }

                walk_find_host(
                    &start_dir,
                    "",
                    pattern.filter(|p| !p.trim().is_empty()),
                    &mut ignore,
                    max_results,
                    &mut visited_dirs,
                    &mut items,
                    &mut truncated,
                )?;
            }
        }
        Ok(json!({
            "items": items,
            "truncated": truncated
        }))
    }

    pub fn search_files(
        &self,
        path: &str,
        query: &str,
        pattern: Option<&str>,
        case_sensitive: bool,
        max_results: usize,
    ) -> Result<Value> {
        ensure!(!query.is_empty(), "Search query must not be empty");
        let max_results = max_results.clamp(1, 1000);
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        let mut matches = Vec::new();
        let mut truncated = false;
        let mut visited_dirs = 0usize;

        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => {
                let rel_base = relative(path)?;
                let start_dir = dir.open_dir(rel_base)?;
                let base_prefix = rel_base
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("/");
                let mut ignore = IgnoreRules::for_sandbox(dir, &start_dir, Path::new(&base_prefix));

                #[allow(clippy::too_many_arguments)]
                fn walk_search(
                    dir: &Dir,
                    prefix: &str,
                    needle: &str,
                    case_sensitive: bool,
                    pattern: Option<&str>,
                    ignore: &mut IgnoreRules,
                    max_results: usize,
                    visited_dirs: &mut usize,
                    matches: &mut Vec<Value>,
                    truncated: &mut bool,
                ) -> Result<()> {
                    *visited_dirs += 1;
                    if *visited_dirs > 4096 {
                        *truncated = true;
                        return Ok(());
                    }
                    let mut entries = Vec::new();
                    for entry in dir.entries()? {
                        let entry = entry?;
                        let ft = entry.file_type()?;
                        if ft.is_symlink() {
                            continue;
                        }
                        let name = entry.file_name().to_string_lossy().into_owned();
                        entries.push((name, entry, ft.is_dir(), ft.is_file()));
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    for (name, entry, is_dir, is_file) in entries {
                        let rel = if prefix.is_empty() {
                            name.clone()
                        } else {
                            format!("{prefix}/{name}")
                        };
                        if ignore.is_ignored(&rel, &name, is_dir) {
                            continue;
                        }
                        if is_dir {
                            if let Ok(sub) = dir.open_dir_nofollow(entry.file_name()) {
                                ignore.within(&rel, IgnoreRules::read_matcher(&sub), |ignore| {
                                    walk_search(
                                        &sub,
                                        &rel,
                                        needle,
                                        case_sensitive,
                                        pattern,
                                        ignore,
                                        max_results,
                                        visited_dirs,
                                        matches,
                                        truncated,
                                    )
                                })?;
                                if *truncated {
                                    return Ok(());
                                }
                            }
                        } else if is_file && pattern.is_none_or(|p| glob_matches(p, &rel)) {
                            let Ok(meta) = entry.metadata() else {
                                continue;
                            };
                            if meta.len() > (2 * 1024 * 1024) as u64 {
                                continue;
                            }
                            let mut options = OpenOptions::new();
                            options.read(true).follow(FollowSymlinks::No);
                            #[cfg(unix)]
                            {
                                use cap_std::fs::OpenOptionsExt;
                                options.custom_flags(libc::O_NONBLOCK);
                            }
                            let Ok(file) = dir.open_with(entry.file_name(), &options) else {
                                continue;
                            };
                            let mut buf = Vec::new();
                            if file.take(2 * 1024 * 1024).read_to_end(&mut buf).is_err() {
                                continue;
                            }
                            if buf.contains(&0) {
                                continue;
                            }
                            let Ok(text) = String::from_utf8(buf) else {
                                continue;
                            };
                            for (idx, line) in text.lines().enumerate() {
                                let matched_col = if case_sensitive {
                                    line.find(needle)
                                } else {
                                    line.to_lowercase().find(needle)
                                };
                                if let Some(col) = matched_col {
                                    if matches.len() >= max_results {
                                        *truncated = true;
                                        return Ok(());
                                    }
                                    let preview: String = line.chars().take(300).collect();
                                    matches.push(json!({
                                        "path": rel,
                                        "line": idx + 1,
                                        "column": col + 1,
                                        "text": preview
                                    }));
                                }
                            }
                        }
                    }
                    Ok(())
                }

                walk_search(
                    &start_dir,
                    &base_prefix,
                    &needle,
                    case_sensitive,
                    pattern.filter(|p| !p.trim().is_empty()),
                    &mut ignore,
                    max_results,
                    &mut visited_dirs,
                    &mut matches,
                    &mut truncated,
                )?;
            }
            FilesInner::Yolo { base } => {
                let start_dir = yolo_resolve(base, path)?;
                ensure!(start_dir.is_dir(), "Directory required");
                let mut ignore = IgnoreRules::default();
                ignore.push(Path::new(""), IgnoreRules::read_host_matcher(&start_dir));

                #[allow(clippy::too_many_arguments)]
                fn walk_search_host(
                    dir: &Path,
                    prefix: &str,
                    needle: &str,
                    case_sensitive: bool,
                    pattern: Option<&str>,
                    ignore: &mut IgnoreRules,
                    max_results: usize,
                    visited_dirs: &mut usize,
                    matches: &mut Vec<Value>,
                    truncated: &mut bool,
                ) -> Result<()> {
                    *visited_dirs += 1;
                    if *visited_dirs > 4096 {
                        *truncated = true;
                        return Ok(());
                    }
                    let Ok(rd) = std::fs::read_dir(dir) else {
                        return Ok(());
                    };
                    let mut entries = Vec::new();
                    for entry in rd.flatten() {
                        let Ok(ft) = entry.file_type() else { continue };
                        if ft.is_symlink() {
                            continue;
                        }
                        let name = entry.file_name().to_string_lossy().into_owned();
                        entries.push((name, entry.path(), ft.is_dir(), ft.is_file()));
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    for (name, full_path, is_dir, is_file) in entries {
                        let rel = if prefix.is_empty() {
                            name.clone()
                        } else {
                            format!("{prefix}/{name}")
                        };
                        if ignore.is_ignored(&rel, &name, is_dir) {
                            continue;
                        }
                        if is_dir {
                            ignore.within(
                                &rel,
                                IgnoreRules::read_host_matcher(&full_path),
                                |ignore| {
                                    walk_search_host(
                                        &full_path,
                                        &rel,
                                        needle,
                                        case_sensitive,
                                        pattern,
                                        ignore,
                                        max_results,
                                        visited_dirs,
                                        matches,
                                        truncated,
                                    )
                                },
                            )?;
                            if *truncated {
                                return Ok(());
                            }
                        } else if is_file && pattern.is_none_or(|p| glob_matches(p, &rel)) {
                            let Ok(meta) = std::fs::metadata(&full_path) else {
                                continue;
                            };
                            if meta.len() > (2 * 1024 * 1024) as u64 {
                                continue;
                            }
                            let Ok(buf) = read_host_regular_file(&full_path, 2 * 1024 * 1024)
                            else {
                                continue;
                            };
                            if buf.contains(&0) {
                                continue;
                            }
                            let Ok(text) = String::from_utf8(buf) else {
                                continue;
                            };
                            for (idx, line) in text.lines().enumerate() {
                                let matched_col = if case_sensitive {
                                    line.find(needle)
                                } else {
                                    line.to_lowercase().find(needle)
                                };
                                if let Some(col) = matched_col {
                                    if matches.len() >= max_results {
                                        *truncated = true;
                                        return Ok(());
                                    }
                                    let preview: String = line.chars().take(300).collect();
                                    matches.push(json!({
                                        "path": rel,
                                        "line": idx + 1,
                                        "column": col + 1,
                                        "text": preview
                                    }));
                                }
                            }
                        }
                    }
                    Ok(())
                }

                walk_search_host(
                    &start_dir,
                    "",
                    &needle,
                    case_sensitive,
                    pattern.filter(|p| !p.trim().is_empty()),
                    &mut ignore,
                    max_results,
                    &mut visited_dirs,
                    &mut matches,
                    &mut truncated,
                )?;
            }
        }
        Ok(json!({
            "items": matches,
            "truncated": truncated
        }))
    }

    pub fn mkdir(&self, path: &str) -> Result<()> {
        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => Ok(dir.create_dir_all(relative(path)?)?),
            FilesInner::Yolo { base } => Ok(std::fs::create_dir_all(yolo_resolve(base, path)?)?),
        }
    }

    pub fn rename(&self, path: &str, destination: &str) -> Result<()> {
        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => {
                Ok(dir.rename(relative(path)?, dir, relative(destination)?)?)
            }
            FilesInner::Yolo { base } => Ok(std::fs::rename(
                yolo_resolve(base, path)?,
                yolo_resolve(base, destination)?,
            )?),
        }
    }

    pub fn remove(&self, path: &str) -> Result<()> {
        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => {
                let rel = relative(path)?;
                ensure!(rel.file_name().is_some(), "Cannot remove project root");
                if dir.symlink_metadata(rel)?.is_dir() {
                    dir.remove_dir(rel)?;
                } else {
                    dir.remove_file(rel)?;
                }
                Ok(())
            }
            FilesInner::Yolo { base } => {
                let target = yolo_resolve(base, path)?;
                ensure!(
                    target != *base && target != Path::new("/"),
                    "Cannot remove base or filesystem root"
                );
                if std::fs::symlink_metadata(&target)?.is_dir() {
                    std::fs::remove_dir(&target)?;
                } else {
                    std::fs::remove_file(&target)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn files_roundtrip_and_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        files.mkdir("src").unwrap();
        files.write("src/a.txt", b"original").unwrap();
        files.write("src/a.txt", b"updated").unwrap();
        assert_eq!(files.read("src/a.txt").unwrap()["content"], "updated");
        files.rename("src/a.txt", "src/b.txt").unwrap();
        assert_eq!(files.list("src").unwrap()["items"][0]["name"], "b.txt");
        for path in ["../secret", "/etc/passwd", "src/../../secret", "C:\\secret"] {
            assert!(files.read(path).is_err());
            assert!(files.write(path, b"bad").is_err());
        }
        assert!(files.write("large", &vec![0; FILE_LIMIT + 1]).is_err());
        assert!(files.remove("src").is_err());
        files.remove("src/b.txt").unwrap();
        files.remove("src").unwrap();
    }

    #[test]
    fn read_range_edit_find_and_search() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        files.mkdir("src/nested").unwrap();
        files.mkdir("node_modules/pkg").unwrap();
        files
            .write("node_modules/pkg/index.js", b"export const ignored = 1;\n")
            .unwrap();
        files.write(".gitignore", b"ignored.log\nbuild/\n").unwrap();
        files.write("ignored.log", b"secret needle\n").unwrap();
        files.mkdir("build").unwrap();
        files.write("build/out.txt", b"needle in build\n").unwrap();
        files
            .write("src/nested/lib.rs", b"line1\nline2 needle\nline3\nline4\n")
            .unwrap();

        let ranged = files
            .read_range("src/nested/lib.rs", Some(2), Some(3))
            .unwrap();
        assert_eq!(ranged["content"], "line2 needle\nline3\n");
        assert_eq!(ranged["total_lines"], 4);
        assert_eq!(ranged["start_line"], 2);
        assert_eq!(ranged["end_line"], 3);
        let sha = ranged["sha256"].as_str().unwrap().to_string();

        // Stale sha256 fails
        assert!(
            files
                .edit(
                    "src/nested/lib.rs",
                    &[Edit {
                        target: "line2 needle".into(),
                        replacement: "line2 updated".into(),
                        replace_all: false,
                    }],
                    Some("0000000000000000000000000000000000000000000000000000000000000000"),
                )
                .is_err()
        );

        // Ambiguous target without replace_all fails
        assert!(
            files
                .edit(
                    "src/nested/lib.rs",
                    &[Edit {
                        target: "line".into(),
                        replacement: "row".into(),
                        replace_all: false,
                    }],
                    Some(&sha),
                )
                .is_err()
        );

        let edited = files
            .edit(
                "src/nested/lib.rs",
                &[Edit {
                    target: "line2 needle".into(),
                    replacement: "line2 needle_updated".into(),
                    replace_all: false,
                }],
                Some(&sha),
            )
            .unwrap();
        assert_eq!(edited["replacements"], 1);

        let found = files.find_files(".", Some("**/*.rs"), 100).unwrap();
        let paths: Vec<&str> = found["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["src/nested/lib.rs"]);

        let searched = files
            .search_files(".", "NEEDLE_UPDATED", None, false, 100)
            .unwrap();
        let hits = searched["items"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["path"], "src/nested/lib.rs");
        assert_eq!(hits[0]["line"], 2);
    }

    #[test]
    fn read_reports_oversized_starting_lines_and_returns_complete_pages() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        let full_line = format!("{}x\n", "é".repeat(INLINE_TEXT_LIMIT / 2 - 1));
        let oversized = format!("{}\n", "é".repeat(INLINE_TEXT_LIMIT / 2));
        assert_eq!(full_line.len(), INLINE_TEXT_LIMIT);
        files
            .write("pages", format!("{full_line}last\n").as_bytes())
            .unwrap();
        files
            .write("large", format!("{oversized}last\n").as_bytes())
            .unwrap();
        files
            .write("middle", format!("first\n{oversized}last\n").as_bytes())
            .unwrap();

        let first = files.read("pages").unwrap();
        assert_eq!(first["content"], full_line);
        assert_eq!(first["end_line"], 1);
        assert_eq!(first["total_lines"], 2);
        assert_eq!(first["truncated"], true);
        let next = first["end_line"].as_u64().unwrap() as usize + 1;
        let last = files.read_range("pages", Some(next), None).unwrap();
        assert_eq!(last["content"], "last\n");
        assert_eq!(last["sha256"], first["sha256"]);
        assert_eq!(last["truncated"], false);
        assert!(
            files
                .read("large")
                .unwrap_err()
                .to_string()
                .contains("Line 1 exceeds")
        );

        let first = files.read("middle").unwrap();
        assert_eq!(first["content"], "first\n");
        assert_eq!(first["end_line"], 1);
        assert_eq!(first["truncated"], true);
        let error = files
            .read_range("middle", Some(2), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Line 2 exceeds"));
        assert!(error.contains(&INLINE_TEXT_LIMIT.to_string()));
        assert_eq!(
            files.read_range("middle", Some(3), None).unwrap()["content"],
            "last\n"
        );
    }

    #[test]
    fn edit_retries_lf_targets_on_crlf_files_and_preserves_exact_matches() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        let crlf = "\u{feff}header\r\nalpha\r\nbeta\r\nalpha\r\nbeta\r\n";
        files.write("crlf", crlf.as_bytes()).unwrap();
        let mut edit = Edit {
            target: "alpha\nbeta".into(),
            replacement: "élan\r\nbonjour\nmerci".into(),
            replace_all: false,
        };
        let sha = sha256_hex(crlf.as_bytes());
        let error = files
            .edit("crlf", std::slice::from_ref(&edit), Some(&sha))
            .unwrap_err();
        assert!(error.to_string().contains("matched 2 occurrences"));
        assert_eq!(files.read("crlf").unwrap()["content"], crlf);
        edit.replace_all = true;
        assert!(
            files
                .edit("crlf", std::slice::from_ref(&edit), Some("stale"))
                .is_err()
        );
        assert_eq!(files.read("crlf").unwrap()["content"], crlf);
        let result = files
            .edit("crlf", std::slice::from_ref(&edit), Some(&sha))
            .unwrap();
        let expected = "\u{feff}header\r\nélan\r\nbonjour\r\nmerci\r\nélan\r\nbonjour\r\nmerci\r\n";
        assert_eq!(result["replacements"], 2);
        assert_eq!(result["sha256"], sha256_hex(expected.as_bytes()));
        assert_eq!(files.read("crlf").unwrap()["content"], expected);

        files
            .write("mixed", b"alpha\nbeta\nalpha\r\nbeta\r\n")
            .unwrap();
        let result = files.edit("mixed", &[edit], None).unwrap();
        assert_eq!(result["replacements"], 1);
        assert_eq!(
            files.read("mixed").unwrap()["content"],
            "élan\r\nbonjour\nmerci\nalpha\r\nbeta\r\n"
        );
    }

    fn assert_find_and_search_paths(files: &Files, directory: &str, expected: &[&str]) {
        let found = files.find_files(directory, None, 100).unwrap();
        let searched = files
            .search_files(directory, "match me", None, true, 100)
            .unwrap();
        for result in [&found, &searched] {
            let paths: Vec<_> = result["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["path"].as_str().unwrap())
                .filter(|path| path.rsplit('/').next() != Some(".gitignore"))
                .collect();
            assert_eq!(paths, expected);
            assert_eq!(result["truncated"], false);
        }
    }

    #[test]
    fn find_and_search_apply_nested_ignore_rules_in_both_modes() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        for dir in ["build", "node_modules", "sibling", "src/deep"] {
            files.mkdir(dir).unwrap();
        }
        files.write(".gitignore", b"*.log\n/root-only.txt\nbuild/\n!build/keep.txt\n!node_modules/keep.txt\nsrc/deep/from-root.txt\n").unwrap();
        files
            .write(
                "src/.gitignore",
                b"!keep.log\n/local.txt\n/deep/from-parent.txt\n*.tmp\n",
            )
            .unwrap();
        files
            .write("src/deep/.gitignore", b"keep.log\n!kept.tmp\n")
            .unwrap();
        for path in [
            "build/keep.txt",
            "node_modules/keep.txt",
            "root-only.txt",
            "sibling/keep.log",
            "src/deep/from-parent.txt",
            "src/deep/from-root.txt",
            "src/deep/keep.log",
            "src/deep/kept.tmp",
            "src/deep/local.txt",
            "src/keep.log",
            "src/local.txt",
            "src/root-only.txt",
        ] {
            files.write(path, b"match me\n").unwrap();
        }
        let yolo = Files::open_yolo(root.path()).unwrap();
        for mode in [&files, &yolo] {
            assert_find_and_search_paths(
                mode,
                ".",
                &[
                    "src/deep/kept.tmp",
                    "src/deep/local.txt",
                    "src/keep.log",
                    "src/root-only.txt",
                ],
            );
        }
        assert_find_and_search_paths(
            &files,
            "./src/deep",
            &["src/deep/kept.tmp", "src/deep/local.txt"],
        );
        assert_find_and_search_paths(
            &yolo,
            "src/deep",
            &["from-parent.txt", "from-root.txt", "kept.tmp", "local.txt"],
        );
    }

    #[test]
    fn oversized_ignore_files_are_skipped_as_a_whole() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        let rules = format!("visible.txt\n#{}", "x".repeat(IGNORE_FILE_LIMIT));
        files.write(".gitignore", rules.as_bytes()).unwrap();
        files.write("visible.txt", b"match me\n").unwrap();
        assert_find_and_search_paths(&files, ".", &["visible.txt"]);
        assert_find_and_search_paths(
            &Files::open_yolo(root.path()).unwrap(),
            ".",
            &["visible.txt"],
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_and_search_skip_symlinked_ignore_files_and_entries() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("rules"), b"visible.txt\n").unwrap();
        std::fs::write(outside.path().join("file.txt"), b"match me\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("rules"), root.path().join(".gitignore"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("file.txt"),
            root.path().join("link.txt"),
        )
        .unwrap();
        let files = Files::open(root.path()).unwrap();
        files.write("visible.txt", b"match me\n").unwrap();
        assert_find_and_search_paths(&files, ".", &["visible.txt"]);
        assert_find_and_search_paths(
            &Files::open_yolo(root.path()).unwrap(),
            ".",
            &["visible.txt"],
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_files_do_not_block_and_writes_preserve_mode() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        let fifo = std::ffi::CString::new(root.path().join("fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(files.read("fifo").is_err());
        files.write("script", b"first").unwrap();
        std::fs::set_permissions(
            root.path().join("script"),
            std::fs::Permissions::from_mode(0o750),
        )
        .unwrap();
        files.write("script", b"second").unwrap();
        assert_eq!(
            std::fs::metadata(root.path().join("script"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
    }
    #[cfg(unix)]
    #[test]
    fn symlink_cannot_escape_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"private needle").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let files = Files::open(root.path()).unwrap();
        assert!(files.read("escape/secret").is_err());
        assert!(files.write("escape/secret", b"bad").is_err());
        assert_eq!(
            files.find_files(".", None, 100).unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            files.search_files(".", "needle", None, false, 100).unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            std::fs::read(outside.path().join("secret")).unwrap(),
            b"private needle"
        );
    }

    #[test]
    fn yolo_mode_allows_host_paths_and_binary_roundtrip() {
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let files = Files::open_yolo(base.path()).unwrap();
        let outside_file = outside.path().join("blob.bin");
        let binary: Vec<u8> = (0u16..=255).map(|b| b as u8).cycle().take(8192).collect();
        files
            .write(outside_file.to_str().unwrap(), &binary)
            .unwrap();
        let read_back = files.read(outside_file.to_str().unwrap()).unwrap();
        assert_eq!(read_back["encoding"], "base64");
        assert_eq!(read_back["bytes"], 8192);
        let decoded = STANDARD
            .decode(read_back["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, binary);
        assert_eq!(
            files.directory(outside.path().to_str().unwrap()).unwrap(),
            outside.path().canonicalize().unwrap()
        );
    }
}
