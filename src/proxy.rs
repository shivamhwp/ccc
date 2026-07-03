//! The auth-rewriting reverse proxy.
//!
//! Every Claude Code thread points `ANTHROPIC_BASE_URL` at this daemon. For
//! each request the proxy:
//!   1. attributes the inbound connection to a claude PID (via source port),
//!   2. resolves that PID (and its ancestors) to a profile in routes.json,
//!      falling back to the default profile,
//!   3. ensures that profile's token is fresh (refreshing under a per-profile
//!      lock if needed),
//!   4. forwards upstream with `Authorization: Bearer <subscription token>`
//!      plus the `anthropic-beta: oauth-2025-04-20` header.
//!
//! The proxy injects the token on EVERY request — including the default
//! account. ccc's store is the sole owner of all refresh tokens (Claude Code
//! runs on seeded, never-refreshing credentials; see `creds`), so the daemon
//! never reads the Keychain and no token can rotate out from under it.

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::oauthcfg;
use crate::procinfo;
use crate::routes::Routes;
use crate::store::Store;
use crate::{daemon, oauth};

/// Refresh when within this many ms of expiry.
const REFRESH_SKEW_MS: i64 = 120_000;

/// After a failed refresh, don't retry the token endpoint for this long.
/// `invalid_grant` is permanent until the user re-auths — without this, every
/// request would hammer the endpoint with a known-dead refresh token.
const REFRESH_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    self_pid: u32,
    /// Emit a per-request routing line to stderr (set `CCC_LOG=1`).
    log: bool,
    /// Per-profile locks to serialize token refresh.
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Per-profile time of the last failed refresh, for backoff.
    refresh_failures: Arc<Mutex<HashMap<String, std::time::Instant>>>,
}

/// Run the proxy on `port` until the process is terminated.
pub async fn run(port: u16) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("ccc-proxy/0.1")
        .build()
        .context("building upstream client")?;

    let state = AppState {
        client,
        self_pid: procinfo::self_pid(),
        log: std::env::var("CCC_LOG").is_ok(),
        locks: Arc::new(Mutex::new(HashMap::new())),
        refresh_failures: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/_ccc/health", any(health))
        .fallback(any(proxy))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding 127.0.0.1:{port}"))?;
    let bound = listener.local_addr()?;

    daemon::write_runtime(bound.port())?;
    eprintln!("ccc proxy listening on http://{bound}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serving proxy")?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ccc ok")
}

async fn proxy(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
    req: axum::http::Request<Body>,
) -> Response {
    match proxy_inner(peer, state, req).await {
        Ok(resp) => resp,
        Err(e) => {
            let msg = format!("ccc proxy error: {e:#}");
            eprintln!("[{}] {msg}", utc_now());
            (StatusCode::BAD_GATEWAY, msg).into_response()
        }
    }
}

async fn proxy_inner(
    peer: SocketAddr,
    state: AppState,
    req: axum::http::Request<Body>,
) -> Result<Response> {
    let (parts, body) = req.into_parts();
    let method: Method = parts.method;
    let uri: Uri = parts.uri;
    let in_headers: HeaderMap = parts.headers;

    // A `/a/<account>` path prefix pins the account explicitly (used by t3code,
    // where each provider instance sets its own base URL). It takes precedence
    // over PID-based routing. Otherwise fall back to PID → default.
    let (pin, fwd_path) = extract_account_pin(uri.path());
    let store = Store::load()?;
    let (profile_name, matched_pid) = match pin {
        Some(ref name) if store.profiles.contains_key(name) => (name.clone(), None),
        _ => resolve_profile(peer.port(), state.self_pid)?,
    };

    if state.log {
        eprintln!(
            "[ccc] {} {} pid={} profile={}{}",
            method,
            uri.path(),
            matched_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".into()),
            profile_name,
            if pin.is_some() { " (pinned)" } else { "" },
        );
    }

    // Build upstream request (with any `/a/<account>` prefix stripped).
    let path_and_query = match uri.query() {
        Some(q) => format!("{fwd_path}?{q}"),
        None => fwd_path,
    };
    let url = format!("{}{}", oauthcfg::upstream_base(), path_and_query);

    let mut fwd = HeaderMap::new();
    for (name, value) in in_headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        fwd.insert(name.clone(), value.clone());
    }
    fwd.remove(axum::http::header::HOST);

    // Always inject the resolved profile's token. Callers run on seeded (stale
    // by design) credentials, so their own Authorization header is never valid
    // upstream.
    let token = ensure_fresh_token(&state, &profile_name).await?;
    fwd.remove("x-api-key");
    fwd.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    inject_beta(&mut fwd)?;

    let body_stream = body.into_data_stream();
    let reqwest_body = reqwest::Body::wrap_stream(body_stream);

    let upstream = state
        .client
        .request(method, &url)
        .headers(fwd)
        .body(reqwest_body)
        .send()
        .await
        .with_context(|| format!("forwarding to {url}"))?;

    // Stream response back.
    let status = upstream.status();
    let mut resp_headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        resp_headers.insert(name.clone(), value.clone());
    }
    let out_stream = upstream.bytes_stream();
    let out_body = Body::from_stream(out_stream);

    let mut response = Response::new(out_body);
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;
    Ok(response)
}

/// Split off a leading `/a/<account>` prefix. Returns the pinned account name
/// (if any) and the path to forward upstream. `/a/work/v1/messages` →
/// `(Some("work"), "/v1/messages")`; `/v1/messages` → `(None, "/v1/messages")`.
fn extract_account_pin(path: &str) -> (Option<String>, String) {
    if let Some(rest) = path.strip_prefix("/a/") {
        let (name, tail) = match rest.split_once('/') {
            Some((n, t)) => (n, format!("/{t}")),
            None => (rest, "/".to_string()),
        };
        if !name.is_empty() {
            return (Some(name.to_string()), tail);
        }
    }
    (None, path.to_string())
}

/// Resolve the profile for an inbound connection from `peer_port`. Returns the
/// profile name and the claude PID it was attributed to (if any).
fn resolve_profile(peer_port: u16, self_pid: u32) -> Result<(String, Option<u32>)> {
    let routes = Routes::load()?;
    let store = Store::load()?;

    // Try to attribute the connection to a claude PID and its route.
    let owner = procinfo::pid_owning_local_port(peer_port, self_pid);
    if let Some(pid) = owner {
        if let Some(profile) = routes.resolve_for(pid) {
            if store.profiles.contains_key(&profile) {
                return Ok((profile, Some(pid)));
            }
        }
    }

    store
        .resolve_default()
        .map(|s| (s.to_string(), owner))
        .context("no route matched and no default profile is set (run `ccc use <name>` or `ccc default <name>`)")
}

/// Return a currently-valid access token for `profile`, refreshing if needed.
/// The ccc store is the single source of truth for every profile, default
/// included — the daemon never touches Claude Code's own credential storage.
async fn ensure_fresh_token(state: &AppState, profile: &str) -> Result<String> {
    {
        let store = Store::load()?;
        let p = store
            .profiles
            .get(profile)
            .with_context(|| format!("profile `{profile}` not found"))?;
        if !p.needs_refresh(REFRESH_SKEW_MS) {
            return Ok(p.access_token.clone());
        }
    }

    // Needs refresh: take the per-profile lock so concurrent requests refresh once.
    let lock = {
        let mut locks = state.locks.lock().await;
        locks
            .entry(profile.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    // Re-check after acquiring the lock — another task may have refreshed.
    let current = {
        let store = Store::load()?;
        store
            .profiles
            .get(profile)
            .cloned()
            .with_context(|| format!("profile `{profile}` disappeared"))?
    };
    if !current.needs_refresh(REFRESH_SKEW_MS) {
        return Ok(current.access_token);
    }

    if let Some(failed_at) = state.refresh_failures.lock().await.get(profile) {
        if failed_at.elapsed() < REFRESH_BACKOFF {
            anyhow::bail!(
                "token refresh for `{profile}` failed recently; backing off. \
                 If this persists, re-auth with `ccc login {profile}`"
            );
        }
    }

    match oauth::refresh(&current).await {
        Ok(refreshed) => {
            state.refresh_failures.lock().await.remove(profile);
            let token = refreshed.access_token.clone();
            let profile_owned = profile.to_string();
            Store::update(move |s| {
                s.profiles.insert(profile_owned, refreshed);
                Ok(())
            })?;
            Ok(token)
        }
        Err(e) => {
            state
                .refresh_failures
                .lock()
                .await
                .insert(profile.to_string(), std::time::Instant::now());
            Err(e.context(format!(
                "refreshing token for `{profile}` (re-auth with `ccc login {profile}` if this persists)"
            )))
        }
    }
}

/// Current UTC time as `YYYY-MM-DD HH:MM:SS` — enough for correlating log
/// lines without pulling in a date dependency.
fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil date from day count (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

fn inject_beta(headers: &mut HeaderMap) -> Result<()> {
    let name = HeaderName::from_static("anthropic-beta");
    let merged = match headers.get(&name).and_then(|v| v.to_str().ok()) {
        Some(existing) if existing.contains(oauthcfg::OAUTH_BETA) => existing.to_string(),
        Some(existing) if !existing.is_empty() => {
            format!("{existing},{}", oauthcfg::OAUTH_BETA)
        }
        _ => oauthcfg::OAUTH_BETA.to_string(),
    };
    headers.insert(name, HeaderValue::from_str(&merged)?);
    Ok(())
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

#[cfg(test)]
mod tests {
    use super::extract_account_pin;

    #[test]
    fn pins_account_from_path_prefix() {
        assert_eq!(
            extract_account_pin("/a/work/v1/messages"),
            (Some("work".into()), "/v1/messages".into())
        );
    }

    #[test]
    fn no_prefix_passes_through() {
        assert_eq!(
            extract_account_pin("/v1/messages"),
            (None, "/v1/messages".into())
        );
    }

    #[test]
    fn bare_account_prefix_forwards_root() {
        assert_eq!(
            extract_account_pin("/a/work"),
            (Some("work".into()), "/".into())
        );
    }
}
