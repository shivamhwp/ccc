//! Daemon lifecycle: runtime metadata + per-platform autostart.
//!
//! Autostart backends:
//!   - macOS: launchd agent in `~/Library/LaunchAgents` (gui domain).
//!   - Linux: systemd user unit in `~/.config/systemd/user`; when no user
//!     systemd is available (some WSL/container setups), a detached background
//!     process is spawned instead (runs now, but won't survive a reboot).
//!   - elsewhere: unsupported — `ccc daemon run` in the foreground still works.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;

use crate::paths;
use crate::procinfo;

pub const DEFAULT_PORT: u16 = 8787;

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

/// Install and start the platform's autostart agent for the daemon. Returns a
/// human-readable description of what was set up (for `✓` output).
pub fn install_autostart(port: u16) -> Result<String> {
    imp::install_autostart(port)
}

/// Stop the daemon and remove its autostart agent.
pub fn uninstall_autostart() -> Result<()> {
    imp::uninstall_autostart()
}

// ---------------------------------------------------------------------------
// macOS: launchd
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use anyhow::Context;

    const LAUNCHD_LABEL: &str = "ing.shivam.ccc";

    fn plist_path() -> Result<std::path::PathBuf> {
        Ok(paths::home()?
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")))
    }

    /// Write and load the launchd agent so the daemon starts on login and
    /// restarts on crash.
    pub fn install_autostart(port: u16) -> Result<String> {
        let exe = std::env::current_exe()?.canonicalize()?;
        let log_dir = paths::ensure_ccc_dir()?;
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
        Ok(format!("launchd agent: {}", path.display()))
    }

    pub fn uninstall_autostart() -> Result<()> {
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

    extern "C" {
        #[link_name = "getuid"]
        fn libc_getuid() -> u32;
    }
}

// ---------------------------------------------------------------------------
// Linux: systemd user unit, with a detached-process fallback
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use anyhow::{anyhow, Context};

    const UNIT_NAME: &str = "ccc.service";

    fn unit_path() -> Result<std::path::PathBuf> {
        let config = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
            _ => paths::home()?.join(".config"),
        };
        Ok(config.join("systemd/user").join(UNIT_NAME))
    }

    /// Does this session have a working user systemd? False on distros without
    /// systemd and in most containers / older WSL setups.
    fn systemd_user_available() -> bool {
        std::process::Command::new("systemctl")
            .args(["--user", "show-environment"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn install_autostart(port: u16) -> Result<String> {
        let exe = std::env::current_exe()?.canonicalize()?;
        if systemd_user_available() {
            install_systemd(&exe, port)
        } else {
            spawn_detached(&exe, port)
        }
    }

    fn install_systemd(exe: &std::path::Path, port: u16) -> Result<String> {
        // The unit appends stdout/stderr to files under ~/.ccc — systemd fails
        // the service at spawn (status 209/STDOUT) if the directory is missing.
        let log_dir = paths::ensure_ccc_dir()?;
        let unit = super::systemd_unit(exe, port, &log_dir);

        let path = unit_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, unit)?;

        let run = |args: &[&str]| -> Result<std::process::Output> {
            std::process::Command::new("systemctl")
                .arg("--user")
                .args(args)
                .output()
                .with_context(|| format!("systemctl --user {}", args.join(" ")))
        };
        run(&["daemon-reload"])?;
        // A prior crash loop leaves the unit in a rate-limited failed state
        // that blocks a fresh start; clear it before restarting.
        let _ = run(&["reset-failed", UNIT_NAME]);
        // `enable` makes it start on login; `restart` starts it now (and picks
        // up a changed unit/binary if it was already running).
        let _ = run(&["enable", UNIT_NAME]);
        let out = run(&["restart", UNIT_NAME])?;
        if !out.status.success() {
            return Err(anyhow!(
                "systemctl --user restart {UNIT_NAME} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(format!("systemd user unit: {}", path.display()))
    }

    /// No user systemd: start the daemon as a detached background process. It
    /// runs until killed or reboot (no autostart on login).
    fn spawn_detached(exe: &std::path::Path, port: u16) -> Result<String> {
        if is_running() {
            let rt = read_runtime().unwrap();
            return Ok(format!("daemon already running (pid {})", rt.pid));
        }
        let log_dir = paths::ensure_ccc_dir()?;
        let open_log = |name: &str| -> Result<std::fs::File> {
            Ok(std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_dir.join(name))?)
        };
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new(exe)
            .args(["daemon", "run", "--port", &port.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(open_log("daemon.out.log")?)
            .stderr(open_log("daemon.err.log")?)
            .process_group(0)
            .spawn()
            .context("spawning detached daemon")?;
        Ok(format!(
            "background process (pid {}) — no user systemd found, so it won't restart after reboot; \
             re-run `ccc daemon start` then",
            child.id()
        ))
    }

    pub fn uninstall_autostart() -> Result<()> {
        // Stop + disable the systemd unit if it exists (ignore errors: systemd
        // may be absent, or the unit never installed).
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", UNIT_NAME])
            .output();
        let path = unit_path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .output();
        }

        // Detached-fallback case: the recorded daemon pid may still be alive.
        // Only kill it if the process actually looks like ccc (pid reuse guard).
        if let Some(rt) = read_runtime() {
            if procinfo::pid_alive(rt.pid) && procinfo::command_of(rt.pid).contains("ccc") {
                let _ = std::process::Command::new("kill")
                    .arg(rt.pid.to_string())
                    .output();
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Windows: HKCU Run key + hidden wscript launcher (no admin required)
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::{anyhow, Context};

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_VALUE: &str = "ccc";

    fn launcher_path() -> Result<std::path::PathBuf> {
        Ok(paths::ccc_dir()?.join("ccc-daemon.vbs"))
    }

    /// Register a Run-key entry that launches the daemon hidden at login, and
    /// start it now. The launcher invokes `ccc daemon start`, which is
    /// idempotent and redirects the daemon's output to ~/.ccc log files —
    /// wscript's window style 0 keeps the console from ever appearing.
    pub fn install_autostart(port: u16) -> Result<String> {
        // No canonicalize() here: on Windows it yields a \\?\-prefixed path
        // that wscript and the shell handle badly.
        let exe = std::env::current_exe()?;
        paths::ensure_ccc_dir()?;

        let vbs_path = launcher_path()?;
        std::fs::write(&vbs_path, super::windows_launcher_vbs(&exe, port))?;

        let launcher = format!("wscript.exe \"{}\"", vbs_path.display());
        let out = std::process::Command::new("reg")
            .args([
                "add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &launcher, "/f",
            ])
            .output()
            .context("reg add (Run key)")?;
        if !out.status.success() {
            return Err(anyhow!(
                "registering the Run key failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        let started = start_daemon_now(&exe, port)?;
        Ok(format!("Run key `{RUN_VALUE}` + {started}"))
    }

    /// Spawn the daemon hidden and detached with output redirected to the
    /// ~/.ccc log files. No-op if it's already running.
    fn start_daemon_now(exe: &std::path::Path, port: u16) -> Result<String> {
        if is_running() {
            let rt = read_runtime().unwrap();
            return Ok(format!("daemon already running (pid {})", rt.pid));
        }
        let log_dir = paths::ensure_ccc_dir()?;
        let open_log = |name: &str| -> Result<std::fs::File> {
            Ok(std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_dir.join(name))?)
        };
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let child = std::process::Command::new(exe)
            .args(["daemon", "run", "--port", &port.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(open_log("daemon.out.log")?)
            .stderr(open_log("daemon.err.log")?)
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .context("spawning daemon")?;
        Ok(format!("daemon started (pid {})", child.id()))
    }

    pub fn uninstall_autostart() -> Result<()> {
        // Remove the Run key + launcher (ignore errors: may never have been
        // installed).
        let _ = std::process::Command::new("reg")
            .args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])
            .output();
        if let Ok(vbs) = launcher_path() {
            let _ = std::fs::remove_file(vbs);
        }

        // Kill the recorded daemon if it's alive — guarded by a command-line
        // check so a reused pid is never killed.
        if let Some(rt) = read_runtime() {
            if procinfo::pid_alive(rt.pid)
                && procinfo::command_of(rt.pid)
                    .to_ascii_lowercase()
                    .contains("ccc")
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &rt.pid.to_string(), "/T", "/F"])
                    .output();
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Other platforms: no autostart backend
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod imp {
    use super::*;
    use anyhow::anyhow;

    pub fn install_autostart(_port: u16) -> Result<String> {
        Err(anyhow!(
            "daemon autostart is not supported on this platform — run `ccc daemon run` instead"
        ))
    }

    pub fn uninstall_autostart() -> Result<()> {
        Err(anyhow!(
            "daemon autostart is not supported on this platform"
        ))
    }
}

/// The systemd user unit for the daemon. Kept as a pure function (and outside
/// the cfg'd module) so it's unit-testable on every platform.
#[allow(dead_code)]
fn systemd_unit(exe: &std::path::Path, port: u16, log_dir: &std::path::Path) -> String {
    format!(
        r#"[Unit]
Description=ccc — per-thread account proxy for Claude Code
After=network.target

[Service]
ExecStart={exe} daemon run --port {port}
Restart=always
RestartSec=1
StandardOutput=append:{log}/daemon.out.log
StandardError=append:{log}/daemon.err.log

[Install]
WantedBy=default.target
"#,
        exe = exe.display(),
        port = port,
        log = log_dir.display(),
    )
}

/// The VBScript login launcher for Windows. Runs `ccc daemon start` with
/// window style 0 (fully hidden) and no wait; `daemon start` is idempotent —
/// it re-registers the Run key and spawns the daemon only if it isn't already
/// running, with output redirected to the ~/.ccc log files. Pure function
/// (and outside the cfg'd module) so it's unit-testable on every platform.
#[allow(dead_code)]
fn windows_launcher_vbs(exe: &std::path::Path, port: u16) -> String {
    // VBScript string literals escape an embedded quote by doubling it, hence
    // the `""…""` around the exe path (which may contain spaces).
    format!(
        "CreateObject(\"WScript.Shell\").Run \"\"\"{exe}\"\" daemon start --port {port}\", 0, False\r\n",
        exe = exe.display(),
        port = port,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn windows_launcher_hides_window_and_quotes_path() {
        let vbs = windows_launcher_vbs(Path::new(r"C:\Users\x y\AppData\Local\ccc\ccc.exe"), 8787);
        assert!(vbs
            .contains(r#""""C:\Users\x y\AppData\Local\ccc\ccc.exe"" daemon start --port 8787""#));
        assert!(vbs.trim_end().ends_with(", 0, False"));
    }

    #[test]
    fn systemd_unit_contains_exec_and_logs() {
        let unit = systemd_unit(
            Path::new("/usr/local/bin/ccc"),
            8787,
            Path::new("/home/u/.ccc"),
        );
        assert!(unit.contains("ExecStart=/usr/local/bin/ccc daemon run --port 8787"));
        assert!(unit.contains("StandardOutput=append:/home/u/.ccc/daemon.out.log"));
        assert!(unit.contains("StandardError=append:/home/u/.ccc/daemon.err.log"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
    }
}
