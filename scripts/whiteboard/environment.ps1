$ErrorActionPreference = 'Stop'
$WhiteboardRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$WhiteboardRuntime = Join-Path $env:LOCALAPPDATA 'OrbiSync/whiteboard-demo'
$keys = @{
  ORBISYNC_TOKEN_SIGNING_KEY='token-signing.pem'
  ORBISYNC_PAGINATION_HMAC_KEY='pagination-hmac.txt'
  ORBISYNC_REFRESH_TOKEN_HMAC_KEY='refresh-hmac.txt'
  ORBISYNC_REALTIME_TICKET_HMAC_KEY='realtime-hmac.txt'
  ORBISYNC_IDEMPOTENCY_HMAC_KEY='idempotency-hmac.txt'
  ORBI_EXTENSION_SECRET_WHITEBOARD_LOCK='lock-hmac.txt'
}
foreach ($item in $keys.GetEnumerator()) {
  [Environment]::SetEnvironmentVariable($item.Key,[IO.File]::ReadAllText((Join-Path $WhiteboardRuntime $item.Value)).Trim(),'Process')
}
$dbPassword=[Uri]::EscapeDataString([IO.File]::ReadAllText((Join-Path $WhiteboardRuntime 'db-password.txt')).Trim())
$env:DATABASE_URL="postgres://whiteboard:${dbPassword}@127.0.0.1:55445/whiteboard"
$env:ORBISYNC_PASSWORD_DENYLIST_FILE=Join-Path $WhiteboardRuntime 'denylist.txt'
$env:ORBISYNC_CORE_URL='http://127.0.0.1:18081'
# The pre-commit hook is the only rule process now. It keeps no state, so the
# lock API port, its CORS origin and the legacy JSON store are no longer set.
# The old whiteboard-locks.json file is left in the runtime folder untouched.
$env:WHITEBOARD_LOCK_HOOK_PORT='8843'
$env:WHITEBOARD_LOCK_CERT=Join-Path $WhiteboardRuntime 'lock-cert.pem'
$env:WHITEBOARD_LOCK_KEY=Join-Path $WhiteboardRuntime 'lock-key.pem'
$ca=(Join-Path $WhiteboardRuntime 'ca.pem').Replace('\','/')
@"
[server]
bind = "127.0.0.1:18081"
[cors]
allowed_origins = ["http://127.0.0.1:5175"]
allow_credentials = true
[extensions]
pre_commit_additional_ca_path = "$ca"
allow_loopback_endpoints = true
"@ | Set-Content -Encoding ascii (Join-Path $WhiteboardRuntime 'runtime-core.toml')
