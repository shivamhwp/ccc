//! Filesystem layout for ccc and the Claude Code config it integrates with.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// Home directory of the invoking user.
pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

/// Root of ccc state: `~/.ccc`.
pub fn ccc_dir() -> Result<PathBuf> {
    Ok(home()?.join(".ccc"))
}

/// Account + token store: `~/.ccc/store.json`.
pub fn store_file() -> Result<PathBuf> {
    Ok(ccc_dir()?.join("store.json"))
}

/// Live per-thread routing table: `~/.ccc/routes.json`.
pub fn routes_file() -> Result<PathBuf> {
    Ok(ccc_dir()?.join("routes.json"))
}

/// Daemon runtime metadata (pid, port): `~/.ccc/daemon.json`.
pub fn daemon_file() -> Result<PathBuf> {
    Ok(ccc_dir()?.join("daemon.json"))
}

/// Claude Code config dir. Honors `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(d));
    }
    Ok(home()?.join(".claude"))
}

/// Claude Code user settings file: `<claude_dir>/settings.json`.
pub fn claude_settings_file() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

/// Ensure `~/.ccc` exists with 0700 perms and return it.
pub fn ensure_ccc_dir() -> Result<PathBuf> {
    let d = ccc_dir()?;
    std::fs::create_dir_all(&d)?;
    set_mode(&d, 0o700)?;
    Ok(d)
}

#[cfg(unix)]
pub fn set_mode(path: &std::path::Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perm = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perm)?;
    Ok(())
}

#[cfg(not(unix))]
pub fn set_mode(_path: &std::path::Path, _mode: u32) -> Result<()> {
    Ok(())
}
