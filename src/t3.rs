//! t3code integration.
//!
//! t3code (github.com/pingdotgg/t3code) stores provider instances in
//! `~/.t3/userdata/settings.json` under `providerInstances`, keyed by a
//! user-defined instance id. Each instance has a `driver` (`claudeAgent` here),
//! optional `displayName`/`accentColor`, and an `environment` array that t3code
//! merges into the spawned agent process.
//!
//! `ccc t3 sync` writes one instance per ccc account. Each instance gets its own
//! `CLAUDE_CONFIG_DIR` home seeded with that account's login (so t3code shows the
//! correct account + subscription — not "API key"), plus
//! `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/a/<account>` so the proxy supplies
//! the live, per-account token. The seeded credentials carry a far-future expiry
//! so Claude Code never refreshes them (the proxy owns the real token) — avoiding
//! any refresh-rotation conflict.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;

use crate::creds;
use crate::daemon;
use crate::oauth;
use crate::paths;
use crate::store::{Profile, Store};

fn t3_settings_file() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("CCC_T3_SETTINGS") {
        return Ok(PathBuf::from(p));
    }
    Ok(paths::home()?.join(".t3/userdata/settings.json"))
}

/// Per-account Claude config-dir home used by t3code instances.
fn account_home(account: &str) -> Result<PathBuf> {
    Ok(paths::ccc_dir()?.join("homes").join(account))
}

/// Instance id we use for a given ccc account.
fn instance_id(account: &str) -> String {
    format!("ccc_{account}")
}

const ACCENTS: [&str; 6] = [
    "#2563eb", "#7c3aed", "#059669", "#d97706", "#dc2626", "#0891b2",
];

/// Seed a per-account home with a login so Claude Code displays the right
/// account. Returns the home path. Enriches the store with fetched identity.
pub async fn provision_home(account: &str, profile: &Profile) -> Result<PathBuf> {
    // Resolve identity (email/org) — fetch it once if we don't have it yet.
    let mut email = profile.email.clone();
    let mut account_uuid = profile.account_uuid.clone();
    let mut org_uuid = profile.organization_uuid.clone();
    let mut org_name = profile.organization_name.clone();
    let mut sub = profile.subscription_type.clone();
    if email.is_none() || org_name.is_none() {
        if let Ok(info) = oauth::fetch_profile(&profile.access_token).await {
            email = email.or(info.email);
            account_uuid = account_uuid.or(info.account_uuid);
            org_uuid = org_uuid.or(info.organization_uuid);
            org_name = org_name.or(info.organization_name);
            sub = sub.or(info.subscription_type);
            // Persist enrichment so `ccc list` etc. show it too.
            let (a, e, au, ou, on, s) = (
                account.to_string(),
                email.clone(),
                account_uuid.clone(),
                org_uuid.clone(),
                org_name.clone(),
                sub.clone(),
            );
            let _ = Store::update(move |st| {
                if let Some(p) = st.profiles.get_mut(&a) {
                    p.email = e;
                    p.account_uuid = au;
                    p.organization_uuid = ou;
                    p.organization_name = on;
                    p.subscription_type = s;
                }
                Ok(())
            });
        }
    }

    let home = account_home(account)?;
    std::fs::create_dir_all(&home)?;
    paths::set_mode(&home, 0o700)?;

    // .claude.json — identity Claude Code shows immediately (before any fetch).
    let claude_json = serde_json::json!({
        "hasCompletedOnboarding": true,
        "installMethod": "native",
        "autoUpdates": false,
        "oauthAccount": {
            "emailAddress": email,
            "accountUuid": account_uuid,
            "organizationUuid": org_uuid,
            "organizationName": org_name,
            "organizationType": sub.as_ref().map(|s| format!("claude_{s}")),
        }
    });
    std::fs::write(
        home.join(".claude.json"),
        serde_json::to_vec_pretty(&claude_json)?,
    )?;

    // .credentials.json — satisfies the auth gate + drives the subscription
    // display; far-future expiry so Claude Code never refreshes it.
    let mut seeded = profile.clone();
    seeded.subscription_type = sub.clone().or(seeded.subscription_type);
    let cred = creds::oauth_json(&seeded, creds::FAR_FUTURE_MS);
    let cred_path = home.join(".credentials.json");
    std::fs::write(&cred_path, serde_json::to_vec(&cred)?)?;
    paths::set_mode(&cred_path, 0o600)?;

    Ok(home)
}

/// Upsert one t3code provider instance per ccc account. Returns the ids written.
pub async fn sync() -> Result<Vec<String>> {
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

    // Provision homes first (may fetch identity + persist to store).
    let mut homes = Vec::new();
    for (account, profile) in &store.profiles {
        let home = provision_home(account, profile).await?;
        homes.push((account.clone(), home));
    }

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
    for (i, (account, home)) in homes.iter().enumerate() {
        let id = instance_id(account);
        let account_base = format!("{base}/a/{account}");
        let home_str = home.to_string_lossy();
        let instance = serde_json::json!({
            "driver": "claudeAgent",
            "displayName": format!("claude · {account}"),
            "accentColor": ACCENTS[i % ACCENTS.len()],
            "enabled": true,
            // homePath sets HOME per instance. t3code keys its per-instance auth
            // display/capabilities cache on the resolved HOME, so a distinct
            // homePath is what makes t3code SHOW distinct accounts (otherwise it
            // collapses both instances onto one identity).
            "config": { "homePath": home_str },
            "environment": [
                // Explicit config dir → sha-suffixed Keychain service that doesn't
                // exist, so Claude Code uses this home's .credentials.json (correct
                // account) instead of the shared default Keychain login.
                { "name": "CLAUDE_CONFIG_DIR", "value": home_str, "sensitive": false },
                // Proxy supplies the live per-account token for actual traffic.
                { "name": "ANTHROPIC_BASE_URL", "value": account_base, "sensitive": false }
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

    // Remove the seeded per-account homes (they contain credentials).
    if let Ok(homes) = paths::ccc_dir().map(|d| d.join("homes")) {
        let _ = std::fs::remove_dir_all(homes);
    }
    Ok(removed)
}
