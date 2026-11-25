#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Provision-VsockAuthority.ps1
<#
.SYNOPSIS
  Provision the RamShared native-vsock lease authority (elevated).

.DESCRIPTION
  Creates the Linux guest service GUID for the configured port under the
  Hyper-V GuestCommunicationServices registry key, protects the shared key with
  DPAPI machine scope, and writes an operator-managed vsock.toml under a
  SYSTEM/Administrators-only control-plane directory.

  The host key blob is never printed. Supply the raw guest key with -KeyFile,
  or let this script generate one and export it once with -ExportGuestKey.
  Install that raw key root-only (0600) on the guest before starting the daemon.

.PARAMETER Port
  Guest-visible AF_VSOCK port in 1..2147483647. The GUID is derived from it; it
  must match RAMSHARED_VSOCK_PORT on the guest.

.PARAMETER KeyFile
  Raw key file to protect and install. When omitted, an existing destination
  blob is reused, or a 32-byte key is generated once.

.PARAMETER ExportGuestKey
  Write the raw key to this path for secure transfer to the guest. Delete the
  export immediately after transfer.

.PARAMETER RestartService
  Cycle RamSharedWinSvc after provisioning so the listener binds the new GUID.

.PARAMETER VerifyOnly
  Check the registry, config, DPAPI unprotect, and ACLs without changing
  anything.
#>
[CmdletBinding()]
param(
    [ValidateRange(1, 2147483647)][uint32]$Port = 2761,
    [string]$ConfigRoot = (Join-Path $env:ProgramData 'RamShared'),
    [string]$KeyFile = '',
    [string]$ExportGuestKey = '',
    [string]$FriendlyName = 'RamShared WSL2 vsock lease',
    [string]$VmId = '',
    [switch]$RestartService,
    [switch]$VerifyOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.Security

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'This script must run from an elevated Windows session.'
    }
}

function Get-ServiceGuid([uint32]$port) {
    return '{0:X8}-FACB-11E6-BD58-64006A7986D3' -f $port
}

function Set-DirectoryAcl([string]$path) {
    & icacls.exe $path /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls failed for $path (exit $LASTEXITCODE)" }
}

function Set-FileAcl([string]$path) {
    & icacls.exe $path /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls failed for $path (exit $LASTEXITCODE)" }
}

if ($ExportGuestKey -and $VerifyOnly) {
    throw 'ExportGuestKey and VerifyOnly are mutually exclusive.'
}
Assert-Administrator

function Get-RunningWslVmId {
    $hosts = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -eq 'wslhost.exe' -and $_.CommandLine -match '--vm-id' })
    $ids = @()
    foreach ($hostProcess in $hosts) {
        if ($hostProcess.CommandLine -match '--vm-id\s+\{?([0-9a-fA-F-]{36})\}?') {
            $ids += $Matches[1]
        }
    }
    $unique = @($ids | Select-Object -Unique)
    if ($unique.Count -eq 1) { return $unique[0] }
    if ($unique.Count -gt 1) {
        throw 'multiple running WSL VMs found; pass -VmId explicitly'
    }
    return $null
}

$keyProvided = $PSBoundParameters.ContainsKey('KeyFile') -and $KeyFile -ne ''
$controlDir = Join-Path $ConfigRoot 'control-plane'
$configPath = Join-Path $controlDir 'vsock.toml'
$destKey = Join-Path $controlDir 'vsock.key'
$guid = Get-ServiceGuid $Port
$serviceRoot = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices'
$serviceKey = Join-Path $serviceRoot $guid
$entropy = [Text.Encoding]::UTF8.GetBytes('ramshared-vsock-key-v1')
$scope = [System.Security.Cryptography.DataProtectionScope]::LocalMachine

$autoVmId = $VmId -ieq 'auto'
if ($autoVmId) { $VmId = 'auto' }
if (-not $autoVmId -and -not $VmId) { $VmId = Get-RunningWslVmId }
if (-not $autoVmId -and -not $VmId) {
    throw 'could not discover the running WSL VM ID; pass -VmId or -VmId auto'
}
if (-not $autoVmId) {
    if ($VmId -notmatch '^\{?[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\}?$') {
        throw "invalid -VmId: $VmId"
    }
    $VmId = $VmId.Trim('{', '}')
}

if (-not $keyProvided) { $KeyFile = $destKey }

if ($VerifyOnly) {
    if (-not (Test-Path $serviceKey)) { throw "missing registry key $serviceKey" }
    $element = (Get-ItemProperty -Path $serviceKey -Name ElementName -ErrorAction Stop).ElementName
    if ($element -ne $FriendlyName) { throw "ElementName mismatch: $element" }
    if (-not (Test-Path $configPath)) { throw "missing $configPath" }
    if (-not (Test-Path $destKey)) { throw "missing $destKey" }
    $blob = [IO.File]::ReadAllBytes($destKey)
    $raw = [System.Security.Cryptography.ProtectedData]::Unprotect($blob, $entropy, $scope)
    if ($raw.Length -lt 1 -or $raw.Length -gt 4096) { throw "key length $($raw.Length) outside 1..4096" }
    Write-Host 'VSOCK_VERIFY=OK'
    Write-Host "VSOCK_GUID=$guid"
    Write-Host "VSOCK_PORT=$Port"
    Write-Host "VSOCK_CONFIG=$configPath"
    Write-Host "VSOCK_KEY=$destKey"
    Write-Host 'VSOCK_KEY_PROTECTION=dpapi_machine'
    Write-Host "VSOCK_SECRET_BYTES=$($raw.Length)"
    Write-Host "VSOCK_KEY_BLOB_BYTES=$($blob.Length)"
    Write-Host 'VSOCK_KEY_ACL:'
    & icacls.exe $destKey
    exit 0
}

New-Item -ItemType Directory -Force -Path $controlDir | Out-Null
Set-DirectoryAcl $controlDir

$rawKey = $null
if ($keyProvided) {
    $source = Get-Item -Path $KeyFile -ErrorAction Stop
    if ($source.PSIsContainer) { throw "$KeyFile is a directory, not a key file" }
    if ($source.Length -lt 1 -or $source.Length -gt 4096) {
        throw "supplied key length $($source.Length) outside 1..4096"
    }
    $rawKey = [IO.File]::ReadAllBytes($source.FullName)
} elseif (Test-Path $destKey) {
    $blob = [IO.File]::ReadAllBytes($destKey)
    try {
        $rawKey = [System.Security.Cryptography.ProtectedData]::Unprotect($blob, $entropy, $scope)
    } catch {
        throw "existing $destKey is not a DPAPI machine blob; supply -KeyFile with the raw key or remove the file"
    }
    if ($rawKey.Length -lt 1 -or $rawKey.Length -gt 4096) {
        throw "existing key length $($rawKey.Length) outside 1..4096"
    }
} else {
    $rawKey = New-Object byte[] 32
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($rawKey)
    } finally {
        $rng.Dispose()
    }
}

$protected = [System.Security.Cryptography.ProtectedData]::Protect($rawKey, $entropy, $scope)
[System.IO.File]::WriteAllBytes($destKey, $protected)
Set-FileAcl $destKey

$tomlKey = $destKey.Replace('\', '\\').Replace('"', '\"')
$toml = "[vsock]`nport = $Port`nkey_file = `"$tomlKey`"`nkey_protection = `"dpapi_machine`"`nmode = `"connect`"`nvm_id = `"$VmId`"`nheartbeat_secs = 1`nlease_timeout_secs = 15`n"
[System.IO.File]::WriteAllText($configPath, $toml, (New-Object System.Text.UTF8Encoding($false)))
Set-FileAcl $configPath

if (-not (Test-Path $serviceKey)) {
    New-Item -Path $serviceKey | Out-Null
}
New-ItemProperty -Path $serviceKey -Name ElementName -Value $FriendlyName -PropertyType String -Force | Out-Null
$element = (Get-ItemProperty -Path $serviceKey -Name ElementName).ElementName
if ($element -ne $FriendlyName) { throw "registry read-back mismatch: $element" }

if ($ExportGuestKey) {
    [System.IO.File]::WriteAllBytes($ExportGuestKey, $rawKey)
    Write-Warning "Raw guest key written to $ExportGuestKey; transfer it over a trusted channel and delete it immediately."
}

if ($RestartService) {
    $service = Get-Service -Name 'RamSharedWinSvc' -ErrorAction Stop
    if ($service.Status -ne 'Stopped') {
        Stop-Service -Name 'RamSharedWinSvc' -Force
    }
    Start-Service -Name 'RamSharedWinSvc'
    $status = (Get-Service -Name 'RamSharedWinSvc').Status
    if ($status -ne 'Running') { throw "RamSharedWinSvc did not reach Running: $status" }
}

Write-Host 'VSOCK_PROVISION=OK'
Write-Host "VSOCK_GUID=$guid"
Write-Host "VSOCK_PORT=$Port"
Write-Host "VSOCK_VM_ID=$VmId"
Write-Host "VSOCK_CONFIG=$configPath"
Write-Host "VSOCK_KEY=$destKey"
Write-Host 'VSOCK_KEY_PROTECTION=dpapi_machine'
Write-Host "VSOCK_SECRET_BYTES=$($rawKey.Length)"
Write-Host "VSOCK_KEY_BLOB_BYTES=$($protected.Length)"
Write-Host "Next: transfer the raw key to the guest (or run this script once with -ExportGuestKey), install it root-only 0600, set RAMSHARED_VSOCK_PORT=$Port and RAMSHARED_VSOCK_KEY_FILE=<guest key path> for ramsharedd, then re-run the cascade."
