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

```sh
cargo build --release
# put target/release/ccc on your PATH
```

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
