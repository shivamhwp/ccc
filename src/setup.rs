//! `ccc setup`: wire ccc into Claude Code and teach agents to use it.
//!
//! Two idempotent actions:
//!   1. Set `env.ANTHROPIC_BASE_URL` in `<claude_dir>/settings.json` so every
//!      thread routes through the proxy.
//!   2. Install a skill at `<claude_dir>/skills/ccc/SKILL.md` so agents know
//!      what "using ccc" means and how to switch accounts.

use anyhow::{Context, Result};
use std::io::Write;

use crate::daemon;
use crate::paths;

/// Patch settings.json to point Claude Code at the proxy. Returns the value set.
pub fn patch_settings(base_url: &str) -> Result<String> {
    let path = paths::claude_settings_file()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut root: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) if !bytes.is_empty() => {
            serde_json::from_slice(&bytes).context("parsing existing settings.json")?
        }
        _ => serde_json::json!({}),
    };

    if !root.is_object() {
        anyhow::bail!("settings.json is not a JSON object");
    }
    let obj = root.as_object_mut().unwrap();
    let env = obj.entry("env").or_insert_with(|| serde_json::json!({}));
    if !env.is_object() {
        *env = serde_json::json!({});
    }
    let env_obj = env.as_object_mut().unwrap();
    env_obj.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        serde_json::json!(base_url),
    );
    // Claude Code has a local auth gate: it will not send any request unless it
    // believes it is authenticated. A placeholder auth token satisfies that gate
    // so every thread reaches the proxy; the proxy always overwrites the
    // Authorization header with the routed account's real subscription token, so
    // this value is never used for anything.
    env_obj.insert(
        "ANTHROPIC_AUTH_TOKEN".to_string(),
        serde_json::json!("ccc-managed-by-proxy"),
    );

    // Back up the previous file once per run before overwriting.
    if path.exists() {
        let backup = path.with_extension("json.ccc-bak");
        let _ = std::fs::copy(&path, &backup);
    }

    let data = serde_json::to_vec_pretty(&root)?;
    let mut f = std::fs::File::create(&path)?;
    f.write_all(&data)?;
    Ok(base_url.to_string())
}

/// Remove the env overrides we added (leaves all other settings intact).
pub fn unpatch_settings() -> Result<()> {
    let path = paths::claude_settings_file()?;
    let bytes = match std::fs::read(&path) {
        Ok(b) if !b.is_empty() => b,
        _ => return Ok(()),
    };
    let mut root: serde_json::Value = serde_json::from_slice(&bytes)?;
    if let Some(env) = root.get_mut("env").and_then(|e| e.as_object_mut()) {
        env.remove("ANTHROPIC_BASE_URL");
        // Only remove the auth token if it's our placeholder.
        if env.get("ANTHROPIC_AUTH_TOKEN").and_then(|v| v.as_str()) == Some("ccc-managed-by-proxy")
        {
            env.remove("ANTHROPIC_AUTH_TOKEN");
        }
    }
    let data = serde_json::to_vec_pretty(&root)?;
    std::fs::write(&path, data)?;
    Ok(())
}

const SKILL_BODY: &str = r#"---
name: ccc
description: Switch which Claude account (subscription) the current Claude Code thread bills to. Use when the user says "use the <name> account", "switch to <name> with ccc", "bill this to <name>", or otherwise names a ccc account/profile to act as. Applies to the current thread only.
---

# ccc — per-thread Claude account switching

`ccc` routes this thread's API traffic through a local proxy that authenticates
as one of several saved Claude subscription accounts. Switching takes effect
within a few seconds, no restart needed, and only affects the current thread.

## When the user names an account

Run the switch, then continue with their actual task:

```
ccc use <name>
```

- `<name>` is a saved profile (see `ccc list`). If the user's word isn't an
  exact profile name, run `ccc list` and pick the obvious match; if ambiguous,
  ask.
- After `ccc use`, proceed with the rest of the user's request normally. The
  account change is already live for subsequent model calls in this thread.

## Useful commands

- `ccc list` — show saved accounts and which is default.
- `ccc whoami` — show which account THIS thread is currently using.
- `ccc use <name>` — bill this thread to `<name>` from now on.
- `ccc use --default` — revert this thread to the default account.

## Notes

- Switching is per-thread: other Claude Code threads are unaffected.
- Only subscription accounts are supported; there are no API keys involved.
- If a command reports the daemon isn't running, run `ccc daemon status` and
  tell the user (they may need `ccc daemon start`).
"#;

/// Install the ccc skill into the Claude Code skills directory.
pub fn install_skill() -> Result<std::path::PathBuf> {
    let dir = paths::claude_dir()?.join("skills").join("ccc");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, SKILL_BODY)?;
    Ok(path)
}

/// Remove the ccc skill directory from Claude Code.
pub fn remove_skill() -> Result<()> {
    let dir = paths::claude_dir()?.join("skills").join("ccc");
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// The base URL ccc will write into settings.json (the running daemon's, or the
/// default port if not yet running).
#[allow(dead_code)]
pub fn effective_base_url() -> String {
    daemon::base_url()
}
