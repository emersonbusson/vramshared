#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-HardenLabVmsStatic.ps1
[CmdletBinding()]
param(
    [string]$ScriptPath
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($ScriptPath)) {
    $ScriptPath = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) "Harden-LabVms.ps1"
}
$text = Get-Content -LiteralPath $ScriptPath -Raw

foreach ($needle in @(
    'linux-kernel-lab',
    'win11-drill',
    'AutomaticCheckpointsEnabled $false',
    'CheckpointType Disabled',
    'Set-VMMemory -VMName $name -DynamicMemoryEnabled $true -StartupBytes 2GB -MinimumBytes 1GB -MaximumBytes 8GB',
    'DONE Harden-LabVms (no destructive disk ops)'
)) {
    if ($text -notmatch [regex]::Escape($needle)) {
        throw ("harden_lab_vms_static: missing " + $needle)
    }
}

$withoutBlockComments = [regex]::Replace($text, '(?s)<#.*?#>', '')
$activeText = (($withoutBlockComments -split "`r?`n") | Where-Object { $_ -notmatch '^\s*#' }) -join "`n"
if ($activeText -match 'Remove-VHD|Resize-VHD|Convert-VHD|Optimize-VHD|Clear-Disk|Initialize-Disk|Format-Volume|Remove-VMHardDiskDrive') {
    throw "harden_lab_vms_static: destructive disk operation is forbidden"
}

Write-Output "PASS Test-HardenLabVmsStatic"
