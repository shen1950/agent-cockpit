param([int]$Iterations = 2)
$ErrorActionPreference = 'Stop'
# Invoke the actual native tray menu. Do not simulate restart by starting an exe.
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public class RestartTrayNative {
  public delegate bool EnumProc(IntPtr hwnd, IntPtr value);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc callback, IntPtr value);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hwnd, StringBuilder value, int count);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint msg, UIntPtr wp, IntPtr lp);
  [DllImport("user32.dll")] public static extern IntPtr SendMessageTimeout(IntPtr hwnd, uint msg, UIntPtr wp, IntPtr lp, uint flags, uint timeout, out UIntPtr result);
  [DllImport("user32.dll")] public static extern int GetMenuItemCount(IntPtr menu);
  [DllImport("user32.dll")] public static extern uint GetMenuItemID(IntPtr menu, int index);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetMenuString(IntPtr menu, uint item, StringBuilder text, int count, uint flags);
  public static IntPtr Find(uint pid, string wanted, bool visible) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((hwnd, _) => {
      uint owner; GetWindowThreadProcessId(hwnd, out owner);
      var text = new StringBuilder(256); GetClassName(hwnd, text, 256);
      if (owner == pid && text.ToString() == wanted && (!visible || IsWindowVisible(hwnd))) { found = hwnd; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
'@
$expectedPath = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\bin\AgentCockpit.exe'))
$dataRoot = $env:AGENT_COCKPIT_HOME
if (-not $dataRoot) {
  $pointer = Join-Path $env:USERPROFILE '.agent-cockpit-root'
  $dataRoot = if (Test-Path -LiteralPath $pointer) { ([IO.File]::ReadAllText($pointer)).Trim() } else { Join-Path $env:USERPROFILE '.agent-cockpit' }
}
$lifecycle = Join-Path $dataRoot 'lifecycle.log'
foreach ($iteration in 1..$Iterations) {
  $instances = @(Get-Process -Name AgentCockpit -ErrorAction SilentlyContinue)
  if ($instances.Count -ne 1 -or $instances[0].Path -ne $expectedPath) { throw 'Expected exactly one installed cockpit instance' }
  $before = $instances[0].Id
  $tray = [RestartTrayNative]::Find($before, 'tray_icon_app', $false)
  if ($tray -eq [IntPtr]::Zero) { throw 'Tray native window missing' }
  # tray-icon's Windows callback: WM_USER_TRAYICON / WM_RBUTTONUP.
  [RestartTrayNative]::PostMessage($tray, 6002, [UIntPtr]::Zero, [IntPtr]0x0205) | Out-Null
  $deadline = [DateTime]::UtcNow.AddSeconds(10)
  do {
    Start-Sleep -Milliseconds 150
    $popup = [RestartTrayNative]::Find($before, '#32768', $true)
  } while ($popup -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $deadline)
  if ($popup -eq [IntPtr]::Zero) { throw 'Tray menu did not open' }
  $menuResult = [UIntPtr]::Zero
  $sent = [RestartTrayNative]::SendMessageTimeout($popup, 0x01E1, [UIntPtr]::Zero, [IntPtr]::Zero, 2, 1000, [ref]$menuResult)
  if ($sent -eq [IntPtr]::Zero) { throw 'Cannot read native tray menu' }
  $menu = [IntPtr]([long]$menuResult.ToUInt64())
  $index = -1
  for ($i = 0; $i -lt [RestartTrayNative]::GetMenuItemCount($menu); $i++) {
    $text = [Text.StringBuilder]::new(256)
    [RestartTrayNative]::GetMenuString($menu, $i, $text, 256, 0x400) | Out-Null
    $restartLabel = -join [char[]](0x91cd, 0x542f, 0x5e94, 0x7528)
    if ($text.ToString().Replace('&','') -eq $restartLabel) { $index = $i; break }
  }
  if ($index -lt 0) { throw 'Restart menu item missing' }
  $command = [RestartTrayNative]::GetMenuItemID($menu, $index)
  if ($command -eq [uint32]::MaxValue) { throw 'Restart entry has no native command ID' }
  # WM_COMMAND is precisely the event produced by selecting this native menu
  # item. Avoid global mouse input: desktop focus/DPI can silently eat a click.
  [RestartTrayNative]::PostMessage($tray, 0x001F, [UIntPtr]::Zero, [IntPtr]::Zero) | Out-Null
  if (-not [RestartTrayNative]::PostMessage($tray, 0x0111, [UIntPtr]$command, [IntPtr]::Zero)) { throw 'Native menu command could not be posted' }
  Write-Host "Selected native restart command $command, PID $before"
  $deadline = [DateTime]::UtcNow.AddSeconds(25)
  do {
    Start-Sleep -Milliseconds 300
    $current = @(Get-Process -Name AgentCockpit -ErrorAction SilentlyContinue)
    $ready = $current.Count -eq 1 -and $current[0].Id -ne $before -and $current[0].MainWindowHandle -ne 0 -and $current[0].Responding
  } while (-not $ready -and [DateTime]::UtcNow -lt $deadline)
  if (-not $ready) { throw "Tray restart failed: original PID $before, current PIDs $($current.Id -join ',')" }
  Start-Sleep -Seconds 2
  $stable = @(Get-Process -Name AgentCockpit -ErrorAction SilentlyContinue)
  if ($stable.Count -ne 1 -or $stable[0].Id -ne $current[0].Id -or $stable[0].Path -ne $expectedPath) { throw 'Replacement did not remain a single installed instance' }
  $loggedExit = Get-Content -LiteralPath $lifecycle -Tail 40 | Where-Object { $_ -match "pid=\s*$before\s+exit\s" }
  if (-not $loggedExit) { throw 'PID changed without a graceful exit log; a concurrent deploy is not a successful tray restart' }
  Write-Host "PASS tray restart $iteration : PID $before -> $($stable[0].Id), single instance, responsive main window"
}
