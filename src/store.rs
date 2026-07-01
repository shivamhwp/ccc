//! Account + token store, persisted to `~/.ccc/store.json` (mode 0600).
//!
//! One entry per Claude account ("profile"). Tokens are claude.ai subscription
//! OAuth tokens — the same `sk-ant-oat…` / `sk-ant-ort…` blobs Claude Code
//! obtains via `/login`. No API keys are ever stored.

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Subscription access token (`sk-ant-oat…`).
    pub access_token: String,
    /// Refresh token (`sk-ant-ort…`).
    pub refresh_token: String,
    /// Absolute expiry, epoch milliseconds.
    pub expires_at: i64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub subscription_type: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub account_uuid: Option<String>,
    #[serde(default)]
    pub organization_uuid: Option<String>,
    #[serde(default)]
    pub organization_name: Option<String>,
}

impl Profile {
    /// True when the access token is expired or within `skew_ms` of expiring.
    pub fn needs_refresh(&self, skew_ms: i64) -> bool {
        now_ms() + skew_ms >= self.expires_at
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Store {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

fn one() -> u32 {
    1
}

impl Store {
    pub fn load() -> Result<Store> {
        let path = paths::store_file()?;
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                serde_json::from_slice(&bytes).context("parsing ~/.ccc/store.json")
            }
            _ => Ok(Store {
                version: 1,
                ..Default::default()
            }),
        }
    }

    /// Load, mutate under an exclusive file lock, and persist atomically.
    /// This is the only safe way to write the store when the daemon and CLI
    /// may run concurrently.
    pub fn update<T>(f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        paths::ensure_ccc_dir()?;
        let lock_path = paths::ccc_dir()?.join(".store.lock");
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        lock.lock_exclusive()?;

        let result = (|| {
            let mut store = Store::load()?;
            let out = f(&mut store)?;
            store.save_atomic()?;
            Ok(out)
        })();

        let _ = FileExt::unlock(&lock);
        result
    }

    fn save_atomic(&self) -> Result<()> {
        let path = paths::store_file()?;
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self)?;
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
        }
        paths::set_mode(&tmp, 0o600)?;
        std::fs::rename(&tmp, &path)?;
        paths::set_mode(&path, 0o600)?;
        Ok(())
    }

    pub fn resolve_default(&self) -> Option<&str> {
        self.default_profile
            .as_deref()
            .filter(|n| self.profiles.contains_key(*n))
            .or_else(|| {
                if self.profiles.len() == 1 {
                    self.profiles.keys().next().map(|s| s.as_str())
                } else {
                    None
                }
            })
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Read the current Claude Code login from the macOS Keychain, if present.
/// Returns the raw `claudeAiOauth` object.
#[cfg(target_os = "macos")]
pub fn read_keychain_login() -> Result<serde_json::Value> {
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
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
pub fn read_keychain_login() -> Result<serde_json::Value> {
    // Linux/Windows: Claude Code stores a plaintext credentials file.
    let path = paths::claude_dir()?.join(".credentials.json");
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)?;
    v.get("claudeAiOauth")
        .cloned()
        .ok_or_else(|| anyhow!("credentials file missing claudeAiOauth"))
}

/// Build a Profile from a `claudeAiOauth` JSON object.
pub fn profile_from_oauth(v: &serde_json::Value) -> Result<Profile> {
    let access_token = v
        .get("accessToken")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("missing accessToken"))?
        .to_string();
    let refresh_token = v
        .get("refreshToken")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let expires_at = v.get("expiresAt").and_then(|x| x.as_i64()).unwrap_or(0);
    let scopes = v
        .get("scopes")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let subscription_type = v
        .get("subscriptionType")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Ok(Profile {
        access_token,
        refresh_token,
        expires_at,
        scopes,
        subscription_type,
        email: None,
        account_uuid: None,
        organization_uuid: None,
        organization_name: None,
    })
}

/// Best-effort read of account identity from `~/.claude.json` (oauthAccount).
pub fn read_claude_identity() -> Option<serde_json::Value> {
    let path = paths::home().ok()?.join(".claude.json");
    let mut f = File::open(path).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("oauthAccount").cloned()
}
