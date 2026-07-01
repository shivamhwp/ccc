//! Per-thread routing table: `~/.ccc/routes.json`.
//!
//! Maps a claude PID to the profile name its requests should authenticate as.
//! Written by `ccc use`, read by the proxy on each request. Dead PIDs are
//! garbage-collected on read.

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;

use crate::paths;
use crate::procinfo;
use crate::store::now_ms;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub profile: String,
    pub set_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Routes {
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
}

impl Routes {
    pub fn load() -> Result<Routes> {
        let path = paths::routes_file()?;
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                Ok(serde_json::from_slice(&bytes).unwrap_or_default())
            }
            _ => Ok(Routes::default()),
        }
    }

    /// Resolve the profile for a request originating from `claude_pid`, by
    /// checking `claude_pid` and each of its ancestors against the table.
    pub fn resolve_for(&self, claude_pid: u32) -> Option<String> {
        let chain = procinfo::ancestors(claude_pid);
        for pid in chain {
            if let Some(r) = self.routes.get(&pid.to_string()) {
                return Some(r.profile.clone());
            }
        }
        None
    }

    fn save_atomic(&self) -> Result<()> {
        let path = paths::routes_file()?;
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
        Ok(())
    }
}

/// Mutate the routes file under an exclusive lock, GCing dead pids first.
pub fn update<T>(f: impl FnOnce(&mut Routes) -> Result<T>) -> Result<T> {
    paths::ensure_ccc_dir()?;
    let lock_path = paths::ccc_dir()?.join(".routes.lock");
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    lock.lock_exclusive()?;

    let result = (|| {
        let mut routes = Routes::load()?;
        gc(&mut routes);
        let out = f(&mut routes)?;
        routes.save_atomic()?;
        Ok(out)
    })();

    let _ = FileExt::unlock(&lock);
    result
}

fn gc(routes: &mut Routes) {
    routes
        .routes
        .retain(|pid, _| pid.parse::<u32>().map(procinfo::pid_alive).unwrap_or(false));
}

/// Set (or clear) the route for a specific pid.
pub fn set_route(pid: u32, profile: Option<&str>) -> Result<()> {
    update(|routes| {
        match profile {
            Some(p) => {
                routes.routes.insert(
                    pid.to_string(),
                    Route {
                        profile: p.to_string(),
                        set_at: now_ms(),
                    },
                );
            }
            None => {
                routes.routes.remove(&pid.to_string());
            }
        }
        Ok(())
    })
    .context("updating routes")
}
