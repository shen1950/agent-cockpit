param([Parameter(Mandatory=$true)][string]$RuntimeRoot)
$ErrorActionPreference = 'Stop'
$revision = '37155a5f75f3f2d7bde1b83054c8d6eb2e23a935'
$expectedHash = '3B2954265E56D22668FE88F19F872467E45522788889570BDA1020DE73A1AA4B'
$nodeVersion = (& node --version).TrimStart('v').Split('.')
if ($LASTEXITCODE -ne 0 -or [int]$nodeVersion[0] -lt 24) { throw 'ZCode component requires Node.js 24 or newer' }
$taskRoot = [IO.Path]::GetFullPath($RuntimeRoot)
New-Item -ItemType Directory -Path $taskRoot -Force | Out-Null
$source = Join-Path $taskRoot "zcode-api-$revision"
$marker = Join-Path $source '.agentcockpit-installed'
if ((Test-Path -LiteralPath $marker) -and (Get-Content -LiteralPath $marker -Raw).Trim() -eq $revision) {
    Write-Output 'ZCode component already installed'; exit 0
}
$archive = Join-Path $taskRoot "$revision.zip"
Invoke-WebRequest -Uri "https://codeload.github.com/TriDefender/zcode-api/zip/$revision" -OutFile $archive
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expectedHash) { throw 'ZCode source checksum mismatch' }
Expand-Archive -LiteralPath $archive -DestinationPath $taskRoot -Force
if (-not (Test-Path -LiteralPath (Join-Path $source 'LICENSE'))) { throw 'ZCode dependency license missing' }
Push-Location -LiteralPath $source
try {
    # No upstream package lifecycle scripts run. Keep its source and license intact.
    & npm.cmd install --ignore-scripts --omit=dev --no-audit --no-fund --save-exact happy-dom@20.11.6 undici@8.10.0 yaml@2.5.0 *> (Join-Path $taskRoot 'install.log')
    if ($LASTEXITCODE -ne 0) { throw 'ZCode dependency install failed; see install.log' }
} finally { Pop-Location }
Set-Content -LiteralPath $marker -Value $revision -Encoding ascii
Write-Output 'ZCode component installed'
