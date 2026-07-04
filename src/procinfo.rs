//! Process introspection used for per-thread routing.
//!
//! Two primitives, via CLIs / procfs to avoid unsafe FFI:
//!   - `pid_owning_local_port`: which process owns a loopback TCP source port
//!     (used by the proxy to attribute an inbound connection to a claude PID).
//!     On Linux this reads `/proc` directly (no external tools needed) and
//!     falls back to `lsof`; on Windows it parses `netstat -ano` with a
//!     `Get-NetTCPConnection` fallback; elsewhere it uses `lsof`.
//!   - `ancestors`: the PID chain up to the session, and `find_claude_ancestor`
//!     (used by `ccc use` to discover the claude process that owns the shell it
//!     was invoked from).

use std::collections::HashMap;

/// Return the pid whose socket has `port` as its *local* port, excluding
/// `exclude` (the daemon's own pid).
#[cfg(not(windows))]
pub fn pid_owning_local_port(port: u16, exclude: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    if let Some(pid) = procfs_pid_owning_local_port(port, exclude) {
        return Some(pid);
    }
    lsof_pid_owning_local_port(port, exclude)
}

#[cfg(windows)]
pub fn pid_owning_local_port(port: u16, exclude: u32) -> Option<u32> {
    netstat_pid_owning_local_port(port, exclude)
        .or_else(|| powershell_pid_owning_local_port(port, exclude))
}

/// `lsof`-based lookup, using machine-readable field output.
#[cfg(not(windows))]
fn lsof_pid_owning_local_port(port: u16, exclude: u32) -> Option<u32> {
    // `lsof -nP -iTCP:PORT` lists both endpoints of the loopback connection:
    // the client (claude) whose local port == PORT, and the daemon whose
    // remote port == PORT. We want the one that is not us.
    //
    // `-FpPn` emits one field per line: `p<pid>`, `n<name>`, etc. A `p` line
    // applies to every `n` line until the next `p` line. This avoids brittle
    // column parsing (the human format appends a `(ESTABLISHED)` token).
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-Fpn", &format!("-iTCP:{port}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_owner_from_lsof(&text, port, exclude)
}

/// Pure parser for `lsof -Fpn` output: find the pid (≠ `exclude`) with a socket
/// whose *local* port equals `port`. Separated out so it can be unit-tested
/// without spawning lsof.
#[cfg_attr(windows, allow(dead_code))]
fn parse_owner_from_lsof(output: &str, port: u16, exclude: u32) -> Option<u32> {
    let mut cur_pid: Option<u32> = None;
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let (tag, rest) = line.split_at(1);
        match tag {
            "p" => cur_pid = rest.parse::<u32>().ok(),
            "n" => {
                let pid = match cur_pid {
                    Some(p) if p != exclude => p,
                    _ => continue,
                };
                // NAME is `127.0.0.1:LOCAL->127.0.0.1:REMOTE`. The client's
                // local port (left of `->`) equals the ephemeral `port`.
                if let Some((local, _remote)) = rest.split_once("->") {
                    if local.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) == Some(port) {
                        return Some(pid);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Linux: resolve the owning pid via procfs, with no external tools.
/// 1. `/proc/net/tcp` (+`tcp6`): find the ESTABLISHED socket whose *local*
///    port is `port` → its socket inode.
/// 2. Scan `/proc/<pid>/fd/*` symlinks for `socket:[inode]` → pid.
#[cfg(target_os = "linux")]
fn procfs_pid_owning_local_port(port: u16, exclude: u32) -> Option<u32> {
    let inode = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|text| parse_inode_from_proc_net_tcp(&text, port))?;
    find_pid_by_socket_inode(inode, exclude)
}

/// Pure parser for `/proc/net/tcp` content: the socket inode of the
/// ESTABLISHED connection whose local port equals `port`. The daemon's own
/// sockets never match: its listener's local port is the daemon port, and its
/// accepted sockets have the ephemeral port on the *remote* side.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_inode_from_proc_net_tcp(text: &str, port: u16) -> Option<u64> {
    // Format (whitespace-separated, after a header line):
    //   sl local_address rem_address st tx:rx tr:tm retrnsmt uid timeout inode …
    // Addresses are HEXIP:HEXPORT; st 01 = ESTABLISHED.
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || f[3] != "01" {
            continue;
        }
        let local_port = f[1]
            .rsplit(':')
            .next()
            .and_then(|h| u16::from_str_radix(h, 16).ok());
        if local_port == Some(port) {
            return f[9].parse::<u64>().ok();
        }
    }
    None
}

/// Scan `/proc/<pid>/fd` symlinks for `socket:[inode]`. Only same-user
/// processes are readable, which is exactly the set we can route anyway.
#[cfg(target_os = "linux")]
fn find_pid_by_socket_inode(inode: u64, exclude: u32) -> Option<u32> {
    let needle = format!("socket:[{inode}]");
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(p) if p != exclude => p,
            _ => continue,
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                if target.as_os_str() == needle.as_str() {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// Windows: `netstat -ano -p TCP` lists connections with owning PIDs.
#[cfg(windows)]
fn netstat_pid_owning_local_port(port: u16, exclude: u32) -> Option<u32> {
    let out = std::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_owner_from_netstat(&text, port, exclude)
}

/// Windows fallback when netstat parsing yields nothing (e.g. a localized
/// state column): structured lookup via PowerShell.
#[cfg(windows)]
fn powershell_pid_owning_local_port(port: u16, exclude: u32) -> Option<u32> {
    let script = format!(
        "(Get-NetTCPConnection -LocalPort {port} -State Established -ErrorAction SilentlyContinue | Select-Object -First 1).OwningProcess"
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    let pid = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (pid != exclude && pid != 0).then_some(pid)
}

/// Pure parser for `netstat -ano` output: the pid (≠ `exclude`, ≠ 0) of the
/// TCP connection whose *local* port equals `port`. Rows look like
/// `  TCP    127.0.0.1:50123    127.0.0.1:8787    ESTABLISHED    1234`
/// (also `[::1]:PORT` for v6). Prefers an ESTABLISHED row but accepts any
/// live-pid match so localized state names still resolve; pid-0 rows
/// (TIME_WAIT ghosts) never match.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_owner_from_netstat(output: &str, port: u16, exclude: u32) -> Option<u32> {
    let mut fallback: Option<u32> = None;
    for line in output.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // proto, local, foreign, state, pid — UDP rows have only 4 fields.
        if f.len() != 5 || !f[0].starts_with("TCP") {
            continue;
        }
        let local_port = f[1].rsplit(':').next().and_then(|p| p.parse::<u16>().ok());
        if local_port != Some(port) {
            continue;
        }
        let pid = match f[4].parse::<u32>() {
            Ok(p) if p != 0 && p != exclude => p,
            _ => continue,
        };
        if f[3] == "ESTABLISHED" {
            return Some(pid);
        }
        fallback.get_or_insert(pid);
    }
    fallback
}

/// Map of pid -> (ppid, full command line) for all processes, via one `ps`
/// call. The full command (not just `comm`) is needed to recognize npm/`node`
/// and `bun` installs of Claude Code, where `comm` is `node`/`bun`.
#[cfg(not(windows))]
fn process_table() -> HashMap<u32, (u32, String)> {
    let mut map = HashMap::new();
    if let Ok(out) = std::process::Command::new("ps")
        .args(["-Ao", "pid=,ppid=,command="])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let mut it = line.split_whitespace();
            if let (Some(pid), Some(ppid)) = (it.next(), it.next()) {
                if let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) {
                    let command = it.collect::<Vec<_>>().join(" ");
                    map.insert(pid, (ppid, command));
                }
            }
        }
    }
    map
}

/// Windows process table via one PowerShell CIM query (wmic is gone from
/// current Windows 11). JSON output avoids CSV quoting pitfalls in
/// CommandLine values.
#[cfg(windows)]
fn process_table() -> HashMap<u32, (u32, String)> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,CommandLine | ConvertTo-Json -Compress",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            parse_process_table_json(&String::from_utf8_lossy(&o.stdout))
        }
        _ => HashMap::new(),
    }
}

/// Pure parser for the `Win32_Process | ConvertTo-Json` output.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_process_table_json(text: &str) -> HashMap<u32, (u32, String)> {
    let mut map = HashMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return map;
    };
    // A single-process result serializes as an object, not a one-item array.
    let items = match &v {
        serde_json::Value::Array(a) => a.as_slice(),
        serde_json::Value::Object(_) => std::slice::from_ref(&v),
        _ => return map,
    };
    for p in items {
        let (Some(pid), Some(ppid)) = (
            p.get("ProcessId").and_then(|x| x.as_u64()),
            p.get("ParentProcessId").and_then(|x| x.as_u64()),
        ) else {
            continue;
        };
        let command = p
            .get("CommandLine")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        map.insert(pid as u32, (ppid as u32, command));
    }
    map
}

/// The chain of pids from `pid` up to (but not including) pid 1/0, inclusive of
/// `pid` itself. Ordered nearest-first.
pub fn ancestors(pid: u32) -> Vec<u32> {
    let table = process_table();
    ancestors_with(pid, &table)
}

fn ancestors_with(pid: u32, table: &HashMap<u32, (u32, String)>) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut cur = pid;
    let mut guard = 0;
    while cur > 1 && guard < 64 {
        chain.push(cur);
        match table.get(&cur) {
            Some((ppid, _)) => cur = *ppid,
            None => break,
        }
        guard += 1;
    }
    chain
}

/// Walk up from `start` and return the first ancestor that looks like the
/// Claude Code CLI process. Used so `ccc use`, invoked from within a Bash tool
/// call, can find the owning claude thread.
pub fn find_claude_ancestor(start: u32) -> Option<u32> {
    let table = process_table();
    let mut cur = start;
    let mut guard = 0;
    while cur > 1 && guard < 64 {
        if let Some((ppid, command)) = table.get(&cur) {
            if is_claude_command(command) {
                return Some(cur);
            }
            cur = *ppid;
        } else {
            break;
        }
        guard += 1;
    }
    None
}

/// Heuristic: does this full command line belong to the Claude Code CLI?
/// Covers the native binary (`.../claude`, `claude.exe`), npm/`node` installs
/// (`node .../@anthropic-ai/claude-code/cli.js`), and `bun` installs — with
/// Windows path separators and quoted argv0 normalized first.
fn is_claude_command(command: &str) -> bool {
    // Normalize separators so one set of matchers covers Windows too.
    let norm = command.replace('\\', "/");

    // argv0 is the first whitespace-separated token, or the quoted prefix on
    // Windows (`"C:\Program Files\...\claude.exe" --flag`).
    let argv0 = match norm.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => norm.split_whitespace().next().unwrap_or(&norm),
    };
    let exe = argv0
        .rsplit('/')
        .next()
        .unwrap_or(argv0)
        .to_ascii_lowercase();
    let exe = exe
        .strip_suffix(".exe")
        .or_else(|| exe.strip_suffix(".cmd"))
        .unwrap_or(&exe);

    // Native install: the executable itself is `claude`.
    if exe == "claude" {
        return true;
    }
    // npm / bun installs run under node/bun with the CLI path in argv. Match
    // on the distinctive package path rather than a bare "claude" substring so
    // we don't misfire on unrelated processes that merely mention the word.
    let runner = matches!(exe, "node" | "bun" | "node.js" | "deno");
    if runner
        && (norm.contains("@anthropic-ai/claude-code")
            || norm.contains("claude-code/cli")
            || norm.contains(".claude/local/")
            || norm.contains("/claude-code/"))
    {
        return true;
    }
    false
}

/// The full command line of a process, or empty if it can't be read. Used as
/// a pid-reuse guard before killing a recorded daemon pid.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn command_of(pid: u32) -> String {
    process_table()
        .get(&pid)
        .map(|(_, cmd)| cmd.clone())
        .unwrap_or_default()
}

/// Opaque token identifying when `pid` started, stable for the process's
/// lifetime. Routes record it so a pid recycled by the OS (same number, new
/// process) no longer matches. None when the process is gone or the platform
/// query fails.
///
/// Cached briefly: the proxy calls this per request via route resolution, and
/// the platform lookup spawns `ps`/PowerShell everywhere but Linux. A
/// (pid, token) pair can only go stale if the pid is recycled within the TTL —
/// a far smaller window than the unguarded resolution this token protects
/// against.
pub fn pid_start_token(pid: u32) -> Option<String> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const TTL: Duration = Duration::from_secs(2);
    type Cache = Mutex<HashMap<u32, (Instant, Option<String>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some((at, token)) = map.get(&pid) {
            if at.elapsed() < TTL {
                return token.clone();
            }
        }
    }
    let token = pid_start_token_uncached(pid);
    if let Ok(mut map) = cache.lock() {
        map.retain(|_, (at, _)| at.elapsed() < TTL);
        map.insert(pid, (Instant::now(), token.clone()));
    }
    token
}

#[cfg(target_os = "linux")]
fn pid_start_token_uncached(pid: u32) -> Option<String> {
    // Field 22 of /proc/<pid>/stat is starttime (clock ticks since boot).
    // comm (field 2) may contain spaces and parens, so split after the last
    // closing paren: state is the next token, starttime the 20th.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after) = stat.rsplit_once(')')?;
    after.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn pid_start_token_uncached(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(windows)]
fn pid_start_token_uncached(pid: u32) -> Option<String> {
    let script =
        format!("(Get-Process -Id {pid} -ErrorAction SilentlyContinue).StartTime.ToFileTime()");
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// True if a process with this pid currently exists.
#[cfg(not(windows))]
pub fn pid_alive(pid: u32) -> bool {
    // signal 0 checks existence without delivering a signal.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// True if a process with this pid currently exists (Windows). `tasklist`
/// prints a CSV row containing the quoted pid on a match, and a prose INFO
/// line (in any locale, without the pid) when there is none.
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

/// Our own pid.
pub fn self_pid() -> u32 {
    std::process::id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_token_is_stable_for_a_live_process() {
        let me = self_pid();
        let a = pid_start_token(me);
        assert!(a.is_some(), "start token for our own pid must resolve");
        // Second call is served by the cache; a fresh platform lookup agrees.
        assert_eq!(a, pid_start_token(me));
        assert_eq!(a, pid_start_token_uncached(me));
    }

    #[test]
    fn picks_client_pid_by_local_port() {
        // curl (pid 111) local port 50123 -> daemon 8791; daemon (pid 222) is
        // the reverse. We want 111 when looking up local port 50123.
        let out =
            "p111\nn127.0.0.1:50123->127.0.0.1:8791\np222\nn127.0.0.1:8791->127.0.0.1:50123\n";
        assert_eq!(parse_owner_from_lsof(out, 50123, 222), Some(111));
    }

    #[test]
    fn excludes_daemon_pid() {
        // Only the daemon side is present for this port; must not match it.
        let out = "p222\nn127.0.0.1:8791->127.0.0.1:50123\n";
        assert_eq!(parse_owner_from_lsof(out, 8791, 222), None);
    }

    #[test]
    fn no_match_returns_none() {
        let out = "p111\nn127.0.0.1:50123->127.0.0.1:8791\n";
        assert_eq!(parse_owner_from_lsof(out, 40000, 999), None);
    }

    #[test]
    fn proc_net_tcp_finds_established_local_port() {
        // Client socket: local 127.0.0.1:50123 (C4AB) -> 127.0.0.1:8791 (2257),
        // ESTABLISHED (st 01), inode 4242. Plus the daemon's listener (st 0A)
        // and a TIME_WAIT ghost (st 06) on the same port that must be skipped.
        let text = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:2257 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1111 1 0 100 0 0 10 0
   1: 0100007F:C4AB 0100007F:2257 06 00000000:00000000 00:00000000 00000000  1000        0 0 1 0 100 0 0 10 0
   2: 0100007F:C4AB 0100007F:2257 01 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0 100 0 0 10 0
";
        assert_eq!(parse_inode_from_proc_net_tcp(text, 0xC4AB), Some(4242));
        // Daemon listener port doesn't match as a client-local port…
        // (st 0A = LISTEN, filtered out)
        assert_eq!(parse_inode_from_proc_net_tcp(text, 0x2257), None);
        // …and an unknown port finds nothing.
        assert_eq!(parse_inode_from_proc_net_tcp(text, 0x1234), None);
    }

    #[test]
    fn netstat_finds_established_local_port() {
        // Client 127.0.0.1:50123 -> daemon :8787 (pid 1234); the daemon's own
        // accepted socket is the mirror row (pid 222); a TIME_WAIT ghost on
        // the same local port has pid 0; UDP rows have no state column.
        let out = "\
Active Connections

  Proto  Local Address          Foreign Address        State           PID
  TCP    0.0.0.0:8787           0.0.0.0:0              LISTENING       222
  TCP    127.0.0.1:50123        127.0.0.1:8787         TIME_WAIT       0
  TCP    127.0.0.1:50123        127.0.0.1:8787         ESTABLISHED     1234
  TCP    127.0.0.1:8787         127.0.0.1:50123        ESTABLISHED     222
  UDP    0.0.0.0:5353           *:*                                    555
";
        assert_eq!(parse_owner_from_netstat(out, 50123, 222), Some(1234));
        // The daemon pid is excluded even though its accepted socket's local
        // port is the daemon port.
        assert_eq!(parse_owner_from_netstat(out, 8787, 222), None);
        assert_eq!(parse_owner_from_netstat(out, 40000, 222), None);
    }

    #[test]
    fn netstat_localized_state_still_resolves() {
        // Non-English Windows localizes the state column; a matching local
        // port with a live pid should still resolve via the fallback.
        let out = "  TCP    [::1]:50200            [::1]:8787             HERGESTELLT     4321\n";
        assert_eq!(parse_owner_from_netstat(out, 50200, 222), Some(4321));
    }

    #[test]
    fn parses_powershell_process_table_json() {
        // Array form + null CommandLine (system processes).
        let text = r#"[{"ProcessId":4,"ParentProcessId":0,"CommandLine":null},
            {"ProcessId":1234,"ParentProcessId":4,"CommandLine":"\"C:\\Users\\x\\AppData\\Local\\Programs\\claude\\claude.exe\" --resume"}]"#;
        let map = parse_process_table_json(text);
        assert_eq!(map.get(&4).map(|(pp, _)| *pp), Some(0));
        assert!(map.get(&1234).unwrap().1.contains("claude.exe"));

        // Single process serializes as a bare object.
        let one = r#"{"ProcessId":7,"ParentProcessId":1,"CommandLine":"ccc daemon run"}"#;
        let map = parse_process_table_json(one);
        assert_eq!(map.get(&7).map(|(pp, _)| *pp), Some(1));

        assert!(parse_process_table_json("not json").is_empty());
    }

    #[test]
    fn detects_windows_installs() {
        // Native install, quoted path with spaces.
        assert!(is_claude_command(
            r#""C:\Users\x\AppData\Local\Programs\claude\claude.exe" --resume"#
        ));
        // Unquoted native path.
        assert!(is_claude_command(r"C:\tools\claude.exe"));
        // npm shim under node.exe.
        assert!(is_claude_command(
            r#""C:\Program Files\nodejs\node.exe" "C:\Users\x\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\cli.js""#
        ));
        // Unrelated Windows processes don't match.
        assert!(!is_claude_command(
            r"C:\Windows\System32\svchost.exe -k netsvcs"
        ));
        assert!(!is_claude_command(
            r#""C:\Program Files\nodejs\node.exe" C:\app\server.js"#
        ));
    }

    #[test]
    fn detects_native_and_npm_installs() {
        // native binary
        assert!(is_claude_command("/Users/x/.local/bin/claude -p hi"));
        assert!(is_claude_command("claude"));
        // npm / node install
        assert!(is_claude_command(
            "node /Users/x/n/lib/node_modules/@anthropic-ai/claude-code/cli.js"
        ));
        // bun install
        assert!(is_claude_command(
            "bun /Users/x/.bun/install/global/node_modules/claude-code/cli.js"
        ));
        // local install path
        assert!(is_claude_command("node /Users/x/.claude/local/cli.js"));
    }

    #[test]
    fn does_not_misfire_on_unrelated() {
        assert!(!is_claude_command("/opt/homebrew/bin/fish"));
        assert!(!is_claude_command("ccc use work --pid 123"));
        assert!(!is_claude_command("node /Users/x/some/other/app.js"));
        // a shell that merely mentions the word in an argument shouldn't match
        assert!(!is_claude_command("bash -c echo claude"));
    }
}
