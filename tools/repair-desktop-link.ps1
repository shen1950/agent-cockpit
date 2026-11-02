$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe = (Resolve-Path -LiteralPath (Join-Path $root 'bin/AgentCockpit.exe')).Path
$desktop = [Environment]::GetFolderPath('DesktopDirectory')
if (-not [IO.Path]::IsPathRooted($desktop)) { throw 'Desktop path could not be resolved' }
$link = Join-Path $desktop 'Agent 驾驶舱.lnk'
$shell = New-Object -ComObject WScript.Shell
if (Test-Path -LiteralPath $link) {
    $existing = $shell.CreateShortcut($link)
    if ($existing.TargetPath -ine $exe) { throw 'Existing shortcut belongs to another target; left unchanged' }
}
# The application uses the same redirected Desktop resolver and AppUserModelID as its settings UI.
$process = Start-Process -FilePath $exe -ArgumentList '--create-desktop-shortcut' -WindowStyle Hidden -Wait -PassThru
if ($process.ExitCode -ne 0) { throw 'Application could not create the desktop shortcut' }
if (-not (Test-Path -LiteralPath $link)) { throw 'Shortcut missing from the actual Desktop directory' }
$verified = $shell.CreateShortcut($link)
if ($verified.TargetPath -ine $exe) { throw 'Shortcut target verification failed' }
$iconPath = $verified.IconLocation -replace ',\s*-?\d+$',''
if (-not (Test-Path -LiteralPath $iconPath)) { throw 'Shortcut icon resource missing' }
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CockpitIconRefresh {
  [DllImport("shell32.dll", CharSet=CharSet.Unicode)]
  public static extern void SHChangeNotify(uint e, uint flags, string first, IntPtr second);
}
'@
# Notify only these changed items. No global icon-cache deletion or Explorer restart.
[CockpitIconRefresh]::SHChangeNotify(0x2000, 5, $exe, [IntPtr]::Zero)
[CockpitIconRefresh]::SHChangeNotify(0x2000, 5, $link, [IntPtr]::Zero)
[pscustomobject]@{ Shortcut=$link; TargetVerified=$true; IconResourceExists=$true }
