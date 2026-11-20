//! Self-update shared by `venticode-tui` and `venticode-harness`, included with
//! `#[path]` so both crates ship the same verified code.
//!
//! A release is published to S3 as `manifest.json` plus a detached minisign
//! signature `manifest.json.minisig`. The manifest names, per platform and per
//! binary, a URL (relative to the manifest or absolute) and a sha256:
//!
//! ```json
//! {"version":"0.4.0","artifacts":{"linux-x64":{"venticode-tui":
//!   {"url":"0.4.0/linux-x64/venticode-tui","sha256":"…"}}}}
//! ```
//!
//! Updating is off unless a manifest URL and a minisign public key are known,
//! from (first wins) `VENTICODE_UPDATE_URL` / `VENTICODE_UPDATE_PUBKEY` at run
//! time, an `update.json` (`{"url","pubkey"}`) next to the executable, which
//! the installer writes, or the same variables at build time.
//! `VENTICODE_NO_UPDATE` turns it off. The signature
//! makes a compromised bucket unable to push code; the sha256 pins the binary.

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{future::Future, path::PathBuf, time::Duration};
use url::Url;

const MAX_MANIFEST: usize = 1 << 20;
const MAX_BINARY: usize = 512 << 20;

pub struct Updated {
    pub version: String,
    exe: PathBuf,
}

fn platform() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("macos", "x86_64") => "macos-x64",
        ("macos", "aarch64") => "macos-arm64",
        ("windows", "x86_64") => "windows-x64",
        _ => return None,
    })
}

fn json_field(text: &str, key: &str) -> Option<String> {
    serde_json::from_str::<Value>(text).ok()?[key]
        .as_str()
        .map(str::to_owned)
}

fn setting(name: &str, key: &str, built: Option<&str>) -> Option<String> {
    std::env::var(name)
        .ok()
        .or_else(|| {
            let file = std::env::current_exe().ok()?.parent()?.join("update.json");
            json_field(&std::fs::read_to_string(file).ok()?, key)
        })
        .or_else(|| built.map(str::to_owned))
        .filter(|v| !v.is_empty())
}

fn secure(url: &Url) -> Result<()> {
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Update URL {url} must use HTTPS"
    );
    Ok(())
}

fn verify(manifest: &[u8], signature: &[u8], key: &str) -> Result<()> {
    let key = minisign_verify::PublicKey::from_base64(key)
        .map_err(|e| anyhow::anyhow!("Invalid update public key: {e}"))?;
    let signature = minisign_verify::Signature::decode(std::str::from_utf8(signature)?)
        .map_err(|e| anyhow::anyhow!("Invalid manifest signature: {e}"))?;
    key.verify(manifest, &signature, false)
        .map_err(|e| anyhow::anyhow!("Manifest signature mismatch: {e}"))
}

/// The artifact to install for `component`, or `None` when `current` is up to date.
fn plan(
    manifest: &Value,
    base: &Url,
    platform: &str,
    component: &str,
    current: &str,
) -> Result<Option<(String, Url, String)>> {
    let version = manifest["version"]
        .as_str()
        .context("Manifest has no version")?;
    // Only ever move forward: a replayed older manifest must not downgrade.
    if semver::Version::parse(version)? <= semver::Version::parse(current)? {
        return Ok(None);
    }
    let Some(entry) = manifest["artifacts"][platform][component].as_object() else {
        return Ok(None);
    };
    let url = base.join(
        entry
            .get("url")
            .and_then(Value::as_str)
            .context("Artifact has no url")?,
    )?;
    secure(&url)?;
    let sha = entry
        .get("sha256")
        .and_then(Value::as_str)
        .context("Artifact has no sha256")?;
    Ok(Some((version.to_owned(), url, sha.to_ascii_lowercase())))
}

fn install(bytes: &[u8], sha256: &str) -> Result<PathBuf> {
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == sha256,
        "Downloaded update does not match the manifest sha256"
    );
    let exe = std::env::current_exe()?.canonicalize()?;
    let staged = exe.with_extension("new");
    std::fs::write(&staged, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    // Windows cannot overwrite a running exe but can rename it away.
    #[cfg(windows)]
    let swapped = {
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).and_then(|()| {
            std::fs::rename(&staged, &exe).inspect_err(|_| {
                let _ = std::fs::rename(&old, &exe);
            })
        })
    };
    #[cfg(not(windows))]
    let swapped = std::fs::rename(&staged, &exe);
    if let Err(e) = swapped {
        let _ = std::fs::remove_file(&staged);
        bail!("Cannot replace {}: {e}", exe.display());
    }
    Ok(exe)
}

/// Check the manifest and, if it carries a newer `component`, install it over
/// the running executable. `get` fetches a URL within a timeout.
pub async fn check<F, Fut>(component: &str, current: &str, get: F) -> Result<Option<Updated>>
where
    F: Fn(String, Duration) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    if std::env::var_os("VENTICODE_NO_UPDATE").is_some() {
        return Ok(None);
    }
    let (Some(url), Some(key), Some(platform)) = (
        setting(
            "VENTICODE_UPDATE_URL",
            "url",
            option_env!("VENTICODE_UPDATE_URL"),
        ),
        setting(
            "VENTICODE_UPDATE_PUBKEY",
            "pubkey",
            option_env!("VENTICODE_UPDATE_PUBKEY"),
        ),
        platform(),
    ) else {
        return Ok(None);
    };
    let url = Url::parse(&url).context("Invalid VENTICODE_UPDATE_URL")?;
    secure(&url)?;
    let quick = Duration::from_secs(10);
    let manifest = get(url.to_string(), quick).await?;
    ensure!(
        manifest.len() <= MAX_MANIFEST,
        "Update manifest is too large"
    );
    let signature = get(format!("{url}.minisig"), quick).await?;
    verify(&manifest, &signature, &key)?;
    let manifest: Value = serde_json::from_slice(&manifest)?;
    let Some((version, artifact, sha256)) = plan(&manifest, &url, platform, component, current)?
    else {
        return Ok(None);
    };
    // A release whose binary reports an older version would otherwise loop.
    if std::env::var("VENTICODE_UPDATED").is_ok_and(|v| v == version) {
        return Ok(None);
    }
    let bytes = get(artifact.to_string(), Duration::from_secs(300)).await?;
    ensure!(bytes.len() <= MAX_BINARY, "Update binary is too large");
    let exe = install(&bytes, &sha256)?;
    Ok(Some(Updated { version, exe }))
}

/// Replace this process with the updated binary and the same arguments. Only
/// returns on failure.
pub fn restart(updated: &Updated) -> anyhow::Error {
    let mut command = std::process::Command::new(&updated.exe);
    command
        .args(std::env::args_os().skip(1))
        .env("VENTICODE_UPDATED", &updated.version);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.exec().into()
    }
    #[cfg(not(unix))]
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Test vector from the minisign-verify crate: signs b"test".
    const KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const SIG: &str = "untrusted comment: signature from minisign secret key
RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=
trusted comment: timestamp:1633700835\tfile:test\tprehashed
wLMDjy9FLAuxZ3q4NlEvkgtyhrr0gtTu6KC4KBJdITbbOeAi1zBIYo0v4iTgt8jJpIidRJnp94ABQkJAgAooBQ==";

    #[test]
    fn signature_must_match_the_manifest() {
        assert!(verify(b"test", SIG.as_bytes(), KEY).is_ok());
        assert!(verify(b"tampered", SIG.as_bytes(), KEY).is_err());
        assert!(verify(b"test", b"garbage", KEY).is_err());
    }

    #[test]
    fn plan_only_moves_forward_for_this_platform() {
        let base = Url::parse("https://bucket.example/venticode/manifest.json").unwrap();
        let manifest = json!({"version": "0.4.0", "artifacts": {"linux-x64": {"app":
            {"url": "0.4.0/app", "sha256": "ABC"}}}});
        let (version, url, sha) = plan(&manifest, &base, "linux-x64", "app", "0.3.0")
            .unwrap()
            .unwrap();
        assert_eq!(
            (version.as_str(), url.as_str(), sha.as_str()),
            ("0.4.0", "https://bucket.example/venticode/0.4.0/app", "abc")
        );
        assert!(
            plan(&manifest, &base, "linux-x64", "app", "0.4.0")
                .unwrap()
                .is_none()
        );
        assert!(
            plan(&manifest, &base, "linux-x64", "app", "0.5.0")
                .unwrap()
                .is_none()
        );
        assert!(
            plan(&manifest, &base, "macos-arm64", "app", "0.3.0")
                .unwrap()
                .is_none()
        );
        assert!(
            plan(&manifest, &base, "linux-x64", "other", "0.3.0")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn plan_rejects_plain_http_artifacts() {
        let base = Url::parse("https://bucket.example/manifest.json").unwrap();
        let manifest = json!({"version": "0.4.0", "artifacts": {"linux-x64": {"app":
            {"url": "http://evil.example/app", "sha256": "abc"}}}});
        assert!(plan(&manifest, &base, "linux-x64", "app", "0.3.0").is_err());
    }

    #[test]
    fn config_file_fields_are_read() {
        let text = r#"{"url":"https://a.example/m.json","pubkey":"RWQ"}"#;
        assert_eq!(json_field(text, "pubkey").as_deref(), Some("RWQ"));
        assert_eq!(json_field(text, "missing"), None);
        assert_eq!(json_field("not json", "url"), None);
    }

    #[test]
    fn install_refuses_a_wrong_hash() {
        assert!(install(b"binary", &"0".repeat(64)).is_err());
    }
}
