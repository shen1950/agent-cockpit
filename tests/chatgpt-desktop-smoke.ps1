$ErrorActionPreference='Stop'
$nativeProfile=Join-Path $env:APPDATA 'Codex\web\Codex'
$nativeAuth=Join-Path $env:USERPROFILE '.codex\auth.json'
function CredentialHash {
  if(-not (Test-Path -LiteralPath $nativeAuth)){return ''}
  $sha=[Security.Cryptography.SHA256]::Create()
  try {return [BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($nativeAuth)))} finally {$sha.Dispose()}
}
$hashBefore=CredentialHash
if((Get-Item -LiteralPath $nativeProfile).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Native profile must be an ordinary directory'}
$exe=(Get-CimInstance Win32_Process -Filter "Name='ChatGPT.exe'" | Where-Object {$_.ExecutablePath} | Select-Object -First 1).ExecutablePath
if(-not $exe){throw 'Open the installed ChatGPT/Codex desktop client before this opt-in smoke check'}
$testRoot=Join-Path ([IO.Path]::GetTempPath()) ('agentcockpit-chatgpt-smoke-'+[guid]::NewGuid().ToString('N'))
$profiles=@();$starts=@();$proof=@()
. (Join-Path $PSScriptRoot '..\src-tauri\scripts\chatgpt-launch.ps1')
New-Item -ItemType Directory -Path $testRoot | Out-Null
try {
  foreach($n in 1..2){
    $profile=Join-Path $testRoot "browser-$n";$auth=Join-Path $testRoot "auth-$n"
    New-Item -ItemType Directory -Path $profile,$auth | Out-Null
    [IO.File]::WriteAllText((Join-Path $auth 'config.toml'),"cli_auth_credentials_store = 'file'`n",[Text.UTF8Encoding]::new($false))
    $profiles+=$profile
    $starts+=Start-AgentCockpitChatgptPackage -Executable $exe -Profile $profile -AuthHome $auth
  }
  $deadline=[DateTime]::UtcNow.AddSeconds(25)
  do {
    Start-Sleep -Seconds 2
    $running=@(Get-CimInstance Win32_Process -Filter "Name='ChatGPT.exe'" | Where-Object {$_.CommandLine -notmatch '--type='})
    $found=@($profiles | Where-Object {$p=$_;@($running | Where-Object {$_.CommandLine.Contains($p)}).Count -gt 0})
  } while($found.Count -lt 2 -and [DateTime]::UtcNow -lt $deadline)
  if($found.Count -ne 2){throw 'Two independent main processes were not confirmed'}
  Start-Sleep -Seconds 5
  foreach($n in 1..2){
    $profile=Join-Path $testRoot "browser-$n";$auth=Join-Path $testRoot "auth-$n"
    $homeEntries=@(Get-ChildItem -LiteralPath $auth -Force | Where-Object {$_.Name -ne 'config.toml'})
    $browserEntries=@(Get-ChildItem -LiteralPath $profile -Force)
    if($browserEntries.Count -eq 0 -or $homeEntries.Count -eq 0){throw "Instance $n did not write to both isolated directories"}
    $proof+= [pscustomobject]@{instance=$n;browserEntries=$browserEntries.Count;authEntries=$homeEntries.Count;authFiles=($homeEntries.Name -join ',')}
  }
  $hashAfter=CredentialHash
  if($hashAfter -ne $hashBefore){throw 'Default credentials changed during smoke check'}
  if((Get-Item -LiteralPath $nativeProfile).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Native profile became a link'}
  $proof | Format-Table -AutoSize
  Write-Host 'PASS: two independent desktop processes, separate browser/runtime writes, default auth hash unchanged, default profile ordinary.'
} finally {
  $targets=@(Get-CimInstance Win32_Process -Filter "Name='ChatGPT.exe'" | Where-Object {$cmd=$_.CommandLine; $cmd -and @($profiles | Where-Object {$cmd.Contains($_)}).Count -gt 0})
  foreach($target in $targets){
    $stopInfo=[Diagnostics.ProcessStartInfo]::new('taskkill.exe',('/PID '+$target.ProcessId+' /T /F'))
    $stopInfo.UseShellExecute=$false;$stopInfo.CreateNoWindow=$true;$stopInfo.RedirectStandardOutput=$true;$stopInfo.RedirectStandardError=$true
    $stop=[Diagnostics.Process]::Start($stopInfo);$stop.WaitForExit(5000) | Out-Null
  }
  # Preserve only this synthetic run's files for review. No user profile is removed.
  Write-Host ('Synthetic profile evidence retained at '+$testRoot)
}
