$ErrorActionPreference = 'Stop'
$procs = @(Get-Process -Name AgentCockpit -ErrorAction SilentlyContinue)
if ($procs.Count -gt 1) {
    Write-Error ("ABORT: {0} cockpit instances running; close the extras first" -f $procs.Count)
    exit 1
}
$listen = @(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue | Where-Object { $_.LocalPort -eq 8788 })
if ($listen.Count -gt 0) {
    Write-Host ("NOTE: proxy port 8788 in use (PID {0}); deploy restarts the process and briefly interrupts the proxy" -f $listen[0].OwningProcess)
}
exit 0
