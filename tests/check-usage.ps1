$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $projectRoot
foreach ($script in @('ui/app.js', 'ui/usage-dashboard.js', 'ui/chatgpt.js', 'ui/antigravity.js', 'ui/zcode.js')) {
    & node --check $script
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
& node --test tests/usage-dashboard.test.cjs tests/usage-cards.test.cjs tests/app-polish.test.cjs tests/chatgpt-ui.test.cjs tests/zcode-ui.test.cjs tests/proxy-guide.test.cjs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& python tests/usage-local.test.py
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Push-Location -LiteralPath (Join-Path $projectRoot 'src-tauri')
try {
    $env:CARGO_NET_OFFLINE = 'true'
    & cargo test
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally { Pop-Location }
& python tests/usage-live-probe.py
exit $LASTEXITCODE
