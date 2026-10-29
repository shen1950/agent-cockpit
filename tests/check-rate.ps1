$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath (Split-Path -Parent $PSScriptRoot)
& node --check ui/usage-dashboard.js
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& node --test tests/usage-dashboard.test.cjs tests/usage-cards.test.cjs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& node tests/usage-rate-browser.test.cjs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& python tests/usage-local.test.py
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Push-Location -LiteralPath 'src-tauri'
try {
    $env:CARGO_NET_OFFLINE = 'true'
    & cargo test
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally { Pop-Location }
& python tests/usage-rate-live.py
exit $LASTEXITCODE
