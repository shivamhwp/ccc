//! OAuth PKCE login and token refresh against the Claude Code public client.
//! Subscription (claude.ai) tokens only.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::oauthcfg;
use crate::store::{now_ms, Profile};

/// PKCE material for one login attempt.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

pub fn new_pkce() -> Pkce {
    let verifier = rand_b64url(32);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = b64url(&hasher.finalize());
    let state = rand_b64url(24);
    Pkce {
        verifier,
        challenge,
        state,
    }
}

/// Build the browser authorize URL for a login attempt.
pub fn authorize_url(pkce: &Pkce) -> String {
    let mut url = url_with_query(
        &oauthcfg::authorize_url(),
        &[
            ("code", "true"),
            ("client_id", &oauthcfg::client_id()),
            ("response_type", "code"),
            ("redirect_uri", &oauthcfg::redirect_uri()),
            ("scope", &oauthcfg::scopes()),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("state", &pkce.state),
        ],
    );
    // Some deployments expect no trailing '&'.
    if url.ends_with('&') {
        url.pop();
    }
    url
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    account: Option<AccountInfo>,
    #[serde(default)]
    organization: Option<OrgInfo>,
}

#[derive(Deserialize)]
struct AccountInfo {
    #[serde(default)]
    email_address: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
}

#[derive(Deserialize)]
struct OrgInfo {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Exchange an authorization code (possibly of the form `CODE#STATE`) for tokens.
pub async fn exchange_code(pkce: &Pkce, pasted: &str) -> Result<Profile> {
    let (code, state) = match pasted.split_once('#') {
        Some((c, s)) => (c.trim().to_string(), s.trim().to_string()),
        None => (pasted.trim().to_string(), pkce.state.clone()),
    };

    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "state": state,
        "client_id": oauthcfg::client_id(),
        "redirect_uri": oauthcfg::redirect_uri(),
        "code_verifier": pkce.verifier,
    });

    let client = http_client()?;
    let resp = client
        .post(oauthcfg::token_url())
        .json(&body)
        .send()
        .await
        .context("posting authorization_code grant")?;
    let profile = token_response_to_profile(resp).await?;
    Ok(profile)
}

/// Refresh a profile's tokens in place. Returns the updated profile.
pub async fn refresh(profile: &Profile) -> Result<Profile> {
    if profile.refresh_token.is_empty() {
        return Err(anyhow!("profile has no refresh token; run `ccc login` again"));
    }
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": profile.refresh_token,
        "client_id": oauthcfg::client_id(),
    });
    let client = http_client()?;
    let resp = client
        .post(oauthcfg::token_url())
        .json(&body)
        .send()
        .await
        .context("posting refresh_token grant")?;
    let mut refreshed = token_response_to_profile(resp).await?;
    // Preserve identity fields we already knew; carry forward refresh token if
    // the server didn't rotate it.
    if refreshed.refresh_token.is_empty() {
        refreshed.refresh_token = profile.refresh_token.clone();
    }
    refreshed.email = refreshed.email.or_else(|| profile.email.clone());
    refreshed.account_uuid = refreshed
        .account_uuid
        .or_else(|| profile.account_uuid.clone());
    refreshed.organization_uuid = refreshed
        .organization_uuid
        .or_else(|| profile.organization_uuid.clone());
    refreshed.organization_name = refreshed
        .organization_name
        .or_else(|| profile.organization_name.clone());
    if refreshed.subscription_type.is_none() {
        refreshed.subscription_type = profile.subscription_type.clone();
    }
    Ok(refreshed)
}

async fn token_response_to_profile(resp: reqwest::Response) -> Result<Profile> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("token endpoint returned {status}: {text}"));
    }
    let tr: TokenResponse =
        serde_json::from_str(&text).with_context(|| format!("parsing token response: {text}"))?;
    let expires_at = now_ms() + tr.expires_in.unwrap_or(3600) * 1000;
    let scopes = tr
        .scope
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    Ok(Profile {
        access_token: tr.access_token,
        refresh_token: tr.refresh_token,
        expires_at,
        scopes,
        subscription_type: None,
        email: tr.account.as_ref().and_then(|a| a.email_address.clone()),
        account_uuid: tr.account.as_ref().and_then(|a| a.uuid.clone()),
        organization_uuid: tr.organization.as_ref().and_then(|o| o.uuid.clone()),
        organization_name: tr.organization.as_ref().and_then(|o| o.name.clone()),
    })
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("ccc/0.1")
        .build()
        .context("building HTTP client")
}

fn rand_b64url(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    b64url(&buf)
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn url_with_query(base: &str, params: &[(&str, &str)]) -> String {
    let mut s = String::from(base);
    s.push('?');
    for (k, v) in params {
        s.push_str(&urlencode(k));
        s.push('=');
        s.push_str(&urlencode(v));
        s.push('&');
    }
    s
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
