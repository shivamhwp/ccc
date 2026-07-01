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

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    self_pid: u32,
    /// Emit a per-request routing line to stderr (set `CCC_LOG=1`).
    log: bool,
    /// Per-profile locks to serialize token refresh.
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
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
            eprintln!("{msg}");
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

    // 1 + 2: attribute connection to a profile.
    let (profile_name, matched_pid) = resolve_profile(peer.port(), state.self_pid)?;
    if state.log {
        eprintln!(
            "[ccc] {} {} pid={} profile={}",
            method,
            uri.path(),
            matched_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".into()),
            profile_name
        );
    }

    // 3: ensure a fresh token for that profile.
    let token = ensure_fresh_token(&state, &profile_name).await?;

    // 4: build upstream request.
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", oauthcfg::upstream_base(), path_and_query);

    let mut fwd = HeaderMap::new();
    for (name, value) in in_headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        fwd.insert(name.clone(), value.clone());
    }
    fwd.remove(axum::http::header::HOST);
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

    let refreshed = oauth::refresh(&current).await?;
    let token = refreshed.access_token.clone();
    let profile_owned = profile.to_string();
    Store::update(move |s| {
        s.profiles.insert(profile_owned, refreshed);
        Ok(())
    })?;
    Ok(token)
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
