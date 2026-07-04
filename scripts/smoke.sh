#!/usr/bin/env bash
# End-to-end smoke test for ccc.
#
# Proves the two things that matter, against your real store, without touching
# your real ~/.claude:
#   1. the proxy authenticates upstream with a saved subscription token (200),
#   2. per-thread routing selects a *different* account for a routed PID.
#
# It creates a throwaway profile with a deliberately broken token, routes a
# shell to it (expect failure), then reverts to default (expect success), and
# cleans up. Requires: a release/debug `ccc` build and at least one real
# account already saved (`ccc import` or `ccc login`).
set -euo pipefail

PORT="${CCC_SMOKE_PORT:-8799}"
BAD="ccc-smoke-bad"
CCC="${CCC_BIN:-$(dirname "$0")/../target/debug/ccc}"
REQ='{"model":"claude-haiku-4-5-20251001","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
fail() { printf '\033[31mFAIL: %s\033[0m\n' "$1"; exit 1; }

command -v "$CCC" >/dev/null 2>&1 || CCC="$(command -v ccc || true)"
[ -x "$CCC" ] || fail "ccc binary not found (build it, or set CCC_BIN)"

say "preflight"
LIST_OUT="$("$CCC" list)"
printf '%s\n' "$LIST_OUT" | grep -q 'PROFILE' || fail "no accounts saved; run 'ccc import' first"
printf '%s\n' "$LIST_OUT" | grep -q '(default)' || fail "no default account set"

# --- craft a throwaway broken profile (copy default, corrupt the token) ------
# The store is encrypted at rest, so edits go through `ccc store export/import`.
say "adding throwaway broken profile '$BAD'"
TMP_STORE="$(mktemp)"
"$CCC" store export "$TMP_STORE" 2>/dev/null
python3 - "$TMP_STORE" "$BAD" <<'PY'
import json, sys
store_path, bad = sys.argv[1], sys.argv[2]
d = json.load(open(store_path))
default = d.get("default_profile") or next(iter(d["profiles"]))
p = dict(d["profiles"][default])
p["access_token"] = "sk-ant-oat01-INVALID-SMOKE-TEST"
p["refresh_token"] = ""            # cannot refresh -> bad token is used as-is
p["expires_at"] = 99999999999999   # far future -> no refresh attempt
p["email"] = "smoke@invalid"
d["profiles"][bad] = p
json.dump(d, open(store_path, "w"), indent=2)
PY
"$CCC" store import "$TMP_STORE" >/dev/null
rm -f "$TMP_STORE"

cleanup() {
  "$CCC" use --default --pid "$$" >/dev/null 2>&1 || true
  "$CCC" remove "$BAD" >/dev/null 2>&1 || true
  [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# --- start daemon -------------------------------------------------------------
say "starting daemon on :$PORT"
CCC_LOG=1 "$CCC" daemon run --port "$PORT" >/tmp/ccc-smoke.log 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$PORT/_ccc/health" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -sf "http://127.0.0.1:$PORT/_ccc/health" >/dev/null || fail "daemon did not come up"

post() {
  curl -s -o /dev/null -w "%{http_code}" -X POST "http://127.0.0.1:$PORT/v1/messages" \
    -H "content-type: application/json" -H "anthropic-version: 2023-06-01" -d "$REQ"
}

# --- route this shell to the broken profile: expect non-200 ------------------
say "route this shell -> $BAD (broken token)"
"$CCC" use "$BAD" --pid "$$" >/dev/null
CODE_BAD=$(post)
echo "  HTTP $CODE_BAD"
[ "$CODE_BAD" != "200" ] || fail "routed request to broken profile unexpectedly succeeded"

# --- revert to default: expect 200 -------------------------------------------
say "revert this shell -> default (real token)"
"$CCC" use --default --pid "$$" >/dev/null
CODE_OK=$(post)
echo "  HTTP $CODE_OK"
[ "$CODE_OK" = "200" ] || fail "default-account request did not succeed (got $CODE_OK)"

say "routing log"
grep '\[ccc\]' /tmp/ccc-smoke.log | tail -4 || true

printf '\n\033[32mSMOKE PASS: subscription auth works and per-thread routing selects distinct accounts.\033[0m\n'
