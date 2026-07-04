<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.png">
    <img alt="ccc" src="assets/logo-light.png" width="190">
  </picture>
</p>

<p align="center">use two or more claude accounts on your device.</p>

<p align="center">
  <a href="#install">Install</a> &nbsp;·&nbsp;
  <a href="#quickstart">Quickstart</a> &nbsp;·&nbsp;
  <a href="#how-it-works">How it works</a> &nbsp;·&nbsp;
  <a href="#commands">Commands</a> &nbsp;·&nbsp;
  <a href="#for-your-agents">For your agents</a>
</p>

<br/>

`ccc` keeps several Claude subscription accounts on one machine and switches which one a Claude Code thread uses — **per thread, live, no restarts**. Two threads can run two different accounts at the same time. Subscription auth only; no API keys are ever stored or used.

> [!NOTE]
> `ccc` is early software. macOS, Linux, and Windows are all supported. See [Platform support](#platform-support).

## Install

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/shivamhwp/ccc/main/scripts/install.sh | bash

# or from source
cargo build --release        # → target/release/ccc
```

```powershell
# Windows
irm https://raw.githubusercontent.com/shivamhwp/ccc/main/scripts/install.ps1 | iex
```

## Quickstart

```sh
ccc setup            # import current login, wire up Claude Code, start the daemon
ccc login work       # add a second account (browser opens; paste the code)
ccc list             # see saved accounts
```

Then, in any Claude Code thread:

> using ccc, use the work account and open a PR for this branch

That's it — the account is live for that thread within a couple of seconds, and no other thread is affected.

## Commands

| Command | What it does |
|---|---|
| `ccc setup` | Import current login, patch Claude Code `settings.json`, install the agent skill + daemon |
| `ccc login <name>` | Browser OAuth login, saved under a profile name |
| `ccc import [name]` | Save the current Claude Code login as a profile (ccc takes over its token refresh) |
| `ccc use <name>` | Route **this thread** to an account |
| `ccc use --default` | Revert this thread to the default account |
| `ccc whoami` | Which account is this thread using? |
| `ccc list` | List saved accounts |
| `ccc default <name>` | Set the fallback account |
| `ccc remove <name>` | Remove a saved account |
| `ccc doctor` | Verify daemon, settings, and auth path |
| `ccc store export \| import <path>` | Back up / restore the decrypted account store (`-` for stdio) |
| `ccc daemon run \| start \| stop \| status` | Control the local proxy daemon |
| `ccc t3 sync \| unsync` | Add/remove one [t3code](https://github.com/pingdotgg/t3code) provider instance per account |
| `ccc teardown` | Undo `ccc setup` (revert settings, hand the login back to Claude Code, remove skill, stop daemon) |

## How it works

`ccc` runs a tiny reverse proxy on `127.0.0.1`. `ccc setup` points Claude Code at it via `ANTHROPIC_BASE_URL` in `settings.json`, so every thread's traffic flows through the daemon — which decides, **per request**, which account to authenticate as.

```
  thread A ─┐                            ┌─────────────────────┐
  thread B ─┼──▶  ccc proxy  ──────────▶ │  api.anthropic.com  │
  thread C ─┘        │                   └─────────────────────┘
                     │  1. source port ──▶ claude PID
                     │  2. PID + ancestors ──▶ account (else default)
                     │  3. refresh token if near expiry
                     ▼  4. Authorization: Bearer <subscription token>
```

Because selection happens per request, `ccc use <name>` changes the account for the current thread instantly — no restart, and independent of your shell (routing is process-based).

**Credential ownership.** Anthropic rotates refresh tokens on use, so a token shared by two refreshers breaks whichever refreshes second. ccc therefore owns every saved account's tokens outright: `ccc setup` (or `ccc import`) copies the live login into ccc's encrypted store once, then overwrites Claude Code's own credentials with a far-future-expiry copy. Claude Code sees a login that never expires — it stays "logged in" (correct account in `/status`), never refreshes, and never touches the Keychain — while the proxy injects the real, ccc-refreshed token on every request. `ccc teardown` writes the live tokens back, returning ownership to Claude Code.

If a `/login` in Claude Code replaces the seed with a live grant, the daemon detects it on Linux/Windows and automatically re-imports + re-seeds (matching the account by uuid/email; unknown accounts are left alone). On macOS the daemon never touches the Keychain, so `ccc doctor` / `ccc setup` flag and fix it instead.

## For your agents

`ccc setup` installs a skill at `~/.claude/skills/ccc/` that teaches Claude Code agents what "using ccc, use the &lt;name&gt; account" means — they run `ccc use <name>` for you, then carry on with the task. Detection covers the native binary as well as `node`/`bun` (npm) installs of Claude Code.

## t3code

`ccc t3 sync` adds one [t3code](https://github.com/pingdotgg/t3code) provider instance per account (`claude · <name>`), each pinned to its account. In t3code you then just pick the provider for a thread — no per-thread routing needed there. `ccc t3 unsync` removes them. Your existing t3code instances are left untouched (and the file is backed up first).

## Platform support

| Platform | Binary | Per-thread routing | Autostart |
|---|:---:|:---:|:---:|
| macOS (arm64 / x64) | ✅ | ✅ | ✅ launchd |
| Linux (x64) | ✅ | ✅ via `/proc` (no lsof needed) | ✅ systemd user unit* |
| Windows (x64) | ✅ | ✅ via `netstat` | ✅ Run key (login, hidden) |

\* On Linux hosts without a user systemd session (some WSL/container setups), `ccc daemon start` falls back to a detached background process — it runs immediately but won't restart after reboot.

## Testing

```sh
cargo test                    # unit: lsof + /proc/net/tcp + netstat parsing, PKCE, store/refresh, vault, routing
./scripts/e2e.sh              # hermetic e2e (macOS/Linux, also in CI): routing, refresh/rotation, store encryption, seed watcher — mock endpoints, no real account
./scripts/smoke.sh            # end-to-end (macOS/Linux): proves real auth + per-thread routing against your account, then cleans up
./scripts/smoke-windows.ps1   # end-to-end (Windows): autostart lifecycle + routing attribution (also runs in CI)
```

## Files & configuration

State lives in `~/.ccc/`: `store.enc` (accounts + subscription tokens, encrypted — see below), `store.enc.bak.1..3` (rotated pre-write backups), `routes.json` (live PID → account), `daemon.json` (pid + port). A pre-encryption `store.json` is migrated to `store.enc` (and shredded) on the next daemon start or setup. The autostart agent lives at `~/Library/LaunchAgents/ing.shivam.ccc.plist` (macOS), `~/.config/systemd/user/ccc.service` (Linux), or the `ccc` value under `HKCU\...\CurrentVersion\Run` plus `~/.ccc/ccc-daemon.vbs` (Windows).

OAuth endpoints and the upstream base are overridable via `CCC_OAUTH_*` and `CCC_UPSTREAM_BASE` env vars (useful if a Claude Code update moves an endpoint). Set `CCC_LOG=1` on the daemon for a per-request routing log. `CCC_SEED_CHECK_SECS` tunes the seed watcher cadence, `CCC_KEY_FILE` overrides where the store key lives.

## Releases

Push a tag `vX.Y.Z` → GitHub Actions builds macOS (arm64 + x64), Linux (x64 gnu + musl), and Windows (x64), packages tarballs/zips with SHA-256 checksums, and publishes the release. CI runs fmt + clippy + tests on every push.

## Security

- Subscription tokens only — no API keys, ever.
- Tokens are encrypted at rest (`~/.ccc/store.enc`, XChaCha20-Poly1305). The key lives in the macOS Keychain, or in `~/.ccc/key` (`0600`) on Linux/Windows — matching those platforms' Claude Code credential storage model. `CCC_KEY_FILE` overrides the location.
- Every store write rotates three backups, and `ccc store export` makes off-machine backup/migration possible — losing one file no longer means re-authing every account.
- The proxy binds `127.0.0.1` only.

## License

[MIT](LICENSE)
