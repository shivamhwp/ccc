# ccc

> **Use two or more Claude accounts on your device.**

`ccc` lets you keep several Claude (subscription) accounts on one machine and switch which one a Claude Code thread uses — per thread, live, with no restarts. Two threads can run two different accounts at the same time, and switching takes effect within a few seconds. Subscription auth only: no API keys are ever stored or used.

## Install

macOS / Linux one-liner:

```sh
curl -fsSL https://raw.githubusercontent.com/shivamhwp/ccc/main/scripts/install.sh | bash
```

Or build from source:

```sh
cargo build --release   # binary at target/release/ccc
```

On Windows, download the `.zip` from the [Releases](https://github.com/shivamhwp/ccc/releases) page and put `ccc.exe` on your PATH.

## Quickstart

```sh
# 1. One-time setup: imports your current Claude Code login as `default`,
#    patches settings.json, and installs the agent skill + daemon.
ccc setup

# 2. Add a second account (opens your browser, then paste the code shown).
ccc login work

# 3. Confirm both are saved.
ccc list
```

Now, inside any Claude Code thread, just tell the agent which account to use:

> using ccc, use the work account and open a PR for this branch

The installed agent skill (`~/.claude/skills/ccc/`) teaches agents to run `ccc use work` for you, routing that thread to the `work` account within a few seconds. Other threads are unaffected, and it works in any shell (fish/zsh/bash) because routing is process-based, not shell-based. To do it yourself:

```sh
ccc use work         # route THIS thread to `work`
ccc whoami           # which account is this thread using?
ccc use --default    # revert this thread to the default account
```

## Commands

| Command | What it does |
|---------|--------------|
| `ccc setup` | Import current login, patch Claude Code `settings.json`, install the agent skill + daemon |
| `ccc login <name>` | Log in to a Claude account via browser OAuth (paste the code) and save it under a profile name |
| `ccc import [name]` | Seed a profile from the account currently logged into Claude Code (default name: `default`) |
| `ccc list` | List saved accounts |
| `ccc whoami` | Show which account THIS thread is using |
| `ccc use <name>` | Route the current thread to an account |
| `ccc use --default` | Revert the current thread to the default account |
| `ccc use <name> --pid <pid>` | Route a specific `claude` PID instead of auto-detecting |
| `ccc default <name>` | Set the fallback account for threads with no explicit route |
| `ccc remove <name>` | Remove a saved account |
| `ccc doctor` | Diagnostics: verify the daemon, settings, and auth path |
| `ccc daemon run\|start\|stop\|status` | Control the localhost proxy daemon |

## How it works

`ccc` runs a small reverse proxy on `127.0.0.1` (default port `8787`). `ccc setup` points Claude Code at it by writing `env.ANTHROPIC_BASE_URL` into `<claude_dir>/settings.json` (honoring `CLAUDE_CONFIG_DIR`, otherwise `~/.claude`). Every thread's API traffic then flows through the daemon, which decides — per request — which saved subscription account to authenticate as.

```
  claude thread A ─┐                              ┌─────────────────────┐
  claude thread B ─┼──▶ ccc proxy (127.0.0.1) ──▶ │  api.anthropic.com  │
  claude thread C ─┘        │                     └─────────────────────┘
                            │  per request:
                            │   1. source port ──▶ claude PID   (lsof)
                            │   2. PID + ancestors ──▶ profile  (routes.json,
                            │                          else default)
                            │   3. refresh token if near expiry (per-profile lock)
                            │   4. Authorization: Bearer <token>
                            ▼      + anthropic-beta: oauth-2025-04-20 (x-api-key stripped)
```

For each inbound request the proxy:

1. **Attributes the connection to a PID.** It maps the connection's source port back to the owning `claude` process using `lsof`.
2. **Resolves that PID to a profile.** The PID and its ancestors are looked up in `~/.ccc/routes.json`; if nothing matches, it falls back to the default profile.
3. **Refreshes the token if needed.** If the profile's subscription token is near expiry it is refreshed under a per-profile lock, so concurrent requests refresh at most once.
4. **Forwards upstream.** It sets `Authorization: Bearer <subscription token>`, adds the `anthropic-beta: oauth-2025-04-20` header, strips any `x-api-key`, and streams the response back.

Because account selection happens per request, `ccc use <name>` changes the account for the current thread live — no restart, and it works regardless of shell (routing is PID-based, not env-based).

> **Note:** Claude Code will not send any request unless it believes it is logged in. So `ccc setup` also writes a placeholder `ANTHROPIC_AUTH_TOKEN` into `settings.json` purely to satisfy that local auth gate. The proxy always overwrites the `Authorization` header with the routed account's real token, so the placeholder value is never actually used.

## Platform support

| Platform | Binary | Per-thread routing | Autostart |
| --- | --- | --- | --- |
| macOS arm64 / x64 | ✅ | ✅ | ✅ (launchd) |
| Linux x64 | ✅ | ✅ (needs `lsof` / `ps`) | ⏳ planned (systemd); run `ccc daemon run` manually for now |
| Windows x64 | ✅ (builds, CLI/proxy run) | 🧪 experimental — needs a native `iphlpapi`/toolhelp backend | 🧪 experimental — needs native backend |

macOS is the fully-supported target today.

## Files

Everything ccc owns lives under `~/.ccc`:

- **`~/.ccc/store.json`** — saved accounts and their subscription OAuth tokens (mode `0600`).
- **`~/.ccc/routes.json`** — the live PID → account map; entries for dead PIDs are garbage-collected.
- **`~/.ccc/daemon.json`** — the running daemon's PID and port.

The current Claude login is read from the OS credential store, not written by ccc: on macOS from the Keychain (`Claude Code-credentials`), on Linux from `~/.claude/.credentials.json`.

## Testing

- **`cargo test`** — unit tests covering `lsof` output parsing, PKCE S256 generation, store load/refresh, and route resolution.
- **`./scripts/smoke.sh`** — an end-to-end smoke test. It starts the daemon on a scratch port, creates a throwaway account with a deliberately broken token, routes a shell to it (expecting the request to fail), reverts to the default account (expecting success), then cleans up. The success path uses your real account but never touches `~/.claude`.

## Releases

Pushing a tag `vX.Y.Z` triggers GitHub Actions to:

- build binaries for macOS (arm64 + x64), Linux (x64 gnu + static musl), and Windows (x64),
- package them as `tar.gz` / `zip` with SHA-256 checksums, and
- create the GitHub Release with those assets.

CI runs `cargo fmt`, `clippy` (with `-D warnings`), and the test suite on every push and pull request.

## Configuration

These environment variables override defaults — handy if a Claude Code update moves an endpoint:

| Variable | Purpose |
| --- | --- |
| `CCC_OAUTH_CLIENT_ID` | OAuth client ID |
| `CCC_OAUTH_AUTHORIZE_URL` | Authorization endpoint |
| `CCC_OAUTH_TOKEN_URL` | Token endpoint |
| `CCC_OAUTH_REDIRECT_URI` | OAuth redirect URI |
| `CCC_OAUTH_SCOPES` | Requested OAuth scopes |
| `CCC_UPSTREAM_BASE` | Upstream API base URL |
| `CCC_LOG=1` | On the daemon, emit a per-request routing log line to stderr |

## Security

- Subscription tokens are stored in `~/.ccc/store.json` with mode `0600` (Keychain-backed storage is planned).
- No API keys are ever used or stored — only subscription OAuth tokens.
- The proxy binds `127.0.0.1` only; it is never exposed off the loopback interface.

## Status

The proxy, per-thread routing, token refresh, Keychain credential import, and the CLI all work and are tested end-to-end on macOS. Next up: a Linux systemd unit for autostart, a native Windows backend for routing and autostart, Keychain-backed token storage, and a real `ccc login` shakeout to complement the current import path.

## License

MIT — see [LICENSE](LICENSE).
