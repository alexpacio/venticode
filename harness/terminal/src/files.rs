use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use globset::{GlobBuilder, GlobMatcher};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    io::{BufRead, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

pub const FILE_LIMIT: usize = 4 * 1024 * 1024;
pub const INLINE_TEXT_LIMIT: usize = 512 * 1024;
pub const RANGE_FILE_LIMIT: usize = 8 * 1024 * 1024;
const IGNORE_FILE_LIMIT: usize = 64 * 1024;
const SEARCH_FILE_LIMIT: usize = 2 * 1024 * 1024;

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

struct CompiledGlob {
    matcher: GlobMatcher,
    basename_only: bool,
}

impl CompiledGlob {
    fn compile(pattern: &str) -> Result<Option<Self>> {
        let pat = pattern.trim().trim_start_matches("./").trim_matches('/');
        if pat.is_empty() {
            return Ok(None);
        }
        let basename_only = !pat.contains('/');
        let matcher = GlobBuilder::new(pat)
            .literal_separator(true)
            .build()
            .with_context(|| format!("Invalid glob pattern '{pattern}'"))?
            .compile_matcher();
        Ok(Some(Self {
            matcher,
            basename_only,
        }))
    }

    fn is_match(&self, rel_path: &str) -> bool {
        if self.basename_only {
            let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path);
            self.matcher.is_match(file_name)
        } else {
            let normalized = rel_path.trim_start_matches("./").trim_matches('/');
            self.matcher.is_match(normalized)
        }
    }
}

pub fn glob_matches(pattern: &str, rel_path: &str) -> bool {
    match CompiledGlob::compile(pattern) {
        Ok(Some(compiled)) => compiled.is_match(rel_path),
        Ok(None) => true,
        Err(_) => false,
    }
}

/// Read a bounded regular file relative to `root` using `cap_std::fs::Dir` without following
/// symlinks on any directory component or the target file, using `O_NONBLOCK` on Unix.
/// Returns `Ok(None)` if the file or any parent directory does not exist.
pub fn read_sandboxed_nofollow(
    root: &Path,
    rel: &Path,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let base = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => parts.push(seg),
            _ => bail!("Invalid relative config path"),
        }
    }
    let Some((file_name, parent_parts)) = parts.split_last() else {
        bail!("Config path requires a file name");
    };
    let mut current: Option<Dir> = None;
    for dir_name in parent_parts {
        let parent_ref = current.as_ref().unwrap_or(&base);
        match parent_ref.open_dir_nofollow(dir_name) {
            Ok(next) => current = Some(next),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        }
    }
    let parent_ref = current.as_ref().unwrap_or(&base);
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match parent_ref.open_with(file_name, &options) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    ensure!(file.metadata()?.is_file(), "Regular file required");
    let mut bytes = Vec::new();
    file.take((max_bytes + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max_bytes, "Config file exceeds size limit");
    Ok(Some(bytes))
}

/// Read a bounded regular file on the host with `O_NONBLOCK` on Unix.
/// Returns `Ok(None)` if the file does not exist.
pub fn read_host_bounded_file(path: &Path, max_bytes: usize) -> Result<Option<Vec<u8>>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    ensure!(file.metadata()?.is_file(), "Regular file required");
    let mut bytes = Vec::new();
    file.take((max_bytes + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max_bytes, "Config file exceeds size limit");
    Ok(Some(bytes))
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

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn is_transient_walk_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    )
}

fn open_host_regular_file(target: &Path, reject_symlink: bool) -> Result<(std::fs::File, u64)> {
    if reject_symlink {
        let meta = std::fs::symlink_metadata(target)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "Cannot edit through a symlink"
        );
        ensure!(meta.is_file(), "Regular file required");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut flags = libc::O_NONBLOCK;
        if reject_symlink {
            flags |= libc::O_NOFOLLOW;
        }
        options.custom_flags(flags);
    }
    let file = options.open(target)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "Regular file required");
    Ok((file, meta.len()))
}

fn read_host_regular_file(target: &Path, limit: usize, reject_symlink: bool) -> Result<Vec<u8>> {
    let (file, _) = open_host_regular_file(target, reject_symlink)?;
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path)?;
        file.write_all(bytes)?;
        match std::fs::symlink_metadata(target) {
            Ok(metadata) if metadata.is_file() => {
                file.set_permissions(metadata.permissions())?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
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

fn detect_line_ending(text: &str) -> &'static str {
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf = 0usize;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            if i > 0 && bytes[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
    }
    if crlf > 0 && crlf >= lf { "\r\n" } else { "\n" }
}

fn normalize_to_lf(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_owned()
    }
}

struct NormalizedMap {
    text: String,
    orig_starts: Vec<usize>,
    orig_ends: Vec<usize>,
    /// When a character is the last non-whitespace char on a line with stripped trailing spaces,
    /// records the byte offset right after the stripped trailing whitespace.
    line_trailing_end: Vec<usize>,
}

impl NormalizedMap {
    fn lf_only(input: &str) -> Self {
        let mut text = String::with_capacity(input.len());
        let mut orig_starts = Vec::with_capacity(input.len());
        let mut orig_ends = Vec::with_capacity(input.len());
        let mut line_trailing_end = Vec::with_capacity(input.len());
        let mut iter = input.char_indices().peekable();
        while let Some((idx, ch)) = iter.next() {
            let (out_ch, end_idx) = if ch == '\r' {
                if let Some(&(next_idx, '\n')) = iter.peek() {
                    iter.next();
                    ('\n', next_idx + 1)
                } else {
                    ('\n', idx + 1)
                }
            } else {
                (ch, idx + ch.len_utf8())
            };
            let mut buf = [0u8; 4];
            let encoded = out_ch.encode_utf8(&mut buf);
            for _ in encoded.as_bytes() {
                orig_starts.push(idx);
                orig_ends.push(end_idx);
                line_trailing_end.push(end_idx);
            }
            text.push_str(encoded);
        }
        Self {
            text,
            orig_starts,
            orig_ends,
            line_trailing_end,
        }
    }

    fn fuzzy(input: &str) -> Self {
        let mut text = String::with_capacity(input.len());
        let mut orig_starts = Vec::with_capacity(input.len());
        let mut orig_ends = Vec::with_capacity(input.len());
        let mut line_trailing_end = Vec::with_capacity(input.len());

        let bytes = input.as_bytes();
        let mut pos = 0usize;
        while pos < input.len() {
            let line_start = pos;
            let mut line_content_end = pos;
            let mut newline_end = None;
            while pos < bytes.len() {
                if bytes[pos] == b'\r' {
                    line_content_end = pos;
                    if pos + 1 < bytes.len() && bytes[pos + 1] == b'\n' {
                        newline_end = Some(pos + 2);
                        pos += 2;
                    } else {
                        newline_end = Some(pos + 1);
                        pos += 1;
                    }
                    break;
                } else if bytes[pos] == b'\n' {
                    line_content_end = pos;
                    newline_end = Some(pos + 1);
                    pos += 1;
                    break;
                }
                pos += 1;
                line_content_end = pos;
            }

            let line_slice = &input[line_start..line_content_end];
            let trimmed = line_slice.trim_end_matches(is_fuzzy_horizontal_ws);
            let trimmed_orig_end = line_start + trimmed.len();

            for (rel_idx, ch) in trimmed.char_indices() {
                let abs_start = line_start + rel_idx;
                let abs_end = abs_start + ch.len_utf8();
                let mapped = normalize_fuzzy_char(ch);
                let mut buf = [0u8; 4];
                let encoded = mapped.encode_utf8(&mut buf);
                for _ in encoded.as_bytes() {
                    orig_starts.push(abs_start);
                    orig_ends.push(abs_end);
                    line_trailing_end.push(abs_end);
                }
                text.push_str(encoded);
            }

            // If this line had trailing whitespace stripped and at least one char was emitted,
            // record the end of the stripped trailing whitespace on the final byte of the line.
            if line_content_end > trimmed_orig_end
                && !trimmed.is_empty()
                && let Some(last) = line_trailing_end.last_mut()
            {
                *last = line_content_end;
            }

            if let Some(nl_end) = newline_end {
                text.push('\n');
                orig_starts.push(line_content_end);
                orig_ends.push(nl_end);
                line_trailing_end.push(nl_end);
            }
        }

        Self {
            text,
            orig_starts,
            orig_ends,
            line_trailing_end,
        }
    }

    fn map_span(&self, norm_start: usize, norm_end: usize) -> (usize, usize) {
        let orig_start = self.orig_starts[norm_start];
        let last_idx = norm_end - 1;
        let at_line_end = norm_end == self.text.len() || self.text.as_bytes()[norm_end] == b'\n';
        let orig_end = if at_line_end {
            self.line_trailing_end[last_idx]
        } else {
            self.orig_ends[last_idx]
        };
        (orig_start, orig_end)
    }
}

fn is_fuzzy_horizontal_ws(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

fn normalize_fuzzy_char(ch: char) -> char {
    match ch {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        other => other,
    }
}

fn apply_spans(content: &str, spans: &[(usize, usize)], replacement: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    for &(start, end) in spans {
        out.push_str(&content[cursor..start]);
        out.push_str(replacement);
        cursor = end;
    }
    out.push_str(&content[cursor..]);
    out
}

fn stream_read_range(reader_src: Box<dyn Read>, req_start: usize, req_end: usize) -> Result<Value> {
    let mut reader = BufReader::with_capacity(64 * 1024, reader_src);
    let mut current_line = 1usize;
    let mut saw_any_bytes_on_line = false;

    while current_line < req_start {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            let total_lines = if saw_any_bytes_on_line {
                current_line
            } else {
                current_line - 1
            };
            return Ok(json!({
                "encoding": "utf8",
                "content": "",
                "sha256": Value::Null,
                "total_lines": total_lines,
                "start_line": req_start,
                "end_line": 0,
                "truncated": false
            }));
        }
        ensure!(
            memchr::memchr(0, chunk).is_none(),
            "Line-range read requires UTF-8 text file / Lettura per righe richiede file UTF-8"
        );
        let mut consumed = 0usize;
        while current_line < req_start && consumed < chunk.len() {
            if let Some(pos) = memchr::memchr(b'\n', &chunk[consumed..]) {
                consumed += pos + 1;
                current_line += 1;
                saw_any_bytes_on_line = false;
            } else {
                consumed = chunk.len();
                saw_any_bytes_on_line = true;
            }
        }
        reader.consume(consumed);
    }

    let mut out = String::new();
    let mut actual_end = 0usize;
    let mut truncated = false;
    let mut reached_eof = false;
    let mut line_buf = Vec::new();
    let mut line_no = req_start;

    while line_no <= req_end {
        line_buf.clear();
        let n = (&mut reader)
            .take((INLINE_TEXT_LIMIT + 1) as u64)
            .read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            reached_eof = true;
            break;
        }
        ensure!(
            memchr::memchr(0, &line_buf).is_none(),
            "Line-range read requires UTF-8 text file / Lettura per righe richiede file UTF-8"
        );
        let line_str = std::str::from_utf8(&line_buf).context(
            "Line-range read requires UTF-8 text file / Lettura per righe richiede file UTF-8",
        )?;
        if out.len() + line_str.len() > INLINE_TEXT_LIMIT {
            ensure!(
                actual_end != 0,
                "Line {req_start} exceeds the {INLINE_TEXT_LIMIT}-byte inline read limit"
            );
            truncated = true;
            break;
        }
        out.push_str(line_str);
        actual_end = line_no;
        line_no += 1;
    }

    Ok(json!({
        "encoding": "utf8",
        "content": out,
        "sha256": Value::Null,
        "total_lines": if reached_eof { json!(line_no - 1) } else { Value::Null },
        "start_line": req_start,
        "end_line": actual_end,
        "truncated": truncated
    }))
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

    fn open_file_handle(&self, path: &str, reject_symlink: bool) -> Result<(Box<dyn Read>, u64)> {
        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => {
                let rel = relative(path)?;
                if reject_symlink {
                    let meta = dir.symlink_metadata(rel)?;
                    ensure!(
                        !meta.file_type().is_symlink(),
                        "Cannot edit through a symlink"
                    );
                    ensure!(meta.is_file(), "Regular file required");
                }
                let mut options = OpenOptions::new();
                options.read(true);
                if reject_symlink {
                    options.follow(FollowSymlinks::No);
                }
                #[cfg(unix)]
                {
                    use cap_std::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NONBLOCK);
                }
                let file = dir.open_with(rel, &options)?;
                let meta = file.metadata()?;
                ensure!(meta.is_file(), "Regular file required");
                Ok((Box::new(file), meta.len()))
            }
            FilesInner::Yolo { base } => {
                let (file, len) =
                    open_host_regular_file(&yolo_resolve(base, path)?, reject_symlink)?;
                Ok((Box::new(file), len))
            }
        }
    }

    fn open_regular_file(&self, path: &str, limit: usize, reject_symlink: bool) -> Result<Vec<u8>> {
        let (file, _) = self.open_file_handle(path, reject_symlink)?;
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

    pub fn list(&self, path: &str) -> Result<Value> {
        let mut items = Vec::new();
        match &self.inner {
            FilesInner::Sandboxed { dir, .. } => {
                for entry in dir.open_dir(relative(path)?)?.entries()? {
                    ensure!(
                        items.len() < 4096,
                        "Too many entries; choose a narrower directory"
                    );
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(err) => return Err(err.into()),
                    };
                    let ft = match entry.file_type() {
                        Ok(ft) => ft,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(err) => return Err(err.into()),
                    };
                    let metadata = match entry.metadata() {
                        Ok(meta) => meta,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(err) => return Err(err.into()),
                    };
                    items.push(json!({
                        "name": entry.file_name().to_string_lossy(),
                        "directory": metadata.is_dir(),
                        "symlink": ft.is_symlink(),
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
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(err) => return Err(err.into()),
                    };
                    let ft = match entry.file_type() {
                        Ok(ft) => ft,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(err) => return Err(err.into()),
                    };
                    let Ok(metadata) = entry
                        .metadata()
                        .or_else(|_| std::fs::symlink_metadata(entry.path()))
                    else {
                        continue;
                    };
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
        if let Some(start) = start_line {
            ensure!(start >= 1, "start_line must be >= 1");
        }
        if let Some(end) = end_line {
            ensure!(end >= 1, "end_line must be >= 1");
        }
        if let (Some(start), Some(end)) = (start_line, end_line) {
            ensure!(
                end >= start,
                "end_line ({end}) must be >= start_line ({start})"
            );
        }
        let has_range = start_line.is_some() || end_line.is_some();
        let req_start = start_line.unwrap_or(1);
        let req_end = end_line.unwrap_or(usize::MAX);

        let (file, file_len) = self.open_file_handle(path, false)?;
        if has_range && file_len > RANGE_FILE_LIMIT as u64 {
            return stream_read_range(file, req_start, req_end);
        }

        let limit = if has_range {
            RANGE_FILE_LIMIT
        } else {
            FILE_LIMIT
        };
        let mut bytes = Vec::new();
        file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            if has_range {
                // File grew past RANGE_FILE_LIMIT while reading; reopen and stream.
                let (stream_file, _) = self.open_file_handle(path, false)?;
                return stream_read_range(stream_file, req_start, req_end);
            }
            bail!("File exceeds 4 MiB");
        }

        let digest = sha256_hex(&bytes);
        let is_binary = memchr::memchr(0, &bytes).is_some();
        let text_opt = if is_binary {
            None
        } else {
            String::from_utf8(bytes.clone()).ok()
        };

        match text_opt {
            Some(full) => {
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
            None => {
                ensure!(
                    !has_range,
                    "Line-range read requires UTF-8 text file / Lettura per righe richiede file UTF-8"
                );
                Ok(json!({
                    "encoding": "base64",
                    "content": STANDARD.encode(&bytes),
                    "bytes": bytes.len(),
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
                    #[cfg(unix)]
                    {
                        use cap_std::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    let mut file = parent.open_with(&temporary, &options)?;
                    file.write_all(bytes)?;
                    match parent.symlink_metadata(name) {
                        Ok(metadata) if metadata.is_file() => {
                            file.set_permissions(metadata.permissions())?
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
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
        let bytes = self.open_regular_file(path, RANGE_FILE_LIMIT, true)?;
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
            // Tier 1: exact match (preserves verbatim replacement and mixed line endings).
            let exact_count = content.matches(&item.target).count();
            if exact_count > 0 {
                if !item.replace_all && exact_count > 1 {
                    bail!(
                        "Edit #{} target matched {} occurrences in {}; provide more context or set replace_all=true",
                        idx + 1,
                        exact_count,
                        path
                    );
                }
                content = content.replace(&item.target, &item.replacement);
                total_replacements += exact_count;
                continue;
            }

            // Tier 2: line-ending normalized match (detect -> normalize_to_lf -> restore_line_endings).
            let ending = detect_line_ending(&content);
            let lf_map = NormalizedMap::lf_only(&content);
            let lf_target = normalize_to_lf(&item.target);
            let lf_spans: Vec<(usize, usize)> = if !lf_target.is_empty() {
                lf_map
                    .text
                    .match_indices(&lf_target)
                    .map(|(start, _)| lf_map.map_span(start, start + lf_target.len()))
                    .collect()
            } else {
                Vec::new()
            };
            if !lf_spans.is_empty() {
                if !item.replace_all && lf_spans.len() > 1 {
                    bail!(
                        "Edit #{} target matched {} occurrences in {}; provide more context or set replace_all=true",
                        idx + 1,
                        lf_spans.len(),
                        path
                    );
                }
                let restored = restore_line_endings(&normalize_to_lf(&item.replacement), ending);
                content = apply_spans(&content, &lf_spans, &restored);
                total_replacements += lf_spans.len();
                continue;
            }

            // Tier 3: fuzzy normalized match (trailing ws, smart quotes, Unicode dashes, NBSP).
            let fuzzy_map = NormalizedMap::fuzzy(&content);
            let fuzzy_target = NormalizedMap::fuzzy(&item.target).text;
            let fuzzy_spans: Vec<(usize, usize)> = if !fuzzy_target.is_empty() {
                fuzzy_map
                    .text
                    .match_indices(&fuzzy_target)
                    .map(|(start, _)| fuzzy_map.map_span(start, start + fuzzy_target.len()))
                    .collect()
            } else {
                Vec::new()
            };
            if fuzzy_spans.is_empty() {
                bail!("Edit #{} target not found in {}", idx + 1, path);
            }
            if !item.replace_all && fuzzy_spans.len() > 1 {
                bail!(
                    "Edit #{} target matched {} occurrences in {}; provide more context or set replace_all=true",
                    idx + 1,
                    fuzzy_spans.len(),
                    path
                );
            }
            let restored = restore_line_endings(&normalize_to_lf(&item.replacement), ending);
            content = apply_spans(&content, &fuzzy_spans, &restored);
            total_replacements += fuzzy_spans.len();
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
        let compiled_glob = match pattern.filter(|p| !p.trim().is_empty()) {
            Some(pat) => CompiledGlob::compile(pat)?,
            None => None,
        };
        let mut items = Vec::new();
        let mut truncated = false;
        let mut visited_dirs = 0usize;

        enum FindWork {
            PopIgnore(usize),
            Dir {
                dir_path: PathBuf,
                prefix: String,
                is_initial: bool,
            },
            File {
                rel: String,
                bytes: u64,
            },
        }

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
                drop(start_dir);

                let mut stack = vec![FindWork::Dir {
                    dir_path: if base_prefix.is_empty() {
                        PathBuf::new()
                    } else {
                        PathBuf::from(&base_prefix)
                    },
                    prefix: base_prefix,
                    is_initial: true,
                }];

                while let Some(work) = stack.pop() {
                    match work {
                        FindWork::PopIgnore(depth) => ignore.0.truncate(depth),
                        FindWork::File { rel, bytes } => {
                            if items.len() >= max_results {
                                truncated = true;
                                break;
                            }
                            items.push(json!({"path": rel, "bytes": bytes}));
                        }
                        FindWork::Dir {
                            dir_path,
                            prefix,
                            is_initial,
                        } => {
                            visited_dirs += 1;
                            if visited_dirs > 4096 {
                                truncated = true;
                                break;
                            }
                            let opened_holder;
                            let current_dir: &Dir = if dir_path.as_os_str().is_empty() {
                                dir
                            } else {
                                if !is_initial {
                                    match dir.symlink_metadata(&dir_path) {
                                        Ok(meta) if !meta.file_type().is_symlink() => {}
                                        _ => continue,
                                    }
                                }
                                match dir.open_dir(&dir_path) {
                                    Ok(d) => {
                                        opened_holder = d;
                                        &opened_holder
                                    }
                                    Err(err) if !is_initial && is_transient_walk_error(&err) => {
                                        continue;
                                    }
                                    Err(_) if !is_initial => continue,
                                    Err(err) => return Err(err.into()),
                                }
                            };

                            let prev_depth = ignore.0.len();
                            if !is_initial {
                                ignore.push(
                                    Path::new(&prefix),
                                    IgnoreRules::read_matcher(current_dir),
                                );
                            }

                            let rd = match current_dir.entries() {
                                Ok(rd) => rd,
                                Err(err) if !is_initial && is_transient_walk_error(&err) => {
                                    ignore.0.truncate(prev_depth);
                                    continue;
                                }
                                Err(err) => return Err(err.into()),
                            };
                            let mut entries = Vec::new();
                            for entry in rd {
                                let entry = match entry {
                                    Ok(e) => e,
                                    Err(err) if is_transient_walk_error(&err) => continue,
                                    Err(err) => return Err(err.into()),
                                };
                                let ft = match entry.file_type() {
                                    Ok(ft) => ft,
                                    Err(err) if is_transient_walk_error(&err) => continue,
                                    Err(err) => return Err(err.into()),
                                };
                                if ft.is_symlink() {
                                    continue;
                                }
                                let name = entry.file_name().to_string_lossy().into_owned();
                                let bytes = if ft.is_file() {
                                    entry.metadata().map(|m| m.len()).unwrap_or(0)
                                } else {
                                    0
                                };
                                entries.push((name, ft.is_dir(), ft.is_file(), bytes));
                            }
                            entries.sort_by(|a, b| a.0.cmp(&b.0));

                            if ignore.0.len() > prev_depth {
                                stack.push(FindWork::PopIgnore(prev_depth));
                            }
                            for (name, is_dir, is_file, bytes) in entries.into_iter().rev() {
                                let rel = if prefix.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{prefix}/{name}")
                                };
                                if ignore.is_ignored(&rel, &name, is_dir) {
                                    continue;
                                }
                                if is_dir {
                                    let child_path = if dir_path.as_os_str().is_empty() {
                                        PathBuf::from(&name)
                                    } else {
                                        dir_path.join(&name)
                                    };
                                    stack.push(FindWork::Dir {
                                        dir_path: child_path,
                                        prefix: rel,
                                        is_initial: false,
                                    });
                                } else if is_file
                                    && compiled_glob.as_ref().is_none_or(|g| g.is_match(&rel))
                                {
                                    stack.push(FindWork::File { rel, bytes });
                                }
                            }
                        }
                    }
                }
            }
            FilesInner::Yolo { base } => {
                let start_dir = yolo_resolve(base, path)?;
                ensure!(start_dir.is_dir(), "Directory required");
                let mut ignore = IgnoreRules::default();
                ignore.push(Path::new(""), IgnoreRules::read_host_matcher(&start_dir));

                let mut stack = vec![FindWork::Dir {
                    dir_path: start_dir,
                    prefix: String::new(),
                    is_initial: true,
                }];

                while let Some(work) = stack.pop() {
                    match work {
                        FindWork::PopIgnore(depth) => ignore.0.truncate(depth),
                        FindWork::File { rel, bytes } => {
                            if items.len() >= max_results {
                                truncated = true;
                                break;
                            }
                            items.push(json!({"path": rel, "bytes": bytes}));
                        }
                        FindWork::Dir {
                            dir_path,
                            prefix,
                            is_initial,
                        } => {
                            visited_dirs += 1;
                            if visited_dirs > 4096 {
                                truncated = true;
                                break;
                            }
                            let Ok(rd) = std::fs::read_dir(&dir_path) else {
                                continue;
                            };
                            let prev_depth = ignore.0.len();
                            if !is_initial {
                                ignore.push(
                                    Path::new(&prefix),
                                    IgnoreRules::read_host_matcher(&dir_path),
                                );
                            }
                            let mut entries = Vec::new();
                            for entry in rd {
                                let Ok(entry) = entry else { continue };
                                let Ok(ft) = entry.file_type() else { continue };
                                if ft.is_symlink() {
                                    continue;
                                }
                                let name = entry.file_name().to_string_lossy().into_owned();
                                let full_path = entry.path();
                                let bytes = if ft.is_file() {
                                    std::fs::metadata(&full_path).map(|m| m.len()).unwrap_or(0)
                                } else {
                                    0
                                };
                                entries.push((name, full_path, ft.is_dir(), ft.is_file(), bytes));
                            }
                            entries.sort_by(|a, b| a.0.cmp(&b.0));

                            if ignore.0.len() > prev_depth {
                                stack.push(FindWork::PopIgnore(prev_depth));
                            }
                            for (name, full_path, is_dir, is_file, bytes) in
                                entries.into_iter().rev()
                            {
                                let rel = if prefix.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{prefix}/{name}")
                                };
                                if ignore.is_ignored(&rel, &name, is_dir) {
                                    continue;
                                }
                                if is_dir {
                                    stack.push(FindWork::Dir {
                                        dir_path: full_path,
                                        prefix: rel,
                                        is_initial: false,
                                    });
                                } else if is_file
                                    && compiled_glob.as_ref().is_none_or(|g| g.is_match(&rel))
                                {
                                    stack.push(FindWork::File { rel, bytes });
                                }
                            }
                        }
                    }
                }
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
        use_regex: bool,
        max_results: usize,
    ) -> Result<Value> {
        ensure!(!query.is_empty(), "Search query must not be empty");
        let max_results = max_results.clamp(1, 1000);
        let compiled_glob = match pattern.filter(|p| !p.trim().is_empty()) {
            Some(pat) => CompiledGlob::compile(pat)?,
            None => None,
        };
        let pattern_str = if use_regex {
            Cow::Borrowed(query)
        } else {
            Cow::Owned(regex::escape(query))
        };
        let matcher = RegexBuilder::new(&pattern_str)
            .case_insensitive(!case_sensitive)
            .size_limit(2 * 1024 * 1024)
            .dfa_size_limit(2 * 1024 * 1024)
            .build()
            .context("Invalid search pattern")?;

        let mut matches = Vec::new();
        let mut truncated = false;
        let mut visited_dirs = 0usize;

        enum SearchWork {
            PopIgnore(usize),
            Dir {
                dir_path: PathBuf,
                prefix: String,
                is_initial: bool,
            },
            File {
                file_path: PathBuf,
                rel: String,
            },
        }

        let mut scan_buffer = |rel: &str, buf: Vec<u8>| -> bool {
            if memchr::memchr(0, &buf).is_some() {
                return false;
            }
            let Ok(text) = String::from_utf8(buf) else {
                return false;
            };
            for (idx, line) in text.lines().enumerate() {
                if let Some(m) = matcher.find(line) {
                    if matches.len() >= max_results {
                        return true;
                    }
                    let col = line[..m.start()].chars().count() + 1;
                    let preview: String = line.chars().take(300).collect();
                    matches.push(json!({
                        "path": rel,
                        "line": idx + 1,
                        "column": col,
                        "text": preview
                    }));
                }
            }
            false
        };

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
                drop(start_dir);

                let mut stack = vec![SearchWork::Dir {
                    dir_path: if base_prefix.is_empty() {
                        PathBuf::new()
                    } else {
                        PathBuf::from(&base_prefix)
                    },
                    prefix: base_prefix,
                    is_initial: true,
                }];

                while let Some(work) = stack.pop() {
                    match work {
                        SearchWork::PopIgnore(depth) => ignore.0.truncate(depth),
                        SearchWork::File { file_path, rel } => {
                            let Ok(meta) = dir.symlink_metadata(&file_path) else {
                                continue;
                            };
                            if meta.file_type().is_symlink()
                                || !meta.is_file()
                                || meta.len() > SEARCH_FILE_LIMIT as u64
                            {
                                continue;
                            }
                            let mut options = OpenOptions::new();
                            options.read(true).follow(FollowSymlinks::No);
                            #[cfg(unix)]
                            {
                                use cap_std::fs::OpenOptionsExt;
                                options.custom_flags(libc::O_NONBLOCK);
                            }
                            let Ok(file) = dir.open_with(&file_path, &options) else {
                                continue;
                            };
                            let mut buf = Vec::new();
                            if file
                                .take(SEARCH_FILE_LIMIT as u64)
                                .read_to_end(&mut buf)
                                .is_err()
                            {
                                continue;
                            }
                            if scan_buffer(&rel, buf) {
                                truncated = true;
                                break;
                            }
                        }
                        SearchWork::Dir {
                            dir_path,
                            prefix,
                            is_initial,
                        } => {
                            visited_dirs += 1;
                            if visited_dirs > 4096 {
                                truncated = true;
                                break;
                            }
                            let opened_holder;
                            let current_dir: &Dir = if dir_path.as_os_str().is_empty() {
                                dir
                            } else {
                                if !is_initial {
                                    match dir.symlink_metadata(&dir_path) {
                                        Ok(meta) if !meta.file_type().is_symlink() => {}
                                        _ => continue,
                                    }
                                }
                                match dir.open_dir(&dir_path) {
                                    Ok(d) => {
                                        opened_holder = d;
                                        &opened_holder
                                    }
                                    Err(err) if !is_initial && is_transient_walk_error(&err) => {
                                        continue;
                                    }
                                    Err(_) if !is_initial => continue,
                                    Err(err) => return Err(err.into()),
                                }
                            };

                            let prev_depth = ignore.0.len();
                            if !is_initial {
                                ignore.push(
                                    Path::new(&prefix),
                                    IgnoreRules::read_matcher(current_dir),
                                );
                            }

                            let rd = match current_dir.entries() {
                                Ok(rd) => rd,
                                Err(err) if !is_initial && is_transient_walk_error(&err) => {
                                    ignore.0.truncate(prev_depth);
                                    continue;
                                }
                                Err(err) => return Err(err.into()),
                            };
                            let mut entries = Vec::new();
                            for entry in rd {
                                let entry = match entry {
                                    Ok(e) => e,
                                    Err(err) if is_transient_walk_error(&err) => continue,
                                    Err(err) => return Err(err.into()),
                                };
                                let ft = match entry.file_type() {
                                    Ok(ft) => ft,
                                    Err(err) if is_transient_walk_error(&err) => continue,
                                    Err(err) => return Err(err.into()),
                                };
                                if ft.is_symlink() {
                                    continue;
                                }
                                let name = entry.file_name().to_string_lossy().into_owned();
                                entries.push((name, ft.is_dir(), ft.is_file()));
                            }
                            entries.sort_by(|a, b| a.0.cmp(&b.0));

                            if ignore.0.len() > prev_depth {
                                stack.push(SearchWork::PopIgnore(prev_depth));
                            }
                            for (name, is_dir, is_file) in entries.into_iter().rev() {
                                let rel = if prefix.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{prefix}/{name}")
                                };
                                if ignore.is_ignored(&rel, &name, is_dir) {
                                    continue;
                                }
                                let child_path = if dir_path.as_os_str().is_empty() {
                                    PathBuf::from(&name)
                                } else {
                                    dir_path.join(&name)
                                };
                                if is_dir {
                                    stack.push(SearchWork::Dir {
                                        dir_path: child_path,
                                        prefix: rel,
                                        is_initial: false,
                                    });
                                } else if is_file
                                    && compiled_glob.as_ref().is_none_or(|g| g.is_match(&rel))
                                {
                                    stack.push(SearchWork::File {
                                        file_path: child_path,
                                        rel,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            FilesInner::Yolo { base } => {
                let start_dir = yolo_resolve(base, path)?;
                ensure!(start_dir.is_dir(), "Directory required");
                let mut ignore = IgnoreRules::default();
                ignore.push(Path::new(""), IgnoreRules::read_host_matcher(&start_dir));

                let mut stack = vec![SearchWork::Dir {
                    dir_path: start_dir,
                    prefix: String::new(),
                    is_initial: true,
                }];

                while let Some(work) = stack.pop() {
                    match work {
                        SearchWork::PopIgnore(depth) => ignore.0.truncate(depth),
                        SearchWork::File { file_path, rel } => {
                            let Ok(meta) = std::fs::symlink_metadata(&file_path) else {
                                continue;
                            };
                            if meta.file_type().is_symlink()
                                || !meta.is_file()
                                || meta.len() > SEARCH_FILE_LIMIT as u64
                            {
                                continue;
                            }
                            let Ok(buf) =
                                read_host_regular_file(&file_path, SEARCH_FILE_LIMIT, true)
                            else {
                                continue;
                            };
                            if scan_buffer(&rel, buf) {
                                truncated = true;
                                break;
                            }
                        }
                        SearchWork::Dir {
                            dir_path,
                            prefix,
                            is_initial,
                        } => {
                            visited_dirs += 1;
                            if visited_dirs > 4096 {
                                truncated = true;
                                break;
                            }
                            let Ok(rd) = std::fs::read_dir(&dir_path) else {
                                continue;
                            };
                            let prev_depth = ignore.0.len();
                            if !is_initial {
                                ignore.push(
                                    Path::new(&prefix),
                                    IgnoreRules::read_host_matcher(&dir_path),
                                );
                            }
                            let mut entries = Vec::new();
                            for entry in rd {
                                let Ok(entry) = entry else { continue };
                                let Ok(ft) = entry.file_type() else { continue };
                                if ft.is_symlink() {
                                    continue;
                                }
                                let name = entry.file_name().to_string_lossy().into_owned();
                                entries.push((name, entry.path(), ft.is_dir(), ft.is_file()));
                            }
                            entries.sort_by(|a, b| a.0.cmp(&b.0));

                            if ignore.0.len() > prev_depth {
                                stack.push(SearchWork::PopIgnore(prev_depth));
                            }
                            for (name, full_path, is_dir, is_file) in entries.into_iter().rev() {
                                let rel = if prefix.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{prefix}/{name}")
                                };
                                if ignore.is_ignored(&rel, &name, is_dir) {
                                    continue;
                                }
                                if is_dir {
                                    stack.push(SearchWork::Dir {
                                        dir_path: full_path,
                                        prefix: rel,
                                        is_initial: false,
                                    });
                                } else if is_file
                                    && compiled_glob.as_ref().is_none_or(|g| g.is_match(&rel))
                                {
                                    stack.push(SearchWork::File {
                                        file_path: full_path,
                                        rel,
                                    });
                                }
                            }
                        }
                    }
                }
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
                let normalized = normalize_lexical(&target);
                ensure!(
                    normalized != *base
                        && normalized != Path::new("/")
                        && normalized.parent().is_some(),
                    "Cannot remove base or filesystem root"
                );
                let meta = std::fs::symlink_metadata(&target)?;
                if !meta.file_type().is_symlink()
                    && let Ok(canonical) = target.canonicalize()
                {
                    ensure!(
                        canonical != *base
                            && canonical != Path::new("/")
                            && canonical.parent().is_some(),
                        "Cannot remove base or filesystem root"
                    );
                }
                if meta.is_dir() {
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
            .search_files(".", "NEEDLE_UPDATED", None, false, false, 100)
            .unwrap();
        let hits = searched["items"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["path"], "src/nested/lib.rs");
        assert_eq!(hits[0]["line"], 2);
        assert_eq!(hits[0]["column"], 7);
    }

    #[test]
    fn glob_is_linear_time_and_supports_braces_and_ranges() {
        let evil_pattern = "**/**/**/**/**/**/**/**/**/**/**/**/**/**/**/**/target.rs";
        let deep_path = "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/u/v/w/x/y/z/other.txt";
        let started = std::time::Instant::now();
        assert!(!glob_matches(evil_pattern, deep_path));
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert!(glob_matches("**/*.{ts,tsx}", "src/app/Page.tsx"));
        assert!(!glob_matches("**/*.{ts,tsx}", "src/app/Page.js"));
        assert!(glob_matches("file_[a-z].txt", "dir/file_k.txt"));
        assert!(!glob_matches("file_[a-z].txt", "dir/file_9.txt"));
    }

    #[test]
    fn search_files_reports_utf8_character_columns_and_supports_regex() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        files
            .write("utf8.txt", "café naïve TARGET_123\nfoo bar\n".as_bytes())
            .unwrap();
        // "café naïve " is 11 Unicode chars (13 UTF-8 bytes).
        let literal = files
            .search_files(".", "target_123", None, false, false, 50)
            .unwrap();
        let hits = literal["items"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["line"], 1);
        assert_eq!(hits[0]["column"], 12);

        let regex_res = files
            .search_files(".", r"target_\d+", None, false, true, 50)
            .unwrap();
        let regex_hits = regex_res["items"].as_array().unwrap();
        assert_eq!(regex_hits.len(), 1);
        assert_eq!(regex_hits[0]["column"], 12);

        // Literal mode treats regex metacharacters as literals
        let no_hit = files
            .search_files(".", r"target_\d+", None, false, false, 50)
            .unwrap();
        assert!(no_hit["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn read_range_validates_bounds_and_streams_oversized_files() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        files.write("small.txt", b"a\nb\nc\n").unwrap();
        assert!(files.read_range("small.txt", Some(0), Some(2)).is_err());
        assert!(files.read_range("small.txt", Some(3), Some(2)).is_err());
        assert!(files.read_range("small.txt", None, Some(0)).is_err());

        // Write a > 8 MiB file directly on disk to test streaming range read.
        let big_path = root.path().join("big.log");
        {
            let mut f = std::fs::File::create(&big_path).unwrap();
            let line = format!("{}\n", "x".repeat(1023));
            for i in 1..=8500usize {
                if i == 5000 {
                    f.write_all(b"target-line-5000\n").unwrap();
                } else {
                    f.write_all(line.as_bytes()).unwrap();
                }
            }
        }
        assert!(std::fs::metadata(&big_path).unwrap().len() > RANGE_FILE_LIMIT as u64);
        let streamed = files.read_range("big.log", Some(5000), Some(5000)).unwrap();
        assert_eq!(streamed["content"], "target-line-5000\n");
        assert!(streamed["sha256"].is_null());
        assert_eq!(streamed["start_line"], 5000);
        assert_eq!(streamed["end_line"], 5000);

        // File with NUL byte is rejected for line-range read and returned as base64 for full read
        files.write("nul.bin", b"hello\0world\n").unwrap();
        assert!(files.read_range("nul.bin", Some(1), Some(1)).is_err());
        assert_eq!(files.read("nul.bin").unwrap()["encoding"], "base64");
    }

    #[test]
    fn edit_supports_fuzzy_match_and_optimistic_sha256() {
        let root = tempfile::tempdir().unwrap();
        let files = Files::open(root.path()).unwrap();
        let original = "prefix\r\nsay “hello”—world\u{00A0}now   \r\nsuffix\r\n";
        files.write("doc.txt", original.as_bytes()).unwrap();
        let sha = sha256_hex(original.as_bytes());

        // Fuzzy match handles smart quotes, em-dash, NBSP, and trailing whitespace while restoring CRLF
        let edited = files
            .edit(
                "doc.txt",
                &[Edit {
                    target: "say \"hello\"-world now".into(),
                    replacement: "say \"hi\"-there\nnext".into(),
                    replace_all: false,
                }],
                Some(&sha),
            )
            .unwrap();
        assert_eq!(edited["replacements"], 1);
        assert_eq!(
            files.read("doc.txt").unwrap()["content"],
            "prefix\r\nsay \"hi\"-there\r\nnext\r\nsuffix\r\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn edit_rejects_symlinks_in_both_modes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("real.txt"), b"hello world\n").unwrap();
        std::os::unix::fs::symlink(root.path().join("real.txt"), root.path().join("link.txt"))
            .unwrap();
        let edit = [Edit {
            target: "hello".into(),
            replacement: "bye".into(),
            replace_all: false,
        }];
        let sandboxed = Files::open(root.path()).unwrap();
        assert!(sandboxed.edit("link.txt", &edit, None).is_err());
        let yolo = Files::open_yolo(root.path()).unwrap();
        assert!(yolo.edit("link.txt", &edit, None).is_err());
        assert!(
            std::fs::symlink_metadata(root.path().join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("real.txt")).unwrap(),
            "hello world\n"
        );
    }

    #[test]
    fn yolo_remove_cannot_bypass_base_with_dotdot() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir(base.path().join("sub")).unwrap();
        let files = Files::open_yolo(base.path()).unwrap();
        assert!(files.remove("sub/..").is_err());
        assert!(files.remove(".").is_err());
        assert!(files.remove("/").is_err());
        assert!(base.path().exists());
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
            .search_files(directory, "match me", None, true, false, 100)
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
            files
                .search_files(".", "needle", None, false, false, 100)
                .unwrap()["items"]
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
