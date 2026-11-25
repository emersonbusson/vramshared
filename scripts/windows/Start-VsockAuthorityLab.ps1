#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Start-VsockAuthorityLab.ps1
<#
.SYNOPSIS
  Bounded lab bootstrap for the RamShared vsock lease authority (elevated).

.DESCRIPTION
  Provisions the service GUID, DPAPI-protected host key, and operator vsock.toml,
  exports the raw guest key once, then runs the lab console host in the
  foreground. This does not install or start the Windows service, driver, or
  product package.

  Run from an elevated PowerShell window. The host exits after -Seconds; closing
  the window or Ctrl+C also ends it. Remove the exported guest key after copying
  it to the guest.
#>
[CmdletBinding()]
param(
    [ValidateRange(1, 2147483647)][uint32]$Port = 2761,
    [string]$HostExe = (Join-Path $PSScriptRoot 'ramshared-vsock-host.exe'),
    [ValidateRange(1, 3600)][uint32]$Seconds = 900,
    [string]$VmId = 'auto',
    [string]$KeyFile = '',
    [string]$ExportGuestKey = (Join-Path $env:ProgramData 'RamShared\control-plane\guest-vsock.key')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell window.'
}
if (-not (Test-Path -LiteralPath $HostExe)) { throw "lab host binary not found: $HostExe" }
if (-not (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'Provision-VsockAuthority.ps1'))) {
    throw 'Provision-VsockAuthority.ps1 is missing next to this script'
}

$control = Join-Path $env:ProgramData 'RamShared\control-plane'
$config = Join-Path $control 'vsock.toml'
New-Item -ItemType Directory -Force -Path $control | Out-Null
$transcript = Join-Path $control 'lab-bootstrap.log'
Start-Transcript -Path $transcript -Append | Out-Null
try {
    & (Join-Path $PSScriptRoot 'Provision-VsockAuthority.ps1') `
        -Port $Port -ExportGuestKey $ExportGuestKey -VmId $VmId -KeyFile $KeyFile
    if ($LASTEXITCODE -ne 0) { throw "provisioning failed with exit $LASTEXITCODE" }
    if (-not (Test-Path -LiteralPath $config)) { throw "missing $config" }
    Write-Host "VSOCK_LAB_HOST=$HostExe"
    Write-Host "VSOCK_LAB_CONFIG=$config"
    Write-Host "VSOCK_LAB_SECONDS=$Seconds"
    Write-Host "VSOCK_LAB_VM_ID=$VmId"
    & $HostExe --config $config --seconds $Seconds
    if ($LASTEXITCODE -ne 0) { throw "lab host exited with $LASTEXITCODE" }
} finally {
    $labDir = Split-Path -Parent $HostExe
    Copy-Item (Join-Path $control 'vsock.log') (Join-Path $labDir 'vsock.log') -Force -ErrorAction SilentlyContinue
    Copy-Item $transcript (Join-Path $labDir 'lab-bootstrap.log') -Force -ErrorAction SilentlyContinue
    Write-Host "VSOCK_LAB_TRANSCRIPT=$transcript"
    Stop-Transcript | Out-Null
}
