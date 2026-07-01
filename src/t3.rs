//! t3code integration.
//!
//! t3code (github.com/pingdotgg/t3code) stores provider instances in
//! `~/.t3/userdata/settings.json` under `providerInstances`, keyed by a
//! user-defined instance id. Each instance has a `driver` (`claudeAgent` here),
//! optional `displayName`/`accentColor`, and an `environment` array that t3code
//! merges into the spawned agent process.
//!
//! `ccc t3 sync` writes one instance per ccc account, each pinned to its account
//! via `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/a/<account>`. The proxy reads
//! that `/a/<account>` prefix and authenticates as that account — so in t3code
//! the account becomes a per-instance choice in the UI, no PID routing needed.

use anyhow::{Context, Result};
use std::io::Write;

use crate::daemon;
use crate::paths;
use crate::store::Store;

fn t3_settings_file() -> Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("CCC_T3_SETTINGS") {
        return Ok(std::path::PathBuf::from(p));
    }
    Ok(paths::home()?.join(".t3/userdata/settings.json"))
}

/// Instance id we use for a given ccc account.
fn instance_id(account: &str) -> String {
    format!("ccc_{account}")
}

const ACCENTS: [&str; 6] = [
    "#2563eb", "#7c3aed", "#059669", "#d97706", "#dc2626", "#0891b2",
];

/// Upsert one t3code provider instance per ccc account. Returns the ids written.
pub fn sync() -> Result<Vec<String>> {
    let store = Store::load()?;
    if store.profiles.is_empty() {
        anyhow::bail!("no ccc accounts saved yet (run `ccc login <name>` or `ccc import`)");
    }

    let path = t3_settings_file()?;
    if !path.exists() {
        anyhow::bail!(
            "t3code settings not found at {}. Is t3code installed and run at least once?",
            path.display()
        );
    }

    let base = daemon::base_url(); // http://127.0.0.1:<port>

    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut root: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing t3code settings.json")?;
    if !root.is_object() {
        anyhow::bail!("t3code settings.json is not a JSON object");
    }

    // Back up once before writing.
    let backup = path.with_extension("json.ccc-bak");
    let _ = std::fs::copy(&path, &backup);

    let obj = root.as_object_mut().unwrap();
    let instances = obj
        .entry("providerInstances")
        .or_insert_with(|| serde_json::json!({}));
    if !instances.is_object() {
        *instances = serde_json::json!({});
    }
    let instances = instances.as_object_mut().unwrap();

    let mut written = Vec::new();
    for (i, account) in store.profiles.keys().enumerate() {
        let id = instance_id(account);
        let account_base = format!("{base}/a/{account}");
        let display = format!("claude · {account}");
        let instance = serde_json::json!({
            "driver": "claudeAgent",
            "displayName": display,
            "accentColor": ACCENTS[i % ACCENTS.len()],
            "enabled": true,
            "environment": [
                { "name": "ANTHROPIC_BASE_URL", "value": account_base, "sensitive": false },
                // Placeholder to satisfy Claude Code's local auth gate; the proxy
                // overwrites the Authorization header with the real token.
                { "name": "ANTHROPIC_AUTH_TOKEN", "value": "ccc-managed-by-proxy", "sensitive": false }
            ]
        });
        instances.insert(id.clone(), instance);
        written.push(id);
    }

    let data = serde_json::to_vec_pretty(&root)?;
    let mut f = std::fs::File::create(&path)?;
    f.write_all(&data)?;
    Ok(written)
}

/// Remove all ccc-managed instances from t3code settings.
pub fn unsync() -> Result<usize> {
    let path = t3_settings_file()?;
    let bytes = match std::fs::read(&path) {
        Ok(b) if !b.is_empty() => b,
        _ => return Ok(0),
    };
    let mut root: serde_json::Value = serde_json::from_slice(&bytes)?;
    let mut removed = 0;
    if let Some(instances) = root
        .get_mut("providerInstances")
        .and_then(|v| v.as_object_mut())
    {
        let keys: Vec<String> = instances
            .keys()
            .filter(|k| k.starts_with("ccc_"))
            .cloned()
            .collect();
        for k in keys {
            instances.remove(&k);
            removed += 1;
        }
    }
    let data = serde_json::to_vec_pretty(&root)?;
    std::fs::write(&path, data)?;
    Ok(removed)
}
