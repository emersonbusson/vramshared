#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Invoke-RamSharedHostLaunchDrill.ps1
<#
.SYNOPSIS
    Live Windows drill for the sealed GUI-subsystem host launcher.

.DESCRIPTION
    Proves the properties that the static contract cannot: the built image is a
    GUI-subsystem executable, stdout survives the launcher, the child exit code
    is forwarded, the deadline returns 124, an empty target returns 2, and no
    visible console window appears while the launcher and its child run.

    This is an attended, host-local drill. It creates no scheduled task, needs
    no elevation, and is not part of the static CI suite.

.PARAMETER LauncherPath
    Built launcher image. Defaults to the staged copy beside this script.

.PARAMETER WindowSeconds
    Window sampling duration. It must cover the child's lifetime.

.EXAMPLE
    .\Invoke-RamSharedHostLaunchDrill.ps1
#>
[CmdletBinding()]
param(
    [string]$LauncherPath,
    [ValidateRange(5, 60)][int]$WindowSeconds = 15
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($LauncherPath)) {
    $LauncherPath = Join-Path $PSScriptRoot "ramshared-host-launch.exe"
}
$LauncherPath = (Resolve-Path -LiteralPath $LauncherPath -ErrorAction Stop).Path
$PowerShellPath = "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"

function Get-PeSubsystem {
    param([Parameter(Mandatory = $true)][string]$Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 512) { throw "host_launch_live_pe_too_short" }
    if ([BitConverter]::ToUInt16($bytes, 0) -ne 0x5A4D) { throw "host_launch_live_pe_dos_signature_missing" }
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    if ($peOffset -lt 0 -or ($peOffset + 24 + 70) -gt $bytes.Length) { throw "host_launch_live_pe_header_out_of_range" }
    if ([BitConverter]::ToUInt32($bytes, $peOffset) -ne 0x00004550) { throw "host_launch_live_pe_signature_missing" }
    $optionalHeader = $peOffset + 24
    $magic = [BitConverter]::ToUInt16($bytes, $optionalHeader)
    if ($magic -ne 0x10B -and $magic -ne 0x20B) { throw "host_launch_live_pe_optional_header_unknown" }
    return [BitConverter]::ToUInt16($bytes, $optionalHeader + 68)
}

function Invoke-Launcher {
    param([Parameter(Mandatory = $true)][string[]]$Arguments)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $LauncherPath
    $psi.Arguments = ($Arguments | ForEach-Object {
        if ($_ -match '\s') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }) -join ' '
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $process = [System.Diagnostics.Process]::Start($psi)
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    return [pscustomobject]@{ exit = $process.ExitCode; stdout = $stdout.Trim(); stderr = $stderr.Trim() }
}

if ((Get-PeSubsystem -Path $LauncherPath) -ne 2) {
    throw "host_launch_live_image_is_not_gui_subsystem"
}
Write-Output "PASS host_launch_live_gui_subsystem"

$drillRoot = Join-Path $env:TEMP ("ramshared-hostlaunch-drill-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $drillRoot | Out-Null
try {
    $samplerPath = Join-Path $drillRoot "window-watch.ps1"
    $samplerOut = Join-Path $drillRoot "windows.json"
    $samplerErr = Join-Path $drillRoot "windows.err"

    $samplerSource = @'
param([int]$Seconds = 15)
Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class WinWatchHostLaunch {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  public static List<string> Windows() {
    var found = new List<string>();
    EnumWindows(delegate(IntPtr h, IntPtr l) {
      if (!IsWindowVisible(h)) return true;
      var cls = new StringBuilder(256);
      GetClassName(h, cls, cls.Capacity);
      if (cls.ToString() != "ConsoleWindowClass") return true;
      var title = new StringBuilder(512);
      GetWindowText(h, title, title.Capacity);
      uint pid; GetWindowThreadProcessId(h, out pid);
      found.Add(pid + "|" + title);
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
"@
$seen = @{}
$log = @()
$end = (Get-Date).AddSeconds($Seconds)
while ((Get-Date) -lt $end) {
  foreach ($entry in [WinWatchHostLaunch]::Windows()) {
    if ($seen.ContainsKey($entry)) { continue }
    $seen[$entry] = $true
    $parts = $entry.Split('|', 2)
    $owner = Get-CimInstance Win32_Process -Filter ("ProcessId=" + $parts[0]) -ErrorAction SilentlyContinue
    $log += [pscustomobject]@{
      at = (Get-Date).ToString('HH:mm:ss.fff')
      pid = $parts[0]
      title = $parts[1]
      process = if ($null -ne $owner) { $owner.Name } else { $null }
      ppid = if ($null -ne $owner) { $owner.ParentProcessId } else { $null }
    }
  }
  Start-Sleep -Milliseconds 80
}
if ($log.Count -eq 0) { Write-Output '[]' } else { $log | ConvertTo-Json -Compress }
'@
    Set-Content -LiteralPath $samplerPath -Value $samplerSource -Encoding UTF8

    $sampler = Start-Process -FilePath $PowerShellPath `
        -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',$samplerPath,'-Seconds',[string]$WindowSeconds) `
        -RedirectStandardOutput $samplerOut -RedirectStandardError $samplerErr -WindowStyle Hidden -PassThru
    Start-Sleep -Milliseconds 700

    $roundTrip = Invoke-Launcher @('--timeout-ms','10000','--',$PowerShellPath,'-NoLogo','-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 4; Write-Output DRILL_STDOUT_OK')
    if ($roundTrip.exit -ne 0) { throw ("host_launch_live_round_trip_exit_" + $roundTrip.exit) }
    if ($roundTrip.stdout -ne "DRILL_STDOUT_OK") { throw "host_launch_live_round_trip_stdout_missing" }
    Write-Output "PASS host_launch_live_round_trip"

    $timeout = Invoke-Launcher @('--timeout-ms','800','--',$PowerShellPath,'-NoLogo','-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 30')
    if ($timeout.exit -ne 124) { throw ("host_launch_live_timeout_exit_" + $timeout.exit) }
    Write-Output "PASS host_launch_live_timeout"

    $usage = Invoke-Launcher @('--timeout-ms','1000')
    if ($usage.exit -ne 2) { throw ("host_launch_live_usage_exit_" + $usage.exit) }
    if (-not $usage.stderr.Contains("target program is empty")) { throw "host_launch_live_usage_stderr_missing" }
    Write-Output "PASS host_launch_live_usage_refusal"

    Wait-Process -Id $sampler.Id -Timeout ($WindowSeconds + 30)
    $windowsJson = Get-Content -Raw -LiteralPath $samplerOut -ErrorAction SilentlyContinue
    if ($null -eq $windowsJson) { $windowsJson = '' }
    if ($windowsJson.Trim() -ne '[]') {
        throw ("host_launch_live_visible_windows: " + $windowsJson.Trim())
    }
    Write-Output "PASS host_launch_live_zero_visible_windows"
} finally {
    Remove-Item -LiteralPath $drillRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output "HOST_LAUNCH_LIVE_DRILL=PASS"
