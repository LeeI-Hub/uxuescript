param([string]$ProcessName = "uxs-2.5.12-control-win-x64", [int]$Hours = 8)
# 只为「刷课进程活着」时阻止系统睡眠（不进睡眠/休眠），不阻止熄屏。
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class KA {
  [DllImport("kernel32.dll")] public static extern uint SetThreadExecutionState(uint flags);
}
"@
$ES_CONTINUOUS       = 0x80000000
$ES_SYSTEM_REQUIRED  = 0x00000001
$deadline = (Get-Date).AddHours($Hours)
while ((Get-Date) -lt $deadline) {
    $alive = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue
    if (-not $alive) { break }
    [KA]::SetThreadExecutionState([uint32]($ES_CONTINUOUS -bor $ES_SYSTEM_REQUIRED)) | Out-Null
    Start-Sleep -Seconds 20
}
[KA]::SetThreadExecutionState([uint32]$ES_CONTINUOUS) | Out-Null
