# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/RootDevice.ps1
# Root-device creation for the Windows lab (AUD-46).
#
# PnPUtil's documented verbs are /add-driver, /delete-driver, /export-driver,
# /enum-drivers, /enum-devices, /enum-devicetree, /disable-device,
# /enable-device, /restart-device, /remove-device, /scan-devices,
# /enum-classes, /enum-interfaces, /enum-containers and /?. There is no
# /add-device. Calling one is how a lab step prints PnPUtil's usage text and
# is then read as success.
#
# Ownership is the hardware-ID list, matched exactly: the live device is
# ROOT\SCSIADAPTER\0000 with hardware id Root\RamShared because the instance
# name is inherited from the first device ID, while a fresh device gets
# ROOT\RAMSHARED\0000. Matching the instance name would create a second
# adapter on a machine that already has the first one.
#
# Root\RamShared is a root-enumerated software device. The lab creates it
# with the WDK tool devcon.exe, which is a lab dependency and is never
# redistributed with the product. The product-side slice — SetupAPI root
# registration in crates/ramshared-winsvc/src/installation/root_device.rs
# (DT-14) — is specified but not implemented yet; until it lands, a clean
# customer host has no product path that creates this device. This helper is
# the lab half only: it never invents a PnPUtil verb, never reports a step it
# did not perform, and never removes a device it does not own.

Set-StrictMode -Version Latest

# The one hardware id this product owns in the lab.
$script:RamSharedOwnedHardwareId = 'Root\RamShared'

function Get-RamSharedOwnedHardwareId {
    return $script:RamSharedOwnedHardwareId
}

function Get-RamSharedLabDevconPath {
    <#
    .SYNOPSIS
      Absolute path of the lab-only devcon.exe, or $null when absent.
    #>
    [CmdletBinding()]
    param(
        [string]$LabBinDir = 'C:\ramshared\bin'
    )
    $candidate = Join-Path $LabBinDir 'devcon.exe'
    if (Test-Path -LiteralPath $candidate) {
        return (Resolve-Path -LiteralPath $candidate).Path
    }
    return $null
}

function Test-RamSharedRootDevicePresent {
    <#
    .SYNOPSIS
      True when exactly the owned Root\RamShared instance is present.
    .DESCRIPTION
      Matching is on the instance id, never on a friendly-name substring,
      so a foreign device whose name happens to contain "RamShared" is not
      counted as owned. The live host shows why the hardware-ID list is the
      key: its instance id is ROOT\SCSIADAPTER\0000 and an instance-name
      match would miss it, so the next install would add a second adapter.
    #>
    [CmdletBinding()]
    param(
        [string]$HardwareId = $script:RamSharedOwnedHardwareId
    )
    $owned = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object {
            $_.InstanceId -and (@($_.HardwareID) -contains $HardwareId)
        })
    return ($owned.Count -gt 0)
}

function Install-RamSharedLabRootDevice {
    <#
    .SYNOPSIS
      Ensure exactly one owned Root\RamShared root-enumerated device exists.
    .DESCRIPTION
      Creates the owned root device when it is absent. A repeat call with the
      device already present is a no-op that reports AlreadyPresent, so an
      install repeated across a reboot cannot grow a second adapter.

      When the device is absent and no supported creator is available the
      call REFUSES and names the gap. It never falls back to a PnPUtil verb
      that does not exist, and it never returns success for a device it did
      not create.

      Returns a typed result:
        Outcome     Created | AlreadyPresent | Refused
        DeviceCount int, owned instances observed after the call
        Detail      human-readable reason
        ExitCode    0 on Created/AlreadyPresent, non-zero on Refused
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string]$InfPath,

        [string]$HardwareId = $script:RamSharedOwnedHardwareId,

        [string]$LabBinDir = 'C:\ramshared\bin'
    )

    $result = [pscustomobject]@{
        Outcome     = 'Refused'
        DeviceCount = 0
        Detail      = ''
        ExitCode    = 1
    }

    if (-not (Test-Path -LiteralPath $InfPath)) {
        $result.Detail = "root registration refused: INF not found at $InfPath"
        return $result
    }

    $present = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object {
            $_.InstanceId -and (@($_.HardwareID) -contains $HardwareId)
        })
    if ($present.Count -gt 0) {
        $result.Outcome = 'AlreadyPresent'
        $result.DeviceCount = $present.Count
        $result.ExitCode = 0
        $result.Detail = "owned root device already present (count=$($present.Count))"
        return $result
    }

    $devcon = Get-RamSharedLabDevconPath -LabBinDir $LabBinDir
    if (-not $devcon) {
        # Refuse closed. The previous behavior called
        # `pnputil /add-device "Root\RamShared"`, a verb PnPUtil does not
        # have, printed its usage text as ADD_DEVICE= and continued to a
        # success claim with no root device at all.
        $result.Detail = "root registration refused: no supported creator. " +
            "devcon.exe is absent from $LabBinDir and PnPUtil has no /add-device verb. " +
            "Production owns this through SetupAPI (see crates/ramshared-winsvc/src/installation/root_device.rs)."
        return $result
    }

    $null = & $devcon install $InfPath $HardwareId 2>&1 | Out-String
    $devconExit = $LASTEXITCODE

    $after = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object {
            $_.InstanceId -and (@($_.HardwareID) -contains $HardwareId)
        })
    $result.DeviceCount = $after.Count

    if ($devconExit -ne 0) {
        $result.Detail = "root registration failed: devcon install exit=$devconExit; owned instances after the call=$($after.Count)"
        return $result
    }

    if ($after.Count -eq 0) {
        # The creator exited zero and produced nothing. That is not success.
        $result.Detail = "root registration failed: devcon install exit=0 but no owned instance of $HardwareId exists"
        return $result
    }

    if ($after.Count -gt 1) {
        $result.Detail = "root registration failed: $($after.Count) owned instances of $HardwareId exist; exactly one is required"
        return $result
    }

    $result.Outcome = 'Created'
    $result.ExitCode = 0
    $result.Detail = "owned root device created (count=1) via lab devcon"
    return $result
}

function Remove-RamSharedLabRootDevice {
    <#
    .SYNOPSIS
      Remove the owned Root\RamShared instance and nothing else.
    .DESCRIPTION
      Removal is by exact owned instance id. A foreign device, an OEM INF
      this product did not publish, or a device whose id does not match the
      owned hardware id is never touched. Removal without devcon is a
      refusal: PnPUtil's /remove-device exists, but it is not used here
      because it accepts wildcards and a lab teardown must not.
    #>
    [CmdletBinding()]
    param(
        [string]$HardwareId = $script:RamSharedOwnedHardwareId,

        [string]$LabBinDir = 'C:\ramshared\bin'
    )

    $result = [pscustomobject]@{
        Outcome     = 'Refused'
        RemovedIds  = @()
        Detail      = ''
        ExitCode    = 1
    }

    $owned = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object {
            $_.InstanceId -and (@($_.HardwareID) -contains $HardwareId)
        })
    if ($owned.Count -eq 0) {
        $result.Outcome = 'Absent'
        $result.ExitCode = 0
        $result.Detail = "no owned instance of $HardwareId to remove"
        return $result
    }

    $devcon = Get-RamSharedLabDevconPath -LabBinDir $LabBinDir
    if (-not $devcon) {
        $result.Detail = "root removal refused: devcon.exe is absent from $LabBinDir; " +
            "an owned device is present and must not be abandoned silently"
        return $result
    }

    $removed = @()
    foreach ($device in $owned) {
        $null = & $devcon remove $device.InstanceId 2>&1 | Out-String
        if ($LASTEXITCODE -eq 0) {
            $removed += $device.InstanceId
        }
    }
    $result.RemovedIds = $removed

    $remaining = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object {
            $_.InstanceId -and (@($_.HardwareID) -contains $HardwareId)
        })
    if ($remaining.Count -gt 0) {
        $result.Detail = "root removal incomplete: $($remaining.Count) owned instance(s) remain"
        return $result
    }

    $result.Outcome = 'Removed'
    $result.ExitCode = 0
    $result.Detail = "removed $($removed.Count) owned instance(s)"
    return $result
}
