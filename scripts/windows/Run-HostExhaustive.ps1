#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Run-HostExhaustive.ps1
<#
.SYNOPSIS
  Compatibility entry for the supervised packaged physical lifecycle.

.DESCRIPTION
  The historical inline TCP/PowerShell broker and force-kill cleanup were
  removed. The autonomous host harness owns the one manifest and watchdog.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Manifest,
    [switch]$ApprovePhysicalHost,
    [int]$ColdBoots = 3
)

$ErrorActionPreference = "Stop"
& (Join-Path $PSScriptRoot "Run-HostAutonomousLifecycle.ps1") `
    -Manifest $Manifest -ColdBoots $ColdBoots `
    -ApprovePhysicalHost:$ApprovePhysicalHost
