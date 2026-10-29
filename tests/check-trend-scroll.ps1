$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath (Split-Path -Parent $PSScriptRoot)
& node --check ui/app.js
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& node --test tests/usage-cards.test.cjs tests/chatgpt-ui.test.cjs tests/zcode-ui.test.cjs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& node tests/trend-scroll.test.cjs
exit $LASTEXITCODE
