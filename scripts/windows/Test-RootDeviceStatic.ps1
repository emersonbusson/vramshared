#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-RootDeviceStatic.ps1
<#
.SYNOPSIS
  Windows lab root-device contracts (AUD-46 lab half).

.DESCRIPTION
  PnPUtil has no /add-device verb. Its documented verbs are /add-driver,
  /delete-driver, /export-driver, /enum-drivers, /enum-devices,
  /enum-devicetree, /disable-device, /enable-device, /restart-device,
  /remove-device, /scan-devices, /enum-classes, /enum-interfaces,
  /enum-containers and /?. A lab step that called /add-device printed usage
  text, wrote it to a log as ADD_DEVICE= and continued to a success claim
  with no root device at all.

  This suite pins the lab half of the fix. Production owns root registration
  through SetupAPI in crates/ramshared-winsvc/src/installation/root_device.rs
  (DT-14); those tests carry their own names and are not claimed here.

  Named tests:
    lab_root_creation_never_invents_a_pnputil_verb
    lab_root_registration_refuses_closed_without_a_supported_creator
    lab_root_repeat_keeps_exactly_one_owned_adapter
    lab_root_removal_touches_only_the_owned_instance_id

  The Windows API effects themselves are environment-bound. These are static
  contracts on the shipped lab scripts.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    $RepoRoot = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path))
}

$windowsDir = Join-Path $RepoRoot "scripts\windows"
$rootDevicePath = Join-Path $windowsDir "RootDevice.ps1"
if (-not (Test-Path -LiteralPath $rootDevicePath -PathType Leaf)) {
    throw "lab_root_creation_never_invents_a_pnputil_verb: RootDevice.ps1 is missing"
}
$rootDevice = Get-Content -LiteralPath $rootDevicePath -Raw

# Comments legitimately name the retired verb in order to forbid it, so the
# forbidden patterns are matched against code with comments stripped - the
# same rule the trust suite applies to /pa.
function Get-CodeOnly([string]$raw) {
    $noBlock = $raw -replace '(?s)<#.*?#>', ''
    return (($noBlock -split '\r?\n' | ForEach-Object { $_ -replace '#.*$', '' }) -join "`n")
}

$rootCode = Get-CodeOnly $rootDevice

# --- lab_root_creation_never_invents_a_pnputil_verb --------------------------
# The forbidden shape is an INVOCATION: `pnputil` immediately followed by a
# slash-verb. A refusal string that says "PnPUtil has no /add-device verb" is
# not an invocation and must keep working, so the gap stays named in code.
$allScripts = @(Get-ChildItem -LiteralPath $windowsDir -Filter '*.ps1' -File |
        Where-Object { $_.Name -ne 'Test-RootDeviceStatic.ps1' })
if ($allScripts.Count -eq 0) {
    throw "lab_root_creation_never_invents_a_pnputil_verb: no lab scripts found in $windowsDir"
}
foreach ($script in $allScripts) {
    $code = Get-CodeOnly (Get-Content -LiteralPath $script.FullName -Raw)
    if ($code -match '(?i)pnputil\s+/add-device') {
        throw "lab_root_creation_never_invents_a_pnputil_verb: $($script.Name) invokes the retired PnPUtil device-creation verb"
    }
}
# The one owner must never shell out to PnPUtil for device creation or removal.
if ($rootCode -match '(?i)pnputil\s+/\w+') {
    throw "lab_root_creation_never_invents_a_pnputil_verb: RootDevice.ps1 must not invoke pnputil at all for creation or removal"
}
# And the module must keep recording why the old call was wrong.
if ($rootDevice -notmatch '(?i)(there is no|has no|does not exist)[^\r\n]{0,80}/add-device') {
    throw "lab_root_creation_never_invents_a_pnputil_verb: RootDevice.ps1 must record that PnPUtil has no /add-device verb"
}
# Every call site must use the one owner. A second copy of the creation logic
# is how the nonexistent verb came back the first time.
$callSites = @(
    "Start-RamSharedLab.ps1",
    "Install-InfAndBackend.ps1",
    "Install-WinDriveVm.ps1"
)
foreach ($site in $callSites) {
    $path = Join-Path $windowsDir $site
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "lab_root_creation_never_invents_a_pnputil_verb: $site is missing"
    }
    $code = Get-CodeOnly (Get-Content -LiteralPath $path -Raw)
    if ($code -notmatch 'RootDevice\.ps1') {
        throw "lab_root_creation_never_invents_a_pnputil_verb: $site does not use the shared RootDevice.ps1 owner"
    }
    if ($code -notmatch 'Install-RamSharedLabRootDevice') {
        throw "lab_root_creation_never_invents_a_pnputil_verb: $site does not call Install-RamSharedLabRootDevice"
    }
    if ($code -notmatch '\$LASTEXITCODE') {
        throw "lab_root_creation_never_invents_a_pnputil_verb: $site does not check a native exit status"
    }
    # A call site that refuses the helper's refusal is how a missing root
    # device becomes a success claim again.
    if ($code -notmatch 'root registration refused') {
        throw "lab_root_creation_never_invents_a_pnputil_verb: $site does not honour the helper's refusal"
    }
}
Write-Output "PASS lab_root_creation_never_invents_a_pnputil_verb"

# --- lab_root_registration_refuses_closed_without_a_supported_creator --------
# No creator, no device, no success. The helper must return a refusal that
# names the gap rather than a zero exit for a device it did not create.
if ($rootCode -notmatch 'Refused') {
    throw "lab_root_registration_refuses_closed_without_a_supported_creator: RootDevice.ps1 has no Refused outcome"
}
if ($rootCode -notmatch 'no supported creator') {
    throw "lab_root_registration_refuses_closed_without_a_supported_creator: the refusal must name the gap"
}
if ($rootCode -notmatch 'PnPUtil has no /add-device') {
    throw "lab_root_registration_refuses_closed_without_a_supported_creator: the refusal must name why the old verb was wrong"
}
if ($rootCode -notmatch 'SetupAPI') {
    throw "lab_root_registration_refuses_closed_without_a_supported_creator: the refusal must name the production owner"
}
# A zero creator exit with no instance is still a failure.
if ($rootCode -notmatch 'no owned instance') {
    throw "lab_root_registration_refuses_closed_without_a_supported_creator: exit=0 with no instance must not be success"
}
if ($rootCode -notmatch 'ExitCode') {
    throw "lab_root_registration_refuses_closed_without_a_supported_creator: result must carry an ExitCode"
}
# Refusal is the initial state, so an early return cannot report success.
if ($rootDevice -notmatch "Outcome\s*=\s*'Refused'") {
    throw "lab_root_registration_refuses_closed_without_a_supported_creator: Refused must be the default outcome"
}
Write-Output "PASS lab_root_registration_refuses_closed_without_a_supported_creator"

# --- lab_root_repeat_keeps_exactly_one_owned_adapter -------------------------
# A repeated install must report AlreadyPresent and must not create a second
# adapter. The after-count is checked, and more than one is a failure.
if ($rootCode -notmatch 'AlreadyPresent') {
    throw "lab_root_repeat_keeps_exactly_one_owned_adapter: no AlreadyPresent outcome"
}
if ($rootCode -notmatch 'exactly one is required') {
    throw "lab_root_repeat_keeps_exactly_one_owned_adapter: more than one owned instance must fail"
}
if ($rootCode -notmatch 'DeviceCount') {
    throw "lab_root_repeat_keeps_exactly_one_owned_adapter: result must report the owned device count"
}
# AlreadyPresent must short-circuit before any creator is invoked.
if ($rootDevice -notmatch "Outcome\s*=\s*'AlreadyPresent'") {
    throw "lab_root_repeat_keeps_exactly_one_owned_adapter: AlreadyPresent must be assigned, not inferred"
}
Write-Output "PASS lab_root_repeat_keeps_exactly_one_owned_adapter"

# --- lab_root_removal_touches_only_the_owned_instance_id ---------------------
# Ownership is the exact hardware-ID list, never a friendly name and never an
# instance-name prefix: the live device is ROOT\SCSIADAPTER\0000 with hardware
# id Root\RamShared, so a prefix match would miss it and add a second adapter.
# Removal addresses the observed instance by its exact id.
if ($rootCode -match 'FriendlyName') {
    throw "lab_root_removal_touches_only_the_owned_instance_id: ownership must not be decided by FriendlyName"
}
if ($rootCode -notmatch 'HardwareID') {
    throw "lab_root_removal_touches_only_the_owned_instance_id: ownership must be decided by the hardware-ID list"
}
if ($rootCode -match 'InstanceId -like') {
    throw "lab_root_removal_touches_only_the_owned_instance_id: an instance-name prefix misses ROOT\SCSIADAPTER\0000"
}
if ($rootCode -notmatch 'Remove-RamSharedLabRootDevice') {
    throw "lab_root_removal_touches_only_the_owned_instance_id: no removal owner"
}
if ($rootCode -notmatch '\$device\.InstanceId') {
    throw "lab_root_removal_touches_only_the_owned_instance_id: removal must use the exact owned instance id"
}
if ($rootCode -notmatch 'must not be abandoned') {
    throw "lab_root_removal_touches_only_the_owned_instance_id: removal must refuse when the owned device cannot be taken down"
}
# Removal without a creator is a refusal, not a silent no-op: an owned device
# is already on the machine and abandoning it is worse than failing loudly.
if ($rootDevice -notmatch "root removal refused") {
    throw "lab_root_removal_touches_only_the_owned_instance_id: removal without a creator must refuse"
}
Write-Output "PASS lab_root_removal_touches_only_the_owned_instance_id"

Write-Output "PASS Test-RootDeviceStatic"
