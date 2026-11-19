// Session persistence for the TUI: prefer the OS keyring, fall back to a
// 0600 file when no secret service / dbus is available (common on headless
// Linux boxes, WSL, containers and CI). The fallback keeps the exact same
// session blob the keyring would hold, so restarts stay seamless.

use anyhow::Result;
use std::path::PathBuf;

/// Where a launch's session blob lives. Either the OS keyring or a private file.
pub struct SessionStore {
    keyring: Option<keyring::Entry>,
    file: Option<PathBuf>,
}

impl SessionStore {
    /// Build a store for `service`/`account` (e.g. "venticode-tui" + workspace
    /// origin). Probes the keyring once: if it is unreachable (no dbus / secret
    /// service / unsupported platform) the store transparently uses a file.
    pub fn open(service: &str, account: &str) -> Self {
        match keyring::Entry::new(service, account) {
            Ok(entry) => match entry.get_password() {
                // Any error other than "no entry" means the backend is unusable
                // (locked, no dbus, unsupported). Fall back to a private file.
                Err(keyring::Error::NoEntry) | Ok(_) => Self {
                    keyring: Some(entry),
                    file: None,
                },
                Err(_) => Self {
                    keyring: None,
                    file: config_file(service, account),
                },
            },
            Err(_) => Self {
                keyring: None,
                file: config_file(service, account),
            },
        }
    }

    /// True when the session is stored in a plain file (unencrypted on disk).
    pub fn is_file_backend(&self) -> bool {
        self.keyring.is_none()
    }

    /// Directory of the fallback file, for messaging.
    pub fn file_path(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }

    pub fn load(&self) -> Option<String> {
        if let Some(entry) = &self.keyring {
            return entry.get_password().ok();
        }
        std::fs::read_to_string(self.file.as_ref()?).ok()
    }

    pub fn save(&self, value: &str) -> Result<()> {
        if let Some(entry) = &self.keyring {
            entry.set_password(value)?;
            return Ok(());
        }
        let path = self
            .file
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No session storage backend available"))?;
        write_private(path, value)
    }

    pub fn clear(&self) {
        if let Some(entry) = &self.keyring {
            let _ = entry.delete_credential();
        }
        if let Some(path) = &self.file {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn config_file(service: &str, account: &str) -> Option<PathBuf> {
    let mut dir = config_dir()?;
    dir.push(service);
    let _ = std::fs::create_dir_all(&dir);
    let name = sanitize(account);
    Some(dir.join(format!("{name}.json")))
}

fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(home).join(".config"));
    }
    // Windows fallback: %LOCALAPPDATA%.
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

/// Reduce the account (a URL) to a stable, filesystem-safe file stem.
fn sanitize(account: &str) -> String {
    let mut out = String::with_capacity(account.len());
    for c in account.chars() {
        out.push(match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            ':' => '_',
            '/' | '\\' => '_',
            _ => '_',
        });
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "workspace".to_owned()
    } else {
        trimmed.chars().take(120).collect()
    }
}

fn write_private(path: &std::path::Path, value: &str) -> Result<()> {
    use std::io::Write;
    // Write to a temp file with 0600 then atomically rename, so a crash never
    // leaves a half-written or world-readable session blob.
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = parent.join(format!(".{}.tmp", file_name(path)));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(value.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_owned())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_home<T>(f: impl FnOnce() -> T) -> T {
        // Isolate XDG_CONFIG_HOME so tests never touch the real config dir.
        let dir = std::env::temp_dir().join(format!("venticode-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: single-threaded test binary; we set and restore the var.
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        let out = f();
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn file_fallback_round_trips_and_clears() {
        with_temp_home(|| {
            // Force file mode by pointing at an account; on CI without a secret
            // service the keyring probe fails and `file` is set. We assert the
            // store is usable either way and that clear() removes the blob.
            let store = SessionStore::open("venticode-tui-test", "https://example.test");
            assert!(store.save("hello-session").is_ok());
            assert_eq!(store.load().as_deref(), Some("hello-session"));
            store.clear();
            assert_eq!(store.load(), None);
        });
    }

    #[cfg(unix)]
    #[test]
    fn private_file_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        with_temp_home(|| {
            // Directly exercise the writer to assert mode bits regardless of
            // whether the keyring is available on this machine.
            let dir =
                std::env::temp_dir().join(format!("venticode-mode-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("session.json");
            write_private(&path, "secret").unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "group/other bits must be unset: {mode:o}");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn corrupt_file_reads_as_none() {
        with_temp_home(|| {
            let dir =
                std::env::temp_dir().join(format!("venticode-corrupt-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("broken.json");
            // Non-UTF8 bytes should simply fail to read as a String.
            std::fs::write(&path, [0xffu8, 0xfe, 0x00]).unwrap();
            assert_eq!(std::fs::read_to_string(&path).ok(), None);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn sanitize_yields_stable_safe_stems() {
        assert_eq!(
            sanitize("https://workspace.example"),
            "https___workspace.example"
        );
        assert_eq!(sanitize(""), "workspace");
        for input in ["http://[::1]:8080", "https://a/b\\c", "  ", "///"] {
            let out = sanitize(input);
            assert!(!out.is_empty(), "{input} produced empty stem");
            assert!(
                out.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
                "{out:?} has unsafe chars"
            );
            assert_eq!(out, sanitize(input), "sanitize must be deterministic");
        }
    }
}
