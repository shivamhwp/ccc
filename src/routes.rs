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
    /// Start token of the routed process, recorded by `ccc use`. Guards
    /// against pid reuse: the OS recycles pids, and a stale route must not
    /// attach an account to an unrelated new process with the same number.
    #[serde(default)]
    pub started: Option<String>,
}

/// True when `route` still refers to the same live process it was set for.
/// Legacy routes without a start token fall back to a liveness check.
fn still_owns(pid: u32, route: &Route) -> bool {
    match &route.started {
        Some(token) => procinfo::pid_start_token(pid).as_deref() == Some(token.as_str()),
        None => procinfo::pid_alive(pid),
    }
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
        // Fast paths first: building the ancestor chain costs a process-table
        // scan (`ps` / PowerShell), which is pure waste when the table is
        // empty and unnecessary on a direct hit — the common case, since
        // requests come from the routed claude pid itself.
        if self.routes.is_empty() {
            return None;
        }
        if let Some(r) = self.routes.get(&claude_pid.to_string()) {
            if still_owns(claude_pid, r) {
                return Some(r.profile.clone());
            }
        }
        let chain = procinfo::ancestors(claude_pid);
        for pid in chain {
            if let Some(r) = self.routes.get(&pid.to_string()) {
                if still_owns(pid, r) {
                    return Some(r.profile.clone());
                }
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
    routes.routes.retain(|pid, route| {
        pid.parse::<u32>()
            .map(|p| still_owns(p, route))
            .unwrap_or(false)
    });
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
                        started: procinfo::pid_start_token(pid),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procinfo;

    fn route(profile: &str, started: Option<String>) -> Route {
        Route {
            profile: profile.into(),
            set_at: 0,
            started,
        }
    }

    #[test]
    fn still_owns_requires_matching_start_token() {
        let me = procinfo::self_pid();
        // Correct token: the route still owns the pid.
        assert!(still_owns(me, &route("x", procinfo::pid_start_token(me))));
        // Recycled pid (token mismatch): it doesn't.
        assert!(!still_owns(me, &route("x", Some("bogus-token".into()))));
        // Legacy route without a token: liveness is enough.
        assert!(still_owns(me, &route("x", None)));
    }

    #[test]
    fn resolve_skips_route_for_recycled_pid() {
        let me = procinfo::self_pid();
        let mut routes = Routes::default();
        routes
            .routes
            .insert(me.to_string(), route("stale", Some("bogus-token".into())));
        assert_eq!(routes.resolve_for(me), None);

        // Same entry with the real token resolves.
        routes
            .routes
            .insert(me.to_string(), route("live", procinfo::pid_start_token(me)));
        assert_eq!(routes.resolve_for(me), Some("live".into()));
    }
}
