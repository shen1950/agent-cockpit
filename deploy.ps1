$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
if (-not $root) { $root = (Get-Location).Path }
$bin = Join-Path $root 'bin'
$src = Join-Path $root 'src-tauri\target\release\agent-cockpit.exe'
if (-not (Test-Path $src)) { Write-Error "build output missing: $src"; exit 1 }

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$target = Join-Path $bin 'AgentCockpit.exe'
if (Test-Path $target) {
    $bak = Join-Path $bin ("AgentCockpit.exe.bak-{0}-pre-astudio" -f $stamp)
    Move-Item -Path $target -Destination $bak -Force
}
Copy-Item -Path $src -Destination $target -Force

Get-Process -Name AgentCockpit -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400
Start-Process -FilePath $target
Start-Sleep -Seconds 2
$p = Get-Process -Name AgentCockpit -ErrorAction SilentlyContinue
if (-not $p) { Write-Error 'new instance did not start'; exit 1 }
Write-Host ("deployed: {0} (pid {1})" -f $target, ($p.Id -join ','))
