use anyhow::{Context, Result};
use std::{env, path::PathBuf};

pub const CREDENTIAL_SERVICE: &str = "venticode-harness-mcp";

pub fn command(name: &str) -> Result<tokio::process::Command> {
    Ok(tokio::process::Command::new(which::which(name)?))
}

pub fn directory() -> Result<PathBuf> {
    let path = if let Some(path) = env::var_os("VENTICODE_HARNESS_CONFIG_DIR") {
        PathBuf::from(path)
    } else if let Some(path) = env::var_os(if cfg!(windows) {
        "APPDATA"
    } else {
        "XDG_CONFIG_HOME"
    }) {
        PathBuf::from(path).join("venticode-harness")
    } else {
        PathBuf::from(env::var_os("HOME").context("Set VENTICODE_HARNESS_CONFIG_DIR")?)
            .join(".config/venticode-harness")
    };
    std::fs::create_dir_all(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}
