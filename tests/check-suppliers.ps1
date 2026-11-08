# Run as agsync build --test-cmd; all checks share its build lock.
# Scope: supplier (provider) CRUD, its UI and the proxy panels it touches.
# tests/gateway-routes.test.cjs is NOT run here: it belongs to the zcode-miaoshou-fix
# task (task gateway-routes 测试跟上接入设置新 DOM). Running another task's file
# inside this batch would make this task fail on somebody else's in-flight edit.
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $projectRoot
$env:CARGO_NET_OFFLINE = 'true'
$env:NO_PROXY = '127.0.0.1,localhost'
foreach ($file in @('ui/gateway-suppliers.js', 'ui/mock.js', 'ui/app.js', 'tests/gateway-suppliers-integration.test.cjs', 'tests/gateway-capabilities.test.cjs')) {
    & node --check $file
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
& git diff --check -- ui/gateway-suppliers.js ui/gateway-suppliers.css ui/mock.js ui/app.js src-tauri/src/gateway.rs src-tauri/src/commands.rs src-tauri/src/proxy.rs tests/gateway-suppliers-integration.test.cjs tests/gateway-capabilities.test.cjs tests/check-suppliers.ps1 CHANGELOG.md
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& cargo test --manifest-path src-tauri/Cargo.toml
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& node --test tests/proxy-fold-and-stats.test.cjs tests/proxy-guide.test.cjs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
foreach ($testFile in @('tests/px-fold-ui.test.cjs', 'tests/gateway-suppliers-integration.test.cjs', 'tests/gateway-capabilities.test.cjs')) {
    & node $testFile
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
