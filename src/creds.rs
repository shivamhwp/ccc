//! Claude Code credential storage — read, seed, restore.
//!
//! ccc is the sole owner of every saved account's refresh token, including the
//! default account. Anthropic rotates refresh tokens on use, so a token with
//! two owners (Claude Code's own storage and ccc's store) breaks whichever
//! side refreshes second. To prevent that, `ccc setup` / `ccc import` copy the
//! live login into the ccc store once and then overwrite Claude Code's storage
//! with a "seeded" copy carrying a far-future expiry. Claude Code treats the
//! seed as valid forever and never refreshes it; the proxy injects the real,
//! ccc-refreshed token on every request. `ccc teardown` writes the live
//! tokens back so Claude Code resumes owning its login.
//!
//! Storage location: macOS Keychain item `Claude Code-credentials` (login
//! keychain); `<claude_dir>/.credentials.json` on Linux and Windows.

use anyhow::{anyhow, Context, Result};

#[cfg(not(target_os = "macos"))]
use crate::paths;
use crate::store::Profile;

/// Far-future expiry (year 2100) marking seeded credentials. Claude Code sees
/// a token that never expires, so it never refreshes and never writes back.
pub const FAR_FUTURE_MS: i64 = 4_102_444_800_000;

/// The `claudeAiOauth` JSON blob Claude Code stores. `expires_at` is the value
/// written; pass [`FAR_FUTURE_MS`] to seed, or the profile's real expiry to
/// restore ownership to Claude Code.
pub fn oauth_json(profile: &Profile, expires_at: i64) -> serde_json::Value {
    serde_json::json!({
        "claudeAiOauth": {
            "accessToken": profile.access_token,
            "refreshToken": profile.refresh_token,
            "expiresAt": expires_at,
            "scopes": profile.scopes,
            "subscriptionType": profile
                .subscription_type
                .clone()
                .unwrap_or_else(|| "max".into()),
        }
    })
}

/// True when a `claudeAiOauth` object is a ccc seed (not a live login).
pub fn is_seeded(oauth: &serde_json::Value) -> bool {
    oauth.get("expiresAt").and_then(|v| v.as_i64()) == Some(FAR_FUTURE_MS)
}

/// Read the current Claude Code login. Returns the raw `claudeAiOauth` object
/// (which may be a ccc seed — check with [`is_seeded`]).
#[cfg(target_os = "macos")]
pub fn read_login() -> Result<serde_json::Value> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .context("running `security` to read Keychain")?;
    if !out.status.success() {
        return Err(anyhow!(
            "no Claude Code login found in Keychain (run `claude` and /login first)"
        ));
    }
    let text = String::from_utf8(out.stdout)?;
    let v: serde_json::Value = serde_json::from_str(text.trim())?;
    v.get("claudeAiOauth")
        .cloned()
        .ok_or_else(|| anyhow!("Keychain entry missing claudeAiOauth"))
}

#[cfg(not(target_os = "macos"))]
pub fn read_login() -> Result<serde_json::Value> {
    let path = paths::claude_dir()?.join(".credentials.json");
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)?;
    v.get("claudeAiOauth")
        .cloned()
        .ok_or_else(|| anyhow!("credentials file missing claudeAiOauth"))
}

/// Overwrite Claude Code's credential storage with `profile`'s tokens and the
/// given expiry. Interactive contexts only (macOS may show one Keychain
/// prompt) — never call this from the daemon.
pub fn write_login(profile: &Profile, expires_at: i64) -> Result<()> {
    let root = oauth_json(profile, expires_at);
    let data = serde_json::to_string(&root)?;
    write_login_raw(&data)
}

#[cfg(target_os = "macos")]
fn write_login_raw(data: &str) -> Result<()> {
    // The item's account attribute matches the login user (Claude Code sets it
    // to $USER). `-U` updates the existing item in place, preserving its ACL so
    // Claude Code keeps silent read access to its own credentials.
    let user = std::env::var("USER").unwrap_or_else(|_| "claude".into());
    let out = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            "Claude Code-credentials",
            "-a",
            &user,
            "-w",
            data,
        ])
        .output()
        .context("running `security` to write Keychain")?;
    if !out.status.success() {
        return Err(anyhow!(
            "writing Claude Code credentials to Keychain failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn write_login_raw(data: &str) -> Result<()> {
    let path = paths::claude_dir()?.join(".credentials.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
    paths::set_mode(&path, 0o600)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Profile {
        Profile {
            access_token: "sk-ant-oat-x".into(),
            refresh_token: "sk-ant-ort-x".into(),
            expires_at: 1_700_000_000_000,
            scopes: vec!["user:inference".into()],
            subscription_type: None,
            email: None,
            account_uuid: None,
            organization_uuid: None,
            organization_name: None,
        }
    }

    #[test]
    fn seed_is_detected_and_live_is_not() {
        let p = profile();
        let seed = oauth_json(&p, FAR_FUTURE_MS);
        assert!(is_seeded(seed.get("claudeAiOauth").unwrap()));
        let live = oauth_json(&p, p.expires_at);
        assert!(!is_seeded(live.get("claudeAiOauth").unwrap()));
    }
}
