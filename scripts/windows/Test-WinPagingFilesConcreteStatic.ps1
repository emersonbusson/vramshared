#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-WinPagingFilesConcreteStatic.ps1
[CmdletBinding()]
param(
    [string]$ScriptPath
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($ScriptPath)) {
    $ScriptPath = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) "Set-WinPagingFilesConcrete.ps1"
}
$text = Get-Content -LiteralPath $ScriptPath -Raw

foreach ($needle in @(
    "PagingFiles",
    "pagingfiles-snapshot.json",
    "-Apply -Approve",
    "Refusing apply without -Approve",
    "Refusing restore without -Approve",
    "PLAN_ONLY=1",
    "REBOOT_REQUIRED=1",
    "C:\pagefile.sys 0 0",
    "Set-ItemProperty"
)) {
    if ($text -notmatch [regex]::Escape($needle)) {
        throw ("pagingfiles_concrete_static: missing " + $needle)
    }
}

foreach ($forbidden in @(
    "Initialize-Disk",
    "Format-Volume",
    "New-Partition",
    "Remove-Partition",
    "Clear-Disk",
    "Set-Disk"
)) {
    if ($text -match [regex]::Escape($forbidden)) {
        throw ("pagingfiles_concrete_static: disk mutation forbidden: " + $forbidden)
    }
}

Write-Output "PASS Test-WinPagingFilesConcreteStatic"
