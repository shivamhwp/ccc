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
    use std::io::Write;
    // The item's account attribute matches the login user (Claude Code sets it
    // to $USER). `-U` updates the existing item in place, preserving its ACL so
    // Claude Code keeps silent read access to its own credentials. The command
    // goes through `security -i` stdin — as an argv the credential blob would
    // be visible in the process list for the duration of the write. The secret
    // is passed with `-X` (hex) so it needs no quoting for `security`'s
    // interactive parser regardless of its contents.
    let user = std::env::var("USER").unwrap_or_else(|_| "claude".into());
    if user.chars().any(char::is_control) {
        return Err(anyhow!(
            "refusing to write Keychain item: USER contains control characters"
        ));
    }
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let cmd = format!(
        "add-generic-password -U -s \"Claude Code-credentials\" -a {} -X {}\n",
        quote(&user),
        hex_encode(data.as_bytes()),
    );
    let mut child = std::process::Command::new("security")
        .arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running `security` to write Keychain")?;
    let write_res = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(cmd.as_bytes());
    // Reap the child before propagating a write error, or it lingers as a
    // zombie until the parent exits.
    let out = child.wait_with_output()?;
    write_res.context("sending credentials to `security`")?;
    if !out.status.success() {
        return Err(anyhow!(
            "writing Claude Code credentials to Keychain failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Uppercase hex for `security add-generic-password -X`, which takes the
/// password as a hex string.
#[cfg(any(target_os = "macos", test))]
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02X}");
            s
        })
}

#[cfg(not(target_os = "macos"))]
fn write_login_raw(data: &str) -> Result<()> {
    let path = paths::claude_dir()?.join(".credentials.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_secret_file(&path, data.as_bytes())
}

/// Outcome of [`reconcile`]: the relationship between Claude Code's stored
/// login and the ccc store.
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Debug)]
pub enum Reconcile {
    /// No Claude Code login stored at all.
    NoLogin,
    /// The stored login is a ccc seed — ownership is intact.
    Seeded,
    /// A live login matching a saved profile was re-imported and re-seeded.
    Healed(String),
    /// A live login for an account ccc doesn't know. Left untouched.
    Foreign { email: Option<String> },
}

/// Detect a live (non-seed) Claude Code login — the state `/login` leaves
/// behind — and re-take ownership when it belongs to a saved profile: adopt
/// its tokens into the store, then re-seed. Without this, Claude Code resumes
/// refreshing the grant and the rotation eventually strands the store's copy.
///
/// Called periodically by the daemon on Linux/Windows, where the login is a
/// plain file. Not called on macOS: the daemon must never touch the Keychain
/// (ACL prompts), so detection there is `ccc doctor` / `ccc setup` plus the
/// `invalid_grant` error hint.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn reconcile() -> Result<Reconcile> {
    let login = match read_login() {
        Ok(v) => v,
        Err(_) => return Ok(Reconcile::NoLogin),
    };
    if is_seeded(&login) {
        return Ok(Reconcile::Seeded);
    }

    // A live login. Identify the account (written by /login alongside the
    // credentials) and find the saved profile it belongs to.
    let identity = crate::store::read_claude_identity();
    let get = |k: &str| {
        identity
            .as_ref()
            .and_then(|id| id.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let (uuid, email) = (get("accountUuid"), get("emailAddress"));

    let store = crate::store::Store::load()?;
    let matched = store.profiles.iter().find_map(|(name, p)| {
        let by_uuid = uuid.is_some() && p.account_uuid == uuid;
        let by_email = email.is_some() && p.email == email;
        (by_uuid || by_email).then(|| name.clone())
    });
    let Some(name) = matched else {
        // Never adopt tokens for an account we can't attribute — that would
        // overwrite a saved profile with a stranger's login.
        return Ok(Reconcile::Foreign { email });
    };

    let mut live = crate::store::profile_from_oauth(&login)?;
    let old = &store.profiles[&name];
    live.email = email.or_else(|| old.email.clone());
    live.account_uuid = uuid.or_else(|| old.account_uuid.clone());
    live.organization_uuid = old.organization_uuid.clone();
    live.organization_name = old.organization_name.clone();
    if live.subscription_type.is_none() {
        live.subscription_type = old.subscription_type.clone();
    }

    // Store first, seed second — same ordering as `ccc import`: a failure
    // between the two leaves Claude Code owning a login the store also has,
    // which the next reconcile pass repairs.
    let seeded = live.clone();
    let profile_name = name.clone();
    crate::store::Store::update(move |s| {
        s.profiles.insert(profile_name, live);
        Ok(())
    })?;
    write_login(&seeded, FAR_FUTURE_MS)?;
    Ok(Reconcile::Healed(name))
}

/// Write a credentials file created with 0600 from the start — `fs::write`
/// followed by chmod would leave a window where the default umask applies.
pub fn write_secret_file(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(data)
        .with_context(|| format!("writing {}", path.display()))?;
    // mode() only applies on create; tighten pre-existing files too.
    crate::paths::set_mode(path, 0o600)?;
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
    fn hex_encode_is_plain_hex_for_any_input() {
        assert_eq!(hex_encode(b"{\"a\":1}"), "7B2261223A317D");
        // Newlines, quotes, and backslashes — everything the `security -i`
        // parser could trip on — come out as [0-9A-F] only.
        let hex = hex_encode("evil\"\\\ntoken".as_bytes());
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
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
