$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'environment.ps1')
$binary=Join-Path $WhiteboardRoot 'target/debug/orbisync-server.exe'
if (!(Test-Path -LiteralPath $binary)) { throw 'Run cargo build -p orbisync-server first.' }
docker start orbisync-whiteboard-demo-db | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Could not start the whiteboard database.' }
function Start-WhiteboardProcess($name,$port,$exe,$arguments,$cwd) {
  if (Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction SilentlyContinue) { throw "Port $port is already in use; no process was stopped." }
  $process=Start-Process -FilePath $exe -ArgumentList $arguments -WorkingDirectory $cwd -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $WhiteboardRuntime "$name.out.log") -RedirectStandardError (Join-Path $WhiteboardRuntime "$name.err.log")
  @{id=$process.Id;started=$process.StartTime.ToUniversalTime().ToString('o');name=$name} | ConvertTo-Json | Set-Content -Encoding ascii (Join-Path $WhiteboardRuntime "$name.process.json")
}
$config=Join-Path $WhiteboardRuntime 'runtime-core.toml'
Start-WhiteboardProcess 'core' 18081 $binary @('--config',('"'+$config+'"'),'serve') $WhiteboardRoot
Start-WhiteboardProcess 'lock' 8843 (Get-Command node.exe).Source @(('"'+(Join-Path $WhiteboardRoot 'apps/whiteboard/rules/whiteboard-lock-hook.mjs')+'"')) $WhiteboardRoot
Start-WhiteboardProcess 'ui' 5175 (Get-Command node.exe).Source @(('"'+(Join-Path $WhiteboardRoot 'apps/whiteboard/node_modules/vite/bin/vite.js')+'"'),'--host','127.0.0.1','--port','5175','--strictPort') (Join-Path $WhiteboardRoot 'apps/whiteboard')
Write-Output 'Whiteboard: http://127.0.0.1:5175/'
