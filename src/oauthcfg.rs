//! OAuth constants for the Claude Code public client.
//!
//! Values reverse-engineered from the Claude Code binary (v2.1.197) and made
//! overridable via env vars so a Claude Code update that moves an endpoint can
//! be worked around without recompiling.

pub fn client_id() -> String {
    env_or("CCC_OAUTH_CLIENT_ID", "9d1c250a-e61b-44d9-88ed-5944d1962f5e")
}

/// claude.ai subscription authorize endpoint.
pub fn authorize_url() -> String {
    env_or("CCC_OAUTH_AUTHORIZE_URL", "https://claude.ai/oauth/authorize")
}

/// Token grant endpoint (authorization_code + refresh_token).
pub fn token_url() -> String {
    env_or("CCC_OAUTH_TOKEN_URL", "https://platform.claude.com/v1/oauth/token")
}

/// Redirect used by the manual copy/paste flow.
pub fn redirect_uri() -> String {
    env_or(
        "CCC_OAUTH_REDIRECT_URI",
        "https://platform.claude.com/oauth/code/callback",
    )
}

/// Upstream API base the proxy forwards to.
pub fn upstream_base() -> String {
    env_or("CCC_UPSTREAM_BASE", "https://api.anthropic.com")
}

/// Scope set requested for subscription (claude.ai) login.
pub fn scopes() -> String {
    env_or(
        "CCC_OAUTH_SCOPES",
        "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
    )
}

/// Beta header value that must accompany OAuth bearer tokens.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
