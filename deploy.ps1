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
# The deploy shell can belong to Codex's Store package. Ask the existing desktop
# shell to launch our portable app so it does not inherit that package identity.
$desktopShell = New-Object -ComObject Shell.Application
$desktopHwnd = 0
$desktop = $desktopShell.Windows().FindWindowSW(0, 0, 8, [ref]$desktopHwnd, 1)
if (-not $desktop -or -not $desktopHwnd -or [IO.Path]::GetFileName($desktop.FullName) -ine 'explorer.exe') {
    throw 'The existing Explorer desktop broker could not be confirmed'
}
# A newly created Shell.Application object can retain the caller's Store token.
# The desktop view's Application is the automation proxy of the existing shell.
$desktop.Document.Application.ShellExecute($target, '', $bin, 'open', 0)
$deadline = [DateTime]::UtcNow.AddSeconds(12)
do {
    Start-Sleep -Milliseconds 300
    $p = Get-Process -Name AgentCockpit -ErrorAction SilentlyContinue
} while (-not $p -and [DateTime]::UtcNow -lt $deadline)
if (-not $p) { Write-Error 'new instance did not start'; exit 1 }
Write-Host ("deployed: {0} (pid {1})" -f $target, ($p.Id -join ','))
