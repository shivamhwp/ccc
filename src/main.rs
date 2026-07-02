//! ccc — use multiple Claude Code accounts on one device, switchable per thread.
//!
//! Subscription auth only. See module docs for the architecture; in short: a
//! local proxy (the daemon) rewrites each thread's requests to authenticate as
//! a chosen saved account, selected via a PID-based route table.

mod daemon;
mod oauth;
mod oauthcfg;
mod paths;
mod procinfo;
mod proxy;
mod routes;
mod setup;
mod store;
mod t3;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use store::{now_ms, Store};

#[derive(Parser)]
#[command(
    name = "ccc",
    version,
    about = "Multi-account switching for Claude Code (subscription auth)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install everything: import current login, patch settings, install skill + daemon.
    Setup,
    /// Log in to a Claude account and save it under a profile name.
    Login {
        /// Profile name to store the account under.
        name: String,
        /// Print the authorization URL and exit (step 1 of a two-step login).
        #[arg(long)]
        begin: bool,
        /// Complete login with the pasted code (step 2). Implies the name used with --begin.
        #[arg(long, value_name = "CODE")]
        finish: Option<String>,
    },
    /// Import the account currently logged into Claude Code as a profile.
    Import {
        /// Profile name (default: "default").
        #[arg(default_value = "default")]
        name: String,
    },
    /// List saved accounts.
    List,
    /// Show which account the current thread is using.
    Whoami,
    /// Route the current thread (or --pid) to an account.
    Use {
        /// Profile name; omit with --default to revert to the default account.
        name: Option<String>,
        /// Revert this thread to the default account.
        #[arg(long)]
        default: bool,
        /// Target a specific claude PID instead of auto-detecting.
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Launch Claude Code as a specific account (like codex-p / codex-vm).
    /// The whole session uses that account — /status and traffic both read
    /// correctly. Chosen at launch (no live in-thread switching).
    Run {
        /// Account to launch as.
        name: String,
        /// Extra arguments passed through to `claude`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Set the default account (used by threads with no explicit route).
    Default { name: String },
    /// Remove a saved account.
    Remove { name: String },
    /// Undo `ccc setup`: revert settings.json, remove the skill, stop the daemon.
    Teardown,
    /// Diagnostics: verify the daemon, settings, and auth path.
    Doctor,
    /// Daemon control.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// t3code integration.
    T3 {
        #[command(subcommand)]
        action: T3Action,
    },
}

#[derive(Subcommand)]
enum T3Action {
    /// Add/update one t3code provider instance per ccc account.
    Sync,
    /// Remove all ccc-managed instances from t3code.
    Unsync,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the proxy in the foreground (used by launchd/systemd).
    Run {
        #[arg(long, default_value_t = daemon::DEFAULT_PORT)]
        port: u16,
    },
    /// Install + start the autostart agent (launchd / systemd / Run key).
    Start {
        #[arg(long, default_value_t = daemon::DEFAULT_PORT)]
        port: u16,
    },
    /// Stop the daemon and remove the autostart agent.
    Stop,
    /// Show daemon status.
    Status,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Setup => cmd_setup().await,
        Command::Login {
            name,
            begin,
            finish,
        } => cmd_login(&name, begin, finish).await,
        Command::Import { name } => cmd_import(&name),
        Command::List => cmd_list(),
        Command::Whoami => cmd_whoami(),
        Command::Use { name, default, pid } => cmd_use(name, default, pid),
        Command::Run { name, args } => cmd_run(&name, args).await,
        Command::Default { name } => cmd_default(&name),
        Command::Remove { name } => cmd_remove(&name),
        Command::Teardown => cmd_teardown(),
        Command::Doctor => cmd_doctor().await,
        Command::Daemon { action } => match action {
            DaemonAction::Run { port } => proxy::run(port).await,
            DaemonAction::Start { port } => cmd_daemon_start(port),
            DaemonAction::Stop => cmd_daemon_stop(),
            DaemonAction::Status => cmd_daemon_status(),
        },
        Command::T3 { action } => match action {
            T3Action::Sync => cmd_t3_sync().await,
            T3Action::Unsync => cmd_t3_unsync(),
        },
    }
}

async fn cmd_t3_sync() -> Result<()> {
    let ids = t3::sync().await?;
    println!("✓ synced {} account(s) into t3code:", ids.len());
    for id in &ids {
        println!("    {id}");
    }
    if !daemon::is_running() {
        eprintln!("warning: the ccc daemon is not running — run `ccc daemon start`.");
    }
    println!("\nRestart t3code (or reload its settings) to see the new providers.");
    Ok(())
}

fn cmd_t3_unsync() -> Result<()> {
    let n = t3::unsync()?;
    println!("✓ removed {n} ccc-managed instance(s) from t3code");
    Ok(())
}

async fn cmd_setup() -> Result<()> {
    paths::ensure_ccc_dir()?;

    // 1. Import current login if we have no profiles yet.
    let store = Store::load()?;
    if store.profiles.is_empty() {
        match cmd_import("default") {
            Ok(()) => {}
            Err(e) => eprintln!(
                "note: could not import an existing Claude login ({e:#}).\n      Run `ccc login <name>` to add an account."
            ),
        }
    }

    // 2. Start the daemon (launchd / systemd / detached fallback).
    let port = daemon::DEFAULT_PORT;
    match daemon::install_autostart(port) {
        Ok(desc) => println!("✓ daemon started — {desc}"),
        Err(e) => eprintln!("note: daemon autostart not installed ({e:#}). Use `ccc daemon run`."),
    }

    // 3. Patch settings.json + install skill.
    let base = format!("http://127.0.0.1:{port}");
    let set = setup::patch_settings(&base)?;
    println!("✓ Claude Code settings.json → ANTHROPIC_BASE_URL={set}");
    let skill = setup::install_skill()?;
    println!("✓ agent skill installed at {}", skill.display());

    println!("\nSetup complete. Add more accounts with `ccc login <name>`.");
    println!("In any Claude thread you can say: \"using ccc, use the <name> account and …\"");
    Ok(())
}

fn login_pending_file() -> Result<std::path::PathBuf> {
    Ok(paths::ccc_dir()?.join("login-pending.json"))
}

async fn cmd_login(name: &str, begin: bool, finish: Option<String>) -> Result<()> {
    // Two-step (non-interactive) mode: --begin prints the URL, --finish <code>
    // completes. Used for SSH/headless and for driving login programmatically.
    if let Some(code) = finish {
        return login_finish(name, &code).await;
    }
    if begin {
        return login_begin(name);
    }

    // Interactive mode: print URL, open browser, read the pasted code.
    let pkce = oauth::new_pkce();
    let url = oauth::authorize_url(&pkce);
    println!("Opening your browser to sign in to Claude…");
    println!("If it doesn't open, visit:\n\n  {url}\n");
    open_browser(&url);

    print!("After approving, paste the code shown (looks like `abc…#state`): ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading pasted code")?;
    let pasted = line.trim();
    if pasted.is_empty() {
        anyhow::bail!("no code entered");
    }
    let profile = oauth::exchange_code(&pkce, pasted).await?;
    save_login(name, profile)
}

fn login_begin(name: &str) -> Result<()> {
    paths::ensure_ccc_dir()?;
    let pkce = oauth::new_pkce();
    let url = oauth::authorize_url(&pkce);
    let pending = serde_json::json!({
        "name": name,
        "verifier": pkce.verifier,
        "state": pkce.state,
    });
    let path = login_pending_file()?;
    std::fs::write(&path, serde_json::to_vec_pretty(&pending)?)?;
    paths::set_mode(&path, 0o600)?;

    println!("Open this URL, sign in, and copy the code shown afterwards:\n");
    println!("  {url}\n");
    println!("Then run:  ccc login {name} --finish '<paste code>'");
    Ok(())
}

async fn login_finish(name: &str, code: &str) -> Result<()> {
    let path = login_pending_file()?;
    let bytes = std::fs::read(&path)
        .context("no pending login found — run `ccc login <name> --begin` first")?;
    let pending: serde_json::Value = serde_json::from_slice(&bytes)?;
    let pending_name = pending.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if pending_name != name {
        anyhow::bail!("pending login is for `{pending_name}`, not `{name}`");
    }
    let pkce = oauth::Pkce {
        verifier: pending
            .get("verifier")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        challenge: String::new(),
        state: pending
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    };
    let profile = oauth::exchange_code(&pkce, code).await?;
    let _ = std::fs::remove_file(&path);
    save_login(name, profile)
}

fn save_login(name: &str, mut profile: store::Profile) -> Result<()> {
    // Enrich identity from ~/.claude.json if the token response lacked it.
    if profile.email.is_none() {
        if let Some(id) = store::read_claude_identity() {
            profile.email = id
                .get("emailAddress")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            profile.organization_name = id
                .get("organizationName")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
    }
    let name_owned = name.to_string();
    let is_first = Store::update(move |s| {
        let first = s.profiles.is_empty();
        s.profiles.insert(name_owned.clone(), profile);
        if first {
            s.default_profile = Some(name_owned);
        }
        Ok(first)
    })?;
    println!(
        "✓ saved account `{name}`{}",
        if is_first { " (set as default)" } else { "" }
    );
    Ok(())
}

fn cmd_import(name: &str) -> Result<()> {
    let oauth_val = store::read_keychain_login()?;
    let mut profile = store::profile_from_oauth(&oauth_val)?;
    if let Some(id) = store::read_claude_identity() {
        profile.email = id
            .get("emailAddress")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        profile.account_uuid = id
            .get("accountUuid")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        profile.organization_uuid = id
            .get("organizationUuid")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        profile.organization_name = id
            .get("organizationName")
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }
    let name_owned = name.to_string();
    Store::update(move |s| {
        let first = s.profiles.is_empty();
        s.profiles.insert(name_owned.clone(), profile);
        if first {
            s.default_profile = Some(name_owned);
        }
        Ok(())
    })?;
    println!("✓ imported current Claude login as `{name}`");
    Ok(())
}

fn cmd_list() -> Result<()> {
    let store = Store::load()?;
    if store.profiles.is_empty() {
        println!("No accounts saved. Add one with `ccc login <name>` or `ccc import`.");
        return Ok(());
    }
    let default = store.resolve_default().map(str::to_string);
    println!("{:<18} {:<10}", "PROFILE", "PLAN");
    for (name, p) in &store.profiles {
        let marker = if default.as_deref() == Some(name.as_str()) {
            "* (default)"
        } else {
            ""
        };
        let plan = p.subscription_type.clone().unwrap_or_else(|| "—".into());
        let expired = if p.needs_refresh(0) { " [expired]" } else { "" };
        println!("{name:<18} {plan:<10} {marker}{expired}");
    }
    Ok(())
}

fn cmd_whoami() -> Result<()> {
    let me = procinfo::self_pid();
    let claude = procinfo::find_claude_ancestor(me);
    let store = Store::load()?;
    let routes = routes::Routes::load()?;

    let profile = match claude {
        Some(pid) => routes
            .resolve_for(pid)
            .or_else(|| store.resolve_default().map(str::to_string)),
        None => store.resolve_default().map(str::to_string),
    };

    match profile {
        Some(name) => match claude {
            Some(pid) => println!("this thread (claude pid {pid}) → {name}"),
            None => println!("no claude thread detected; default account → {name}"),
        },
        None => println!("no account resolved (no route and no default set)"),
    }
    Ok(())
}

async fn cmd_run(name: &str, args: Vec<String>) -> Result<()> {
    let store = Store::load()?;
    let profile = store
        .profiles
        .get(name)
        .cloned()
        .with_context(|| format!("no account named `{name}`"))?;

    if !daemon::is_running() {
        anyhow::bail!("the ccc daemon is not running — run `ccc daemon start` first");
    }

    // Seed a per-account home exactly like the t3code integration: identity +
    // credentials with a FAR-FUTURE expiry so Claude Code treats them as valid
    // and never refreshes (hence never writes to the Keychain — that write is
    // what pops the macOS "keychain cannot be found to store" dialog). The
    // session's real traffic is authenticated by the proxy via the /a/<name>
    // pin, so the seeded token doesn't need to be live.
    let home = t3::provision_home(name, &profile).await?;

    let base = format!("{}/a/{name}", daemon::base_url());
    eprintln!("launching Claude Code as `{name}`…");
    let mut cmd = std::process::Command::new("claude");
    cmd.env("CLAUDE_CONFIG_DIR", &home)
        .env("ANTHROPIC_BASE_URL", &base)
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .args(&args);

    // Replace this process with claude where we can; elsewhere run it as a
    // child and mirror its exit code.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(anyhow::anyhow!("failed to launch claude: {err}"))
    }
    #[cfg(not(unix))]
    {
        let status = match cmd.status() {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // npm installs ship a `claude.cmd` shim, which CreateProcess
                // won't resolve for a bare `claude` — route through cmd.exe.
                let mut shim = std::process::Command::new("cmd");
                shim.args(["/C", "claude"])
                    .env("CLAUDE_CONFIG_DIR", &home)
                    .env("ANTHROPIC_BASE_URL", &base)
                    .env_remove("ANTHROPIC_AUTH_TOKEN")
                    .args(&args);
                shim.status()
                    .map_err(|e| anyhow::anyhow!("failed to launch claude: {e}"))?
            }
            Err(e) => return Err(anyhow::anyhow!("failed to launch claude: {e}")),
        };
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn cmd_use(name: Option<String>, default: bool, pid: Option<u32>) -> Result<()> {
    let target_pid = match pid {
        Some(p) => p,
        None => procinfo::find_claude_ancestor(procinfo::self_pid()).context(
            "couldn't find the claude process for this thread. \
             Run inside a Claude Code thread, or pass --pid <claude pid>.",
        )?,
    };

    if default {
        routes::set_route(target_pid, None)?;
        println!("✓ thread (claude pid {target_pid}) reverted to the default account");
        return Ok(());
    }

    let name = name.context("specify an account name, or use --default")?;
    let store = Store::load()?;
    if !store.profiles.contains_key(&name) {
        anyhow::bail!(
            "no account named `{name}`. Saved: {}",
            store
                .profiles
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    routes::set_route(target_pid, Some(&name))?;

    if !daemon::is_running() {
        eprintln!("warning: the ccc daemon is not running — run `ccc daemon start`.");
    }
    println!("✓ this thread (claude pid {target_pid}) now uses `{name}`");
    Ok(())
}

fn cmd_default(name: &str) -> Result<()> {
    let name_owned = name.to_string();
    Store::update(move |s| {
        if !s.profiles.contains_key(&name_owned) {
            anyhow::bail!("no account named `{name_owned}`");
        }
        s.default_profile = Some(name_owned);
        Ok(())
    })?;
    println!("✓ default account is now `{name}`");
    Ok(())
}

fn cmd_remove(name: &str) -> Result<()> {
    let name_owned = name.to_string();
    Store::update(move |s| {
        if s.profiles.remove(&name_owned).is_none() {
            anyhow::bail!("no account named `{name_owned}`");
        }
        if s.default_profile.as_deref() == Some(name_owned.as_str()) {
            s.default_profile = s.profiles.keys().next().cloned();
        }
        Ok(())
    })?;
    println!("✓ removed `{name}`");
    Ok(())
}

fn cmd_teardown() -> Result<()> {
    setup::unpatch_settings()?;
    println!("✓ reverted Claude Code settings.json");
    setup::remove_skill()?;
    println!("✓ removed agent skill");
    match t3::unsync() {
        Ok(n) if n > 0 => println!("✓ removed {n} ccc instance(s) from t3code"),
        _ => {}
    }
    match daemon::uninstall_autostart() {
        Ok(()) => println!("✓ stopped daemon"),
        Err(e) => eprintln!("note: could not stop daemon ({e:#})"),
    }
    println!("\nDone. Saved accounts remain in ~/.ccc (delete it to remove them).");
    Ok(())
}

async fn cmd_doctor() -> Result<()> {
    let mut ok = true;

    // Daemon.
    if daemon::is_running() {
        let rt = daemon::read_runtime().unwrap();
        println!("✓ daemon running (pid {}, port {})", rt.pid, rt.port);
        // Health probe.
        match reqwest::Client::new()
            .get(format!("{}/_ccc/health", daemon::base_url()))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => println!("✓ daemon health check passed"),
            Ok(r) => {
                ok = false;
                println!("✗ daemon health check returned {}", r.status());
            }
            Err(e) => {
                ok = false;
                println!("✗ daemon not reachable: {e}");
            }
        }
    } else {
        ok = false;
        println!("✗ daemon not running (run `ccc daemon start`)");
    }

    // settings.json.
    let settings_path = paths::claude_settings_file()?;
    match std::fs::read(&settings_path) {
        Ok(b) if !b.is_empty() => {
            let v: serde_json::Value = serde_json::from_slice(&b)?;
            match v.get("env").and_then(|e| e.get("ANTHROPIC_BASE_URL")) {
                Some(u) => println!("✓ settings.json routes to {}", u),
                None => {
                    ok = false;
                    println!("✗ settings.json has no ANTHROPIC_BASE_URL (run `ccc setup`)");
                }
            }
        }
        _ => {
            ok = false;
            println!("✗ no settings.json at {}", settings_path.display());
        }
    }

    // Profiles.
    let store = Store::load()?;
    println!(
        "• {} account(s) saved; default = {}",
        store.profiles.len(),
        store.resolve_default().unwrap_or("(none)")
    );
    for (name, p) in &store.profiles {
        let secs = (p.expires_at - now_ms()) / 1000;
        let state = if p.needs_refresh(0) {
            "expired, will refresh on next use".to_string()
        } else {
            format!("valid for ~{}m", secs / 60)
        };
        let has_refresh = if p.refresh_token.is_empty() {
            " [NO REFRESH TOKEN — re-login needed]"
        } else {
            ""
        };
        println!("    {name}: {state}{has_refresh}");
    }

    // Per-thread routing prerequisites (Linux: /proc, else lsof).
    #[cfg(target_os = "linux")]
    {
        let has_proc = std::path::Path::new("/proc/net/tcp").exists();
        let has_lsof = std::process::Command::new("lsof")
            .arg("-v")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if has_proc {
            println!("✓ per-thread routing via /proc");
        } else if has_lsof {
            println!("✓ per-thread routing via lsof");
        } else {
            ok = false;
            println!("✗ neither /proc/net/tcp nor lsof available — per-thread routing won't work");
        }
    }

    // Per-thread routing prerequisites (Windows: netstat / PowerShell).
    #[cfg(windows)]
    {
        let has_netstat = std::process::Command::new("netstat")
            .args(["-p", "TCP", "-n"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let has_powershell = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$PSVersionTable.PSVersion.Major",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if has_netstat || has_powershell {
            println!(
                "✓ per-thread routing via {}",
                if has_netstat { "netstat" } else { "PowerShell" }
            );
        } else {
            ok = false;
            println!("✗ neither netstat nor PowerShell available — per-thread routing won't work");
        }
    }

    // Skill.
    let skill = paths::claude_dir()?.join("skills/ccc/SKILL.md");
    if skill.exists() {
        println!("✓ agent skill installed");
    } else {
        println!("• agent skill not installed (run `ccc setup`)");
    }

    println!(
        "\n{}",
        if ok {
            "All core checks passed."
        } else {
            "Some checks failed — see above."
        }
    );
    Ok(())
}

fn cmd_daemon_start(port: u16) -> Result<()> {
    let desc = daemon::install_autostart(port)?;
    println!("✓ daemon started — {desc}");
    Ok(())
}

fn cmd_daemon_stop() -> Result<()> {
    daemon::uninstall_autostart()?;
    println!("✓ daemon stopped");
    Ok(())
}

/// Open a URL in the default browser, best-effort.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(url).spawn();
}

fn cmd_daemon_status() -> Result<()> {
    if daemon::is_running() {
        let rt = daemon::read_runtime().unwrap();
        println!("running: pid {}, port {}", rt.pid, rt.port);
    } else {
        println!("not running");
    }
    Ok(())
}
