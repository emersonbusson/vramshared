#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Install-RamSharedService.ps1
<#
.SYNOPSIS
  Thin wrapper for the authoritative Rust two-service product controller.

.DESCRIPTION
  This script performs no copy, ACL, SCM, start, or rollback operation. The
  controller owns the complete manifest transaction.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Manifest,
    [string]$Controller = "C:\ramshared\bin\ramshared-winsvc.exe",
    [ValidateSet("install", "repair")]
    [string]$Operation = "install",
    [switch]$AuthorityOnly
)

$ErrorActionPreference = "Stop"
if (-not [IO.Path]::IsPathRooted($Manifest) -or
    -not (Test-Path -LiteralPath $Manifest -PathType Leaf)) {
    throw "Manifest must be an existing absolute file: $Manifest"
}
if (-not [IO.Path]::IsPathRooted($Controller) -or
    -not (Test-Path -LiteralPath $Controller -PathType Leaf)) {
    throw "Controller must be an existing absolute file: $Controller"
}

$controllerArgs = @($Operation, "--manifest", (Resolve-Path -LiteralPath $Manifest).Path)
if ($AuthorityOnly) {
    $controllerArgs += "--authority-only"
}

& $Controller @controllerArgs
if ($LASTEXITCODE -ne 0) {
    throw "product controller failed: operation=$Operation exit=$LASTEXITCODE"
}
