# ccc

Use multiple Claude Code accounts on one machine, switchable **per thread**, with no restarts.

`ccc` runs a small localhost proxy that every Claude Code thread points at
(`ANTHROPIC_BASE_URL`). For each request it decides which saved account the
request should authenticate as — based on the calling thread's process — and
rewrites the request with that account's **subscription** OAuth token. Two
threads can run two different accounts at the same time; switching an account in
a thread takes effect within seconds and needs no restart.

Subscription auth only. No API keys are stored or used.

## How it works

```
        ~/.ccc/
          store.json     accounts + subscription tokens (mode 0600)
          routes.json    live map: claude PID -> account

  thread A (fish) ─┐   thread B (zsh) ─┐   t3code thread ─┐
   claude pid 111  │    claude pid 222 │    claude pid 333│
        └──────────┴─────────┬─────────┴─────────┴────────┘
                             ▼   (ANTHROPIC_BASE_URL)
                     ccc daemon (localhost proxy)
                       1. attribute connection -> claude PID (via source port)
                       2. PID + ancestors -> account (routes.json), else default
                       3. refresh that account's token if near expiry
                       4. forward with Authorization: Bearer <subscription token>
                          + anthropic-beta: oauth-2025-04-20
```

Because selection happens per request, `ccc use <name>` changes the account for
the current thread live. The shell is not involved, so fish/zsh/bash all behave
identically, and t3code's spawned `claude` processes route the same way.

> Note: Claude Code won't send any request unless it believes it's logged in, so
> `ccc setup` also sets a placeholder `ANTHROPIC_AUTH_TOKEN` in `settings.json`.
> The proxy always overwrites the auth header, so the placeholder is never used.

## Install

Prebuilt binaries are published on every tagged release (see below).

```sh
# macOS / Linux one-liner (downloads the right binary from the latest release)
curl -fsSL https://raw.githubusercontent.com/shivamhwp/ccc/main/scripts/install.sh | bash

# or build from source
cargo build --release   # binary at target/release/ccc
```

On Windows, download the `.zip` from the Releases page and put `ccc.exe` on your PATH.

## Platform support

| Platform | Binary | Per-thread routing | Daemon autostart |
|----------|:------:|:------------------:|:----------------:|
| macOS (arm64/x64) | ✅ | ✅ | ✅ launchd |
| Linux (x64)       | ✅ | ✅ (`lsof`/`ps`)   | ⏳ systemd unit (planned) |
| Windows (x64)     | ✅ builds | ⏳ needs native port | ⏳ planned |

macOS is the fully-supported target today. Linux routing works where `lsof`
and `ps` are present; autostart is manual (`ccc daemon run`) until a systemd
unit lands. Windows binaries build and the CLI/proxy run, but per-thread routing
and autostart need a native (iphlpapi/toolhelp) backend — treat Windows as
experimental.

## Testing

```sh
cargo test            # unit tests (parsing, PKCE, store, routing resolution)
./scripts/smoke.sh    # end-to-end: proves auth + per-thread routing live
```

`smoke.sh` starts the daemon on a scratch port, creates a throwaway account with
a broken token, routes a shell to it (expects failure), reverts to the default
account (expects success), and cleans up. It uses your real saved account for
the success path but never touches `~/.claude`.

## Releases

Push a tag and GitHub Actions builds and publishes binaries for all targets:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The `release` workflow cross-builds macOS (arm64 + x64), Linux (x64 gnu + musl),
and Windows (x64), packages tarballs/zips with SHA-256 checksums, and creates
the GitHub Release. `ci` runs fmt + clippy + tests on every push/PR.

## Usage

```sh
ccc setup            # import current login, patch settings.json, install skill + daemon
ccc login work       # add another account (browser OAuth, paste the code)
ccc list             # show saved accounts
ccc use work         # route THIS thread to `work` (what agents run)
ccc use --default    # revert this thread to the default account
ccc whoami           # which account is this thread using?
ccc default personal # set the fallback account
ccc doctor           # verify daemon, settings, tokens
ccc daemon status    # daemon control: run | start | stop | status
```

Inside any Claude Code thread you can just say:

> using ccc, use the work account and …

An installed agent skill (`~/.claude/skills/ccc/`) teaches agents to run
`ccc use <name>` when you name an account.

## Status

Early. The proxy, per-thread routing, token refresh, keychain import, and the
CLI are working and tested end-to-end against a live subscription. Fresh
`ccc login` (vs. `ccc import`) and Keychain-backed token storage are the next
items.

## Configuration

All OAuth endpoints and the upstream base are overridable via env vars
(`CCC_OAUTH_*`, `CCC_UPSTREAM_BASE`) in case a Claude Code update moves them.
Set `CCC_LOG=1` on the daemon for a per-request routing log.
