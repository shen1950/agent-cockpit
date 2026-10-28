$ErrorActionPreference = 'Stop'
$tokens = $null
$parseErrors = $null
$deployment = Join-Path (Split-Path -Parent $PSScriptRoot) 'deploy.ps1'
[void][System.Management.Automation.Language.Parser]::ParseFile($deployment, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ($parseErrors.Message -join "`n") }
$shell = New-Object -ComObject Shell.Application
$desktopHwnd = 0
$desktop = $shell.Windows().FindWindowSW(0, 0, 8, [ref]$desktopHwnd, 1)
if (-not $desktop -or -not $desktopHwnd -or [IO.Path]::GetFileName($desktop.FullName) -ine 'explorer.exe') {
    throw 'The existing Explorer desktop broker could not be confirmed'
}
if (-not $desktop.Document.Application) { throw 'The desktop automation proxy is unavailable' }
Write-Output 'deployment_syntax_valid=true existing_explorer_desktop_broker=true no_application_launched=true'
