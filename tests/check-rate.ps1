$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath (Split-Path -Parent $PSScriptRoot)
& node --check ui/usage-dashboard.js
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& node --test tests/usage-dashboard.test.cjs tests/usage-cards.test.cjs tests/app-polish.test.cjs tests/chatgpt-ui.test.cjs tests/zcode-ui.test.cjs tests/proxy-guide.test.cjs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& node tests/usage-rate-browser.test.cjs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& node tests/trend-scroll.test.cjs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& python tests/usage-local.test.py
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Push-Location -LiteralPath 'src-tauri'
try {
    & cargo test
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally { Pop-Location }
& python tests/usage-rate-live.py
exit $LASTEXITCODE
