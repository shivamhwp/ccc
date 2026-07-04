#!/usr/bin/env bash
# Hermetic end-to-end test for ccc — no real account, no real ~/.ccc or
# ~/.claude. Everything runs against a throwaway HOME with a mock upstream
# (echoes the Authorization header, so each response reveals which account a
# request was billed as) and a mock OAuth token endpoint (rotates refresh
# tokens like the real one, and rejects a reused token).
#
# Proves, on macOS and Linux:
#   - store encryption: plaintext store.json migrates to store.enc + backups
#   - routing: default account, live per-thread switching, /a/<name> pins,
#     unknown-pin rejection, recycled-pid fallback
#   - refresh: refresh-on-expiry, single-flight under concurrency, rotated
#     refresh tokens persisted, invalid_grant backoff with actionable errors
#   - store export/import round-trip
#   - (Linux) the seed watcher re-owning a live login that matches a saved
#     profile, and leaving unknown accounts alone
#
# Requires: bash, python3, curl, a debug/release ccc build (or CCC_BIN).
set -uo pipefail

# In a container this script can be pid 1 (bash -c execs its last command).
# Routes on pid 1 never resolve (ancestors stop before init), so re-run as a
# child shell — as any real Claude Code process would be.
if [ $$ -eq 1 ]; then bash "$0" "$@"; exit $?; fi

PASS=0; FAIL=0
check() { # check <name> <expected> <actual>
  if [ "$2" = "$3" ]; then PASS=$((PASS+1)); echo "ok   - $1"
  else FAIL=$((FAIL+1)); echo "FAIL - $1: expected [$2] got [$3]"; fi
}
contains() { # contains <name> <needle> <haystack>
  case "$3" in
    *"$2"*) PASS=$((PASS+1)); echo "ok   - $1" ;;
    *) FAIL=$((FAIL+1)); echo "FAIL - $1: [$3] does not contain [$2]" ;;
  esac
}

CCC="${CCC_BIN:-$(dirname "$0")/../target/debug/ccc}"
[ -x "$CCC" ] || CCC="$(command -v ccc || true)"
[ -x "$CCC" ] || { echo "ccc binary not found (build it, or set CCC_BIN)"; exit 1; }
CCC="$(cd "$(dirname "$CCC")" && pwd)/$(basename "$CCC")"

PORT="${CCC_E2E_PORT:-8797}"
MOCK_PORT="${CCC_E2E_MOCK_PORT:-9917}"
FAR_FUTURE=4102444800000

# --- hermetic environment -----------------------------------------------------
HOME="$(mktemp -d)" || { echo "mktemp failed"; exit 1; }
export HOME
export CLAUDE_CONFIG_DIR="$HOME/.claude"
export CCC_KEY_FILE="$HOME/.ccc/e2e.key"
export CCC_UPSTREAM_BASE="http://127.0.0.1:$MOCK_PORT"
export CCC_OAUTH_TOKEN_URL="http://127.0.0.1:$MOCK_PORT/token"
export CCC_SEED_CHECK_SECS=1
mkdir -p "$HOME/.ccc" "$CLAUDE_CONFIG_DIR"

# Legacy plaintext store — the daemon must migrate it to store.enc on start.
#   alpha: default, fresh token           beta: fresh token
#   hot:   expires imminently (forces refresh; mock rotates its tokens)
#   dead:  expired with a revoked refresh token (mock answers invalid_grant)
python3 - "$HOME/.ccc/store.json" <<'PY'
import json, sys, time
now = int(time.time() * 1000)
far = 4102444800000
mk = lambda tok, rt, exp, **id: dict(
    access_token=tok, refresh_token=rt, expires_at=exp, scopes=[], **id)
json.dump({
    "version": 1,
    "default_profile": "alpha",
    "profiles": {
        "alpha": mk("tok-alpha", "rt-alpha", far,
                    account_uuid="uuid-alpha", email="alpha@e2e.test"),
        "beta":  mk("tok-beta", "rt-beta", far),
        "hot":   mk("tok-hot-0", "rt-hot-1", now + 1000),
        "dead":  mk("tok-dead", "rt-dead", now - 1000),
    },
}, open(sys.argv[1], "w"))
PY

# --- mock upstream + token endpoint -------------------------------------------
python3 - "$MOCK_PORT" <<'PY' &
import json, sys, threading
from http.server import BaseHTTPRequestHandler, HTTPServer

state = {"hot_next": 1, "hot_calls": 0, "dead_calls": 0}
lock = threading.Lock()

class H(BaseHTTPRequestHandler):
    def _send(self, code, body):
        data = json.dumps(body).encode() if isinstance(body, dict) else body
        self.send_response(code)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/counts":
            with lock:
                self._send(200, {"hot": state["hot_calls"], "dead": state["dead_calls"]})
        else:
            self._send(200, b"ok")

    def do_POST(self):
        length = int(self.headers.get("content-length", 0) or 0)
        body = self.rfile.read(length)
        if self.path == "/token":
            rt = json.loads(body or b"{}").get("refresh_token", "")
            with lock:
                if rt == f"rt-hot-{state['hot_next']}":
                    n = state["hot_next"]
                    state["hot_next"] += 1
                    state["hot_calls"] += 1
                    self._send(200, {"access_token": f"tok-hot-{n}",
                                     "refresh_token": f"rt-hot-{n+1}",
                                     "expires_in": 300})
                elif rt.startswith("rt-hot-"):
                    # A reused (already-rotated) token: the real endpoint
                    # revokes these. Catches rotation-not-persisted bugs.
                    self._send(400, {"error": "invalid_grant",
                                     "error_description": "refresh token reused"})
                elif rt == "rt-dead":
                    state["dead_calls"] += 1
                    self._send(400, {"error": "invalid_grant"})
                else:
                    self._send(400, {"error": "invalid_grant",
                                     "error_description": f"unknown token {rt}"})
        else:
            # Upstream echo: the Authorization header is the response body.
            self._send(200, self.headers.get("Authorization", "none").encode())

    def log_message(self, *a):
        pass

HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
MOCK_PID=$!
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$MOCK_PORT/counts" >/dev/null 2>&1 && break
  sleep 0.2
done

# --- daemon --------------------------------------------------------------------
CCC_LOG=1 "$CCC" daemon run --port "$PORT" >"$HOME/daemon.log" 2>&1 &
DAEMON_PID=$!
cleanup() { kill "$DAEMON_PID" "$MOCK_PID" 2>/dev/null; }
trap cleanup EXIT
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$PORT/_ccc/health" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -sf "http://127.0.0.1:$PORT/_ccc/health" >/dev/null \
  || { echo "FAIL - daemon did not start"; cat "$HOME/daemon.log"; exit 1; }

post() { curl -s -X POST "http://127.0.0.1:$PORT$1" -H 'content-type: application/json' -d '{}'; }
counts() { curl -s "http://127.0.0.1:$MOCK_PORT/counts"; }
count_of() { counts | python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"; }

# --- store encryption & migration ----------------------------------------------
[ -f "$HOME/.ccc/store.enc" ] \
  && check "store migrated to store.enc" "ok" "ok" \
  || check "store migrated to store.enc" "store.enc exists" "missing"
[ ! -f "$HOME/.ccc/store.json" ] \
  && check "plaintext store.json removed" "ok" "ok" \
  || check "plaintext store.json removed" "gone" "still present"

# --- routing -------------------------------------------------------------------
check "default account used when unrouted" "Bearer tok-alpha" "$(post /v1/messages)"
"$CCC" use beta --pid $$ >/dev/null 2>&1
check "routed thread uses beta" "Bearer tok-beta" "$(post /v1/messages)"
"$CCC" use --default --pid $$ >/dev/null 2>&1
check "reverted thread uses default" "Bearer tok-alpha" "$(post /v1/messages)"
check "path pin selects account" "Bearer tok-beta" "$(post /a/beta/v1/messages)"

ghost_body="$(post /a/ghost/v1/messages)"
contains "unknown pin is rejected" "not saved" "$ghost_body"
case "$ghost_body" in
  *tok-*) check "unknown pin leaks no token" "no token" "leaked: $ghost_body" ;;
  *) check "unknown pin leaks no token" "ok" "ok" ;;
esac

"$CCC" use beta --pid $$ >/dev/null 2>&1
python3 - "$$" "$HOME/.ccc/routes.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[2]))
d["routes"][sys.argv[1]]["started"] = "424242"   # simulate a recycled pid
json.dump(d, open(sys.argv[2], "w"))
PY
check "recycled pid falls back to default" "Bearer tok-alpha" "$(post /v1/messages)"
"$CCC" use --default --pid $$ >/dev/null 2>&1

# --- refresh: single-flight under concurrency -----------------------------------
outs="$HOME/concurrent"
mkdir -p "$outs"
pids=()
for i in 1 2 3 4 5; do post /a/hot/v1/messages >"$outs/$i" & pids+=($!); done
wait "${pids[@]}"
check "concurrent requests share one refresh" "1" "$(count_of hot)"
for i in 1 2 3 4 5; do
  check "concurrent request $i got the refreshed token" "Bearer tok-hot-1" "$(cat "$outs/$i")"
done

# --- refresh: rotation persisted (via store export/import round-trip) ----------
exp="$HOME/exported.json"
"$CCC" store export "$exp" 2>/dev/null
python3 - "$exp" <<'PY'
import json, sys, time
d = json.load(open(sys.argv[1]))
assert d["profiles"]["hot"]["refresh_token"] == "rt-hot-2", \
    f"rotated refresh token not persisted: {d['profiles']['hot']['refresh_token']}"
d["profiles"]["hot"]["expires_at"] = int(time.time() * 1000)  # force re-refresh
json.dump(d, open(sys.argv[1], "w"))
PY
check "export shows rotated refresh token" "0" "$?"
"$CCC" store import "$exp" >/dev/null
check "second refresh uses the rotated token" "Bearer tok-hot-2" "$(post /a/hot/v1/messages)"
check "token endpoint saw exactly two refreshes" "2" "$(count_of hot)"

# --- refresh: invalid_grant backoff + actionable error --------------------------
dead_body="$(post /a/dead/v1/messages)"
contains "invalid_grant error names the profile" "ccc login dead" "$dead_body"
contains "invalid_grant error hints at /login takeover" "ccc import" "$dead_body"
contains "second request is backed off" "backing off" "$(post /a/dead/v1/messages)"
check "backoff stopped the token endpoint hammering" "1" "$(count_of dead)"

# --- backups -------------------------------------------------------------------
[ -f "$HOME/.ccc/store.enc.bak.1" ] \
  && check "store backup rotation exists" "ok" "ok" \
  || check "store backup rotation exists" "store.enc.bak.1" "missing"

# --- seed watcher (Linux/Windows daemon only; file-based credentials) -----------
if [ "$(uname -s)" = "Linux" ]; then
  # A live login for a KNOWN account (matches alpha by uuid): the daemon must
  # adopt its tokens and re-seed within a couple of watcher ticks.
  python3 - "$HOME" "$CLAUDE_CONFIG_DIR" "$FAR_FUTURE" <<'PY'
import json, sys
home, cdir, far = sys.argv[1], sys.argv[2], int(sys.argv[3])
json.dump({"oauthAccount": {"accountUuid": "uuid-alpha",
                            "emailAddress": "alpha@e2e.test"}},
          open(f"{home}/.claude.json", "w"))
json.dump({"claudeAiOauth": {"accessToken": "tok-alpha-live",
                             "refreshToken": "rt-alpha-live",
                             "expiresAt": far - 1,   # live: not the seed marker
                             "scopes": [], "subscriptionType": "max"}},
          open(f"{cdir}/.credentials.json", "w"))
PY
  healed=""
  for _ in $(seq 1 20); do
    sleep 0.5
    exp_at="$(python3 -c "import json;print(json.load(open('$CLAUDE_CONFIG_DIR/.credentials.json'))['claudeAiOauth']['expiresAt'])")"
    [ "$exp_at" = "$FAR_FUTURE" ] && healed=yes && break
  done
  check "watcher re-seeded a known live login" "yes" "${healed:-no}"
  check "watcher adopted the live tokens" "Bearer tok-alpha-live" "$(post /v1/messages)"

  # A live login for an UNKNOWN account: must be left untouched.
  python3 - "$HOME" "$CLAUDE_CONFIG_DIR" <<'PY'
import json, sys
home, cdir = sys.argv[1], sys.argv[2]
json.dump({"oauthAccount": {"accountUuid": "uuid-stranger"}},
          open(f"{home}/.claude.json", "w"))
json.dump({"claudeAiOauth": {"accessToken": "tok-stranger",
                             "refreshToken": "rt-stranger",
                             "expiresAt": 9999999999999, "scopes": []}},
          open(f"{cdir}/.credentials.json", "w"))
PY
  sleep 3
  stranger_tok="$(python3 -c "import json;print(json.load(open('$CLAUDE_CONFIG_DIR/.credentials.json'))['claudeAiOauth']['accessToken'])")"
  check "watcher leaves unknown accounts alone" "tok-stranger" "$stranger_tok"
  check "store untouched by unknown login" "Bearer tok-alpha-live" "$(post /v1/messages)"
else
  echo "skip - seed watcher (daemon watcher runs on Linux/Windows; macOS uses doctor/setup)"
fi

# --- report --------------------------------------------------------------------
echo
echo "passed=$PASS failed=$FAIL"
if [ "$FAIL" != 0 ]; then
  echo "--- daemon log ---"
  tail -30 "$HOME/daemon.log"
  exit 1
fi
rm -rf "$HOME"
