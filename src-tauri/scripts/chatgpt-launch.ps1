# Embedded in the desktop binary. The opt-in smoke check calls this same function.
function Start-AgentCockpitChatgptPackage {
  param([string]$Executable,[string]$Profile,[string]$AuthHome)
  $ErrorActionPreference='Stop'
  foreach($path in @($Executable,$Profile,$AuthHome)) {
    if(-not [IO.Path]::IsPathRooted($path) -or ($path -split '[\\/]') -contains '..'){throw 'An absolute ordinary path is required'}
    $part=Get-Item -LiteralPath $path
    while($part){if($part.Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'A profile or parent is a directory link'};$part=$part.Parent}
  }
  $packages=@(Get-AppxPackage -Name OpenAI.Codex | Where-Object {
    [IO.Path]::GetFullPath((Join-Path $_.InstallLocation 'app\ChatGPT.exe')) -ieq [IO.Path]::GetFullPath($Executable)
  })
  if($packages.Count -ne 1){throw 'The selected Codex Store executable is not registered'}
  $package=$packages[0]
  [xml]$manifest=[IO.File]::ReadAllText((Join-Path $package.InstallLocation 'AppxManifest.xml'))
  $entries=@($manifest.Package.Applications.Application | Where-Object {$_.Executable.Replace('/','\') -ieq 'app\ChatGPT.exe'})
  if($entries.Count -ne 1){throw 'The Codex Store application entry could not be confirmed'}
  $receipt=Join-Path ([IO.Path]::GetTempPath()) ('agentcockpit-launch-'+[guid]::NewGuid().ToString('N')+'.json')
  function Literal([string]$value){return "'"+$value.Replace("'","''")+"'"}
  $inner=@'
$ErrorActionPreference='Stop'
try {
  $info=[Diagnostics.ProcessStartInfo]::new($exe)
  $info.UseShellExecute=$false
  $info.WorkingDirectory=[IO.Path]::GetDirectoryName($exe)
  $info.Arguments='--user-data-dir="'+$profile+'"'
  foreach($key in @($info.EnvironmentVariables.Keys)) {
    if($key -match '^(CODEX|QODER|CLAUDE|TRAE|GEMINI|ANTIGRAVITY)' -or $key -in @('OPENAI_API_KEY','OPENAI_BASE_URL','OPENAI_ACCESS_TOKEN','OPENAI_IDENTITY_TOKEN_FILE')){$info.EnvironmentVariables.Remove($key)}
  }
  $info.EnvironmentVariables['CODEX_HOME']=$auth
  $info.EnvironmentVariables['CODEX_ELECTRON_USER_DATA_PATH']=$profile
  $child=[Diagnostics.Process]::Start($info)
  $result=@{ok=$true;pid=$child.Id}
} catch {$result=@{ok=$false;message=$_.Exception.Message}}
[IO.File]::WriteAllText($receipt,($result | ConvertTo-Json -Compress),[Text.UTF8Encoding]::new($false))
'@
  $inner='$exe='+(Literal $Executable)+';$profile='+(Literal $Profile)+';$auth='+(Literal $AuthHome)+';$receipt='+(Literal $receipt)+"`n"+$inner
  $encoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($inner))
  $helper=Join-Path $env:WINDIR 'System32\WindowsPowerShell\v1.0\powershell.exe'
  try {
    Invoke-CommandInDesktopPackage -PackageFamilyName $package.PackageFamilyName -AppId $entries[0].Id -PreventBreakaway -Command $helper -Args ('-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand '+$encoded) | Out-Null
    $deadline=[DateTime]::UtcNow.AddSeconds(15)
    while(-not [IO.File]::Exists($receipt) -and [DateTime]::UtcNow -lt $deadline){Start-Sleep -Milliseconds 100}
    if(-not [IO.File]::Exists($receipt)){throw 'The package helper did not confirm startup; no fallback or second launch was attempted'}
    $result=[IO.File]::ReadAllText($receipt) | ConvertFrom-Json
    if(-not $result.ok){throw $result.message}
    return $result.pid
  } finally {if([IO.File]::Exists($receipt)){[IO.File]::Delete($receipt)}}
}
