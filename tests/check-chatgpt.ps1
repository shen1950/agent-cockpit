$ErrorActionPreference='Stop'
$root=Split-Path -Parent $PSScriptRoot
$env:CARGO_NET_OFFLINE='true'
$env:CARGO_BUILD_JOBS='1'
Push-Location (Join-Path $root 'src-tauri')
try { & cargo test -- --test-threads=1; if($LASTEXITCODE -ne 0){exit $LASTEXITCODE} } finally { Pop-Location }
foreach($file in @('ui\app.js','ui\chatgpt.js','ui\zcode.js')) {
 & node --check (Join-Path $root $file)
 if($LASTEXITCODE -ne 0){exit $LASTEXITCODE}
}
& node --test (Join-Path $PSScriptRoot 'chatgpt-ui.test.cjs') (Join-Path $PSScriptRoot 'zcode-ui.test.cjs') (Join-Path $PSScriptRoot 'app-polish.test.cjs')
if($LASTEXITCODE -ne 0){exit $LASTEXITCODE}
& python (Join-Path $PSScriptRoot 'chatgpt-history.test.py')
if($LASTEXITCODE -ne 0){exit $LASTEXITCODE}
if($env:AGENTCOCKPIT_CARD_MAINTENANCE -eq '1'){
 Push-Location (Join-Path $root 'src-tauri')
 try { & cargo test maintain_registered_chatgpt_cards -- --ignored --nocapture --test-threads=1; if($LASTEXITCODE -ne 0){exit $LASTEXITCODE} } finally { Pop-Location }
}
if($env:AGENTCOCKPIT_QUOTA_SMOKE -eq '1'){
 Push-Location (Join-Path $root 'src-tauri')
 try { & cargo test live_registered_chatgpt_quotas -- --ignored --nocapture --test-threads=1; if($LASTEXITCODE -ne 0){exit $LASTEXITCODE} } finally { Pop-Location }
}
if($env:AGENTCOCKPIT_ZCODE_SMOKE -eq '1'){
 Push-Location (Join-Path $root 'src-tauri')
 try { & cargo test registered_zcode_account_migration_smoke -- --ignored --nocapture --test-threads=1; if($LASTEXITCODE -ne 0){exit $LASTEXITCODE} } finally { Pop-Location }
}
if($env:AGENTCOCKPIT_WINDOW_SMOKE -eq '1'){
 Push-Location (Join-Path $root 'src-tauri')
 try { & cargo test registered_api_window_restore_smoke -- --ignored --nocapture --test-threads=1; if($LASTEXITCODE -ne 0){exit $LASTEXITCODE} } finally { Pop-Location }
}
if($env:AGENTCOCKPIT_OFFICIAL_SMOKE -eq '1'){
 Push-Location (Join-Path $root 'src-tauri')
 try { & cargo test official_client_cancel_smoke -- --ignored --test-threads=1; if($LASTEXITCODE -ne 0){exit $LASTEXITCODE} } finally { Pop-Location }
}
if($env:AGENTCOCKPIT_CHATGPT_SMOKE -eq '1'){
 & (Join-Path $PSScriptRoot 'chatgpt-desktop-smoke.ps1')
}
if($env:AGENTCOCKPIT_LAYOUT_SMOKE -eq '1'){
 & node (Join-Path $PSScriptRoot 'chatgpt-layout.test.cjs')
 if($LASTEXITCODE -ne 0){exit $LASTEXITCODE}
 & node (Join-Path $PSScriptRoot 'typography-layout.test.cjs')
 if($LASTEXITCODE -ne 0){exit $LASTEXITCODE}
}
if($env:AGENTCOCKPIT_CLAUDE_SMOKE -eq '1'){
 Push-Location (Join-Path $root 'src-tauri')
 try { & cargo test real_claude_desktop_registration_is_visible_without_creating_an_account -- --ignored --nocapture --test-threads=1; if($LASTEXITCODE -ne 0){exit $LASTEXITCODE} } finally { Pop-Location }
}
exit $LASTEXITCODE
