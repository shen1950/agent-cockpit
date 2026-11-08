# Invoke through agsync build --test-cmd, which owns the shared build lock.
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $projectRoot
$env:CARGO_NET_OFFLINE = 'true'
& cargo test --manifest-path src-tauri/Cargo.toml
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& node --test tests/proxy-fold-and-stats.test.cjs tests/proxy-guide.test.cjs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
foreach ($testFile in @('tests/gateway-capabilities.test.cjs', 'tests/gateway-routes.test.cjs', 'tests/px-fold-ui.test.cjs')) {
    & node $testFile
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
& python tests/license-report.test.py
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
