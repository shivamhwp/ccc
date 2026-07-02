# End-to-end smoke test for ccc on Windows (PowerShell 7).
#
# Proves, without touching any real Claude login:
#   1. per-request PID attribution works (netstat path) — the proxy log line
#      carries a real pid for a request made through it,
#   2. `ccc daemon start` registers the HKCU Run key + hidden launcher and
#      spawns the daemon, `ccc daemon stop` cleans both up.
#
# Uses a throwaway store with a fake token and a local dummy upstream, so no
# request ever reaches api.anthropic.com with real credentials.
# Requires: a debug/release ccc.exe build (set CCC_BIN to override) + python.
$ErrorActionPreference = "Stop"

$ccc = if ($env:CCC_BIN) { $env:CCC_BIN } else { Join-Path $PSScriptRoot "..\target\debug\ccc.exe" }
if (-not (Test-Path $ccc)) { Write-Error "ccc binary not found at $ccc (build it, or set CCC_BIN)" }

# Resolve home the same way ccc does (HOME, then USERPROFILE).
$homeDir = if ($env:HOME) { $env:HOME } else { $env:USERPROFILE }
$cccDir = Join-Path $homeDir ".ccc"

function Say($msg) { Write-Host "`n== $msg" -ForegroundColor Cyan }
function Fail($msg) { Write-Host "FAIL: $msg" -ForegroundColor Red; exit 1 }

function Wait-Health($port) {
    foreach ($i in 1..50) {
        try {
            $r = Invoke-WebRequest "http://127.0.0.1:$port/_ccc/health" -UseBasicParsing -TimeoutSec 2
            if ($r.StatusCode -eq 200) { return }
        } catch { Start-Sleep -Milliseconds 200 }
    }
    Fail "daemon on :$port never became healthy"
}

Say "seeding throwaway store (fake token, far-future expiry)"
New-Item -ItemType Directory -Force -Path $cccDir | Out-Null
@'
{"version":1,"default_profile":"smoke","profiles":{"smoke":{"access_token":"sk-ant-oat01-SMOKE-FAKE","refresh_token":"","expires_at":4102444800000,"scopes":[],"subscription_type":"max"}}}
'@ | Set-Content -Path (Join-Path $cccDir "store.json") -Encoding UTF8

# --- 1. routing: PID attribution through the proxy -------------------------
Say "starting dummy upstream on :9999 and logging daemon on :8788"
$upstream = Start-Process python -ArgumentList "-m", "http.server", "9999" -PassThru -WindowStyle Hidden
$logOut = Join-Path $cccDir "smoke-daemon.out.log"
$logErr = Join-Path $cccDir "smoke-daemon.err.log"
$env:CCC_LOG = "1"
$env:CCC_UPSTREAM_BASE = "http://127.0.0.1:9999"
$daemon = Start-Process $ccc -ArgumentList "daemon", "run", "--port", "8788" `
    -PassThru -WindowStyle Hidden -RedirectStandardOutput $logOut -RedirectStandardError $logErr
Wait-Health 8788

Say "sending a request through the proxy"
& curl.exe -s -o NUL "http://127.0.0.1:8788/v1/models"
Start-Sleep -Milliseconds 500

$log = Get-Content $logErr -Raw -ErrorAction SilentlyContinue
if ($log -notmatch '\[ccc\] .* pid=(\d+) profile=smoke') {
    Write-Host $log
    Fail "proxy log has no per-request pid attribution (netstat lookup failed?)"
}
Say "pid attribution ok: request attributed to pid $($Matches[1])"

Stop-Process -Id $daemon.Id -Force
Stop-Process -Id $upstream.Id -Force
Remove-Item Env:CCC_LOG, Env:CCC_UPSTREAM_BASE

# --- 2. autostart: Run key + hidden spawn + stop ----------------------------
Say "ccc daemon start (Run key + detached hidden daemon)"
$out = & $ccc daemon start 2>&1 | Out-String
Write-Host $out
if ($LASTEXITCODE -ne 0) { Fail "ccc daemon start exited $LASTEXITCODE" }
if ($out -notmatch "Run key") { Fail "start output does not mention the Run key" }

& reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v ccc | Out-Null
if ($LASTEXITCODE -ne 0) { Fail "Run key was not registered" }
if (-not (Test-Path (Join-Path $cccDir "ccc-daemon.vbs"))) { Fail "hidden launcher vbs missing" }
Wait-Health 8787

$status = & $ccc daemon status | Out-String
if ($status -notmatch "running") { Fail "daemon status says: $status" }
$daemonPid = (Get-Content (Join-Path $cccDir "daemon.json") | ConvertFrom-Json).pid
Say "daemon running (pid $daemonPid), Run key + launcher in place"

Say "ccc doctor"
& $ccc doctor | Out-String | Write-Host

Say "ccc daemon stop (must remove Run key, launcher, and process)"
& $ccc daemon stop
if ($LASTEXITCODE -ne 0) { Fail "ccc daemon stop exited $LASTEXITCODE" }
Start-Sleep -Milliseconds 500

& reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v ccc 2>$null | Out-Null
if ($LASTEXITCODE -eq 0) { Fail "Run key still present after stop" }
if (Test-Path (Join-Path $cccDir "ccc-daemon.vbs")) { Fail "launcher vbs still present after stop" }
if (Get-Process -Id $daemonPid -ErrorAction SilentlyContinue) { Fail "daemon pid $daemonPid still alive after stop" }

$statusAfter = & $ccc daemon status | Out-String
if ($statusAfter -notmatch "not running") { Fail "daemon status after stop says: $statusAfter" }

Write-Host "`nPASS: routing attribution + autostart lifecycle verified" -ForegroundColor Green
