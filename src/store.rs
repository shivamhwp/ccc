//! Account + token store, persisted encrypted to `~/.ccc/store.enc`.
//!
//! One entry per Claude account ("profile"). Tokens are claude.ai subscription
//! OAuth tokens — the same `sk-ant-oat…` / `sk-ant-ort…` blobs Claude Code
//! obtains via `/login`. No API keys are ever stored.
//!
//! At rest the store is sealed by the `vault` module (key in the Keychain or
//! a 0600 key file). A legacy plaintext `store.json` is still readable; the
//! first write migrates it to `store.enc` and shreds the plaintext. Every
//! successful write also rotates `store.enc.bak.1..3` so a corrupted or
//! deleted store is recoverable.

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths;
use crate::vault;

/// How many rotated backups of the encrypted store to keep.
const BACKUPS: u32 = 3;

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
        let enc_path = paths::store_enc_file()?;
        if let Ok(bytes) = std::fs::read(&enc_path) {
            if !bytes.is_empty() {
                let key = vault::master_key().context("resolving the store encryption key")?;
                let plain = vault::open(&key, &bytes).with_context(|| {
                    format!(
                        "reading {p} — if the file is corrupt, restore a backup: \
                         cp {p}.bak.1 {p}",
                        p = enc_path.display()
                    )
                })?;
                return serde_json::from_slice(&plain).context("parsing decrypted store");
            }
        }
        // Legacy plaintext store (pre-encryption, or not yet migrated).
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

    /// One-time migration: if only the legacy plaintext store exists, rewrite
    /// it as `store.enc` (a no-op update does load → sealed save → shred).
    /// Returns true when a migration happened.
    pub fn migrate_plaintext() -> Result<bool> {
        let legacy = paths::store_file()?;
        let enc = paths::store_enc_file()?;
        if !legacy.exists() || enc.exists() {
            return Ok(false);
        }
        Store::update(|_| Ok(()))?;
        Ok(true)
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
        let key = vault::master_key().context("resolving the store encryption key")?;
        let path = paths::store_enc_file()?;
        rotate_backups(&path);

        let tmp = path.with_extension("enc.tmp");
        let sealed = vault::seal(&key, &serde_json::to_vec_pretty(self)?)?;
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&sealed)?;
            f.sync_all()?;
        }
        paths::set_mode(&tmp, 0o600)?;
        std::fs::rename(&tmp, &path)?;
        paths::set_mode(&path, 0o600)?;

        // The sealed copy is durable; drop the legacy plaintext if present.
        if let Ok(legacy) = paths::store_file() {
            if legacy.exists() {
                shred(&legacy);
            }
        }
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

/// Rotate `store.enc` → `.bak.1` → `.bak.2` → `.bak.3` before a write, so the
/// last three pre-write states survive corruption or deletion. Best-effort:
/// a failed rotation must never block a token write. The live file is copied
/// (not renamed) so `store.enc` exists at every instant for concurrent reads.
fn rotate_backups(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    let bak = |i: u32| path.with_extension(format!("enc.bak.{i}"));
    for i in (1..BACKUPS).rev() {
        let _ = std::fs::rename(bak(i), bak(i + 1));
    }
    if std::fs::copy(path, bak(1)).is_ok() {
        let _ = paths::set_mode(&bak(1), 0o600);
    }
}

/// Best-effort destruction of the legacy plaintext store: overwrite with
/// zeros, sync, then remove — so the tokens don't linger on disk after the
/// encrypted copy takes over.
fn shred(path: &std::path::Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        if let Ok(mut f) = OpenOptions::new().write(true).open(path) {
            let _ = f.write_all(&vec![0u8; meta.len() as usize]);
            let _ = f.sync_all();
        }
    }
    let _ = std::fs::remove_file(path);
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_oauth_blob() {
        let v = serde_json::json!({
            "accessToken": "sk-ant-oat01-abc",
            "refreshToken": "sk-ant-ort01-def",
            "expiresAt": 1782967530954i64,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max"
        });
        let p = profile_from_oauth(&v).unwrap();
        assert_eq!(p.access_token, "sk-ant-oat01-abc");
        assert_eq!(p.refresh_token, "sk-ant-ort01-def");
        assert_eq!(p.expires_at, 1782967530954);
        assert_eq!(p.subscription_type.as_deref(), Some("max"));
        assert_eq!(p.scopes.len(), 2);
    }

    #[test]
    fn needs_refresh_respects_skew() {
        let mut p = profile_from_oauth(&serde_json::json!({
            "accessToken": "t", "refreshToken": "r", "expiresAt": 0
        }))
        .unwrap();
        // Already expired.
        assert!(p.needs_refresh(0));
        // Far future, no skew -> fresh.
        p.expires_at = now_ms() + 10 * 60 * 1000;
        assert!(!p.needs_refresh(0));
        // Within skew window -> needs refresh.
        assert!(p.needs_refresh(11 * 60 * 1000));
    }

    #[test]
    fn resolve_default_prefers_explicit_then_solo() {
        let mut s = Store {
            version: 1,
            default_profile: None,
            profiles: Default::default(),
        };
        let mk = || {
            profile_from_oauth(&serde_json::json!({
                "accessToken":"t","refreshToken":"r","expiresAt":0
            }))
            .unwrap()
        };
        s.profiles.insert("only".into(), mk());
        // Single profile resolves even without an explicit default.
        assert_eq!(s.resolve_default(), Some("only"));
        s.profiles.insert("second".into(), mk());
        // Ambiguous without explicit default.
        assert_eq!(s.resolve_default(), None);
        s.default_profile = Some("second".into());
        assert_eq!(s.resolve_default(), Some("second"));
    }
}
