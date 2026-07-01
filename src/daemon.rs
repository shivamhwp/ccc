//! Daemon lifecycle: runtime metadata + launchd integration.

#[cfg(not(target_os = "macos"))]
use anyhow::anyhow;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;

use crate::paths;
use crate::procinfo;

pub const DEFAULT_PORT: u16 = 8787;
const LAUNCHD_LABEL: &str = "ing.shivam.ccc";

#[derive(Debug, Serialize, Deserialize)]
pub struct Runtime {
    pub pid: u32,
    pub port: u16,
}

pub fn write_runtime(port: u16) -> Result<()> {
    paths::ensure_ccc_dir()?;
    let rt = Runtime {
        pid: procinfo::self_pid(),
        port,
    };
    let path = paths::daemon_file()?;
    let mut f = std::fs::File::create(&path)?;
    f.write_all(&serde_json::to_vec_pretty(&rt)?)?;
    paths::set_mode(&path, 0o600)?;
    Ok(())
}

pub fn read_runtime() -> Option<Runtime> {
    let path = paths::daemon_file().ok()?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Is the daemon process recorded in daemon.json alive?
pub fn is_running() -> bool {
    match read_runtime() {
        Some(rt) => procinfo::pid_alive(rt.pid),
        None => false,
    }
}

/// The proxy base URL the daemon serves, e.g. `http://127.0.0.1:8787`.
pub fn base_url() -> String {
    let port = read_runtime().map(|r| r.port).unwrap_or(DEFAULT_PORT);
    format!("http://127.0.0.1:{port}")
}

fn plist_path() -> Result<std::path::PathBuf> {
    Ok(paths::home()?
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

/// Write and load the launchd agent so the daemon starts on login and restarts
/// on crash. Returns the plist path.
#[cfg(target_os = "macos")]
pub fn install_launchd(port: u16) -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let log_dir = paths::ccc_dir()?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>run</string>
        <string>--port</string>
        <string>{port}</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>{log}/daemon.out.log</string>
    <key>StandardErrorPath</key><string>{log}/daemon.err.log</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exe = exe.display(),
        port = port,
        log = log_dir.display(),
    );

    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, plist)?;

    // Reload: bootout (ignore errors) then bootstrap into the GUI domain.
    let uid = unsafe { libc_getuid() };
    let domain = format!("gui/{uid}");
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &domain, &path.to_string_lossy()])
        .output();
    let out = std::process::Command::new("launchctl")
        .args(["bootstrap", &domain, &path.to_string_lossy()])
        .output()
        .context("launchctl bootstrap")?;
    if !out.status.success() {
        // Fall back to legacy `load` for older macOS.
        let _ = std::process::Command::new("launchctl")
            .args(["load", "-w", &path.to_string_lossy()])
            .output();
    }
    Ok(path)
}

#[cfg(target_os = "macos")]
pub fn uninstall_launchd() -> Result<()> {
    let path = plist_path()?;
    let uid = unsafe { libc_getuid() };
    let domain = format!("gui/{uid}");
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &domain, &path.to_string_lossy()])
        .output();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install_launchd(_port: u16) -> Result<std::path::PathBuf> {
    Err(anyhow!("launchd install is only supported on macOS"))
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall_launchd() -> Result<()> {
    Err(anyhow!("launchd uninstall is only supported on macOS"))
}

#[cfg(target_os = "macos")]
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}
