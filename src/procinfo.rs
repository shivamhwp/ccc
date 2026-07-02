//! Process introspection used for per-thread routing.
//!
//! Two primitives, both via always-present macOS/Unix CLIs to avoid unsafe FFI:
//!   - `pid_owning_local_port`: which process owns a loopback TCP source port
//!     (used by the proxy to attribute an inbound connection to a claude PID).
//!   - `ancestors`: the PID chain up to the session, and `find_claude_ancestor`
//!     (used by `ccc use` to discover the claude process that owns the shell it
//!     was invoked from).

use std::collections::HashMap;

/// Return the pid whose socket has `port` as its *local* port, excluding
/// `exclude` (the daemon's own pid). Uses `lsof` machine-readable field output.
pub fn pid_owning_local_port(port: u16, exclude: u32) -> Option<u32> {
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

/// Map of pid -> (ppid, full command line) for all processes, via one `ps`
/// call. The full command (not just `comm`) is needed to recognize npm/`node`
/// and `bun` installs of Claude Code, where `comm` is `node`/`bun`.
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
/// Covers the native binary (`.../claude`), npm/`node` installs
/// (`node .../@anthropic-ai/claude-code/cli.js`), and `bun` installs.
fn is_claude_command(command: &str) -> bool {
    // First whitespace-separated token is the executable (argv0).
    let argv0 = command.split_whitespace().next().unwrap_or(command);
    let exe = argv0.rsplit('/').next().unwrap_or(argv0);

    // Native install: the executable itself is `claude`.
    if exe == "claude" {
        return true;
    }
    // npm / bun installs run under node/bun with the CLI path in argv. Match
    // on the distinctive package path rather than a bare "claude" substring so
    // we don't misfire on unrelated processes that merely mention the word.
    let runner = matches!(exe, "node" | "bun" | "node.js" | "deno");
    if runner
        && (command.contains("@anthropic-ai/claude-code")
            || command.contains("claude-code/cli")
            || command.contains(".claude/local/")
            || command.contains("/claude-code/"))
    {
        return true;
    }
    false
}

/// True if a process with this pid currently exists.
pub fn pid_alive(pid: u32) -> bool {
    // signal 0 checks existence without delivering a signal.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
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
