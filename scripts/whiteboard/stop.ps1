$ErrorActionPreference='Stop'
$runtime=Join-Path $env:LOCALAPPDATA 'OrbiSync/whiteboard-demo'
foreach($name in @('ui','lock','core')) {
  $file=Join-Path $runtime "$name.process.json"
  if (!(Test-Path -LiteralPath $file)) { continue }
  $record=Get-Content -Raw -LiteralPath $file | ConvertFrom-Json
  $process=Get-Process -Id $record.id -ErrorAction SilentlyContinue
  if($process -and $process.StartTime.ToUniversalTime().ToString('o') -eq $record.started) { Stop-Process -Id $process.Id }
}
docker stop orbisync-whiteboard-demo-db
