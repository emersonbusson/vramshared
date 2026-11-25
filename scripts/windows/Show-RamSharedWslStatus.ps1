#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Show-RamSharedWslStatus.ps1
<#
.SYNOPSIS
  Show the last RamShared guest observation without entering the guest.

.DESCRIPTION
  Reads only the host-visible heartbeat written by the RamShared monitor. The
  default is a plan preview. Use -Run to read the file and -Json for machine
  output. This viewer starts no process and performs no lifecycle operation.
#>
[CmdletBinding()]
param(
    [switch]$Run,
    [switch]$Json,
    [string]$HeartbeatPath = "C:\wsl-forensics\ramshared-heartbeat.json",
    [ValidateRange(3, 300)][int]$StaleAfterSec = 5
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# The schema version this viewer can witness.
#
# Producer of truth: the `render_status_json` emit in
# `crates/ramshared-cli/src/cascade/lifecycle.rs`, whose wire contract is
# `Documentation/architecture/three-tier-cascade.rst` DT-4
# (the current `schema_version` is 5: the tier role and the cache cause sit
# beside the state they qualify). The monitor heartbeat flattens that status
# document into itself, so the version this file gates on is the status
# document's own.
#
# `Test-RamSharedWslStatusStatic.ps1` reads the producer's emit and pins this
# constant to it. If the producer moves and this does not, that gate fails
# there — rather than the viewer silently refusing the live document again,
# which is the defect this replaces (the viewer sat on `3` while the product
# had published `4` for its whole schema-4 life).
$SupportedHeartbeatSchemaVersion = 5

function Get-ObservationProperty {
    param(
        [Parameter(Mandatory = $true)]$Object,
        [Parameter(Mandatory = $true)][string]$Name,
        $Default = $null
    )
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $Default
    }
    return $property.Value
}

function ConvertTo-OptionalMiB {
    # A KiB figure that may never have arrived, published in MiB.
    # `0.0` MiB is the measured value of a present figure holding nothing;
    # `$null` is a figure that never arrived. The two are different facts and
    # must not share an arm.
    param($Kib)
    if ($null -eq $Kib) { return $null }
    return [Math]::Round(([double]$Kib) / 1024, 1)
}

function ConvertTo-OptionalDouble {
    # A scalar figure that may never have arrived. `0.0` is a measured zero;
    # `$null` is a figure that never arrived.
    param($Value)
    if ($null -eq $Value) { return $null }
    return [double]$Value
}

function ConvertTo-OptionalBool {
    # A boolean claim that may never have arrived. `$false` is a measured
    # refutation; `$null` is a claim that was never taken.
    param($Value)
    if ($null -eq $Value) { return $null }
    return [bool]$Value
}

function Get-TierUsedKib {
    # A tier's hold, published only on a tier the wire measured as present.
    #
    # `present: false` is not a tier holding nothing — it is no tier — so a
    # hold figure beside it is not a claim this face can make. `used_kib: 0`
    # on a measured-present tier is the zero arm; `used_kib` that never
    # arrived is `$null`. All three are different facts and the first must
    # not share the second's arm.
    #
    # `present` is read as a bool witness and nothing else: a `present` that
    # is not a bool this face can read (`"true"`, `1`, `null`) is never
    # taken, exactly like the daemon-side `extract_bool` refusal.
    param($Tier)
    if ($null -eq $Tier) { return $null }
    $property = $Tier.PSObject.Properties["present"]
    if ($null -eq $property) { return $null }
    $present = $property.Value
    if ($present -isnot [bool]) { return $null }
    if (-not $present) { return $null }
    return Get-ObservationProperty -Object $Tier -Name "used_kib"
}

if (-not $Run) {
    [ordered]@{
        state = "PLAN"
        heartbeat = $HeartbeatPath
        stale_after_seconds = $StaleAfterSec
        action = "read_host_heartbeat_only"
    } | ConvertTo-Json -Depth 3
    exit 0
}

$heartbeat = Get-Item -LiteralPath $HeartbeatPath -ErrorAction Stop
$record = Get-Content -Raw -LiteralPath $HeartbeatPath | ConvertFrom-Json
$schemaVersion = Get-ObservationProperty -Object $record -Name "schema_version"
if ($null -eq $schemaVersion) {
    throw "RamShared heartbeat has no schema_version; refusing to guess one"
}
if ([int]$schemaVersion -ne [int]$SupportedHeartbeatSchemaVersion) {
    throw "Unsupported RamShared heartbeat schema: $schemaVersion (this viewer witnesses $SupportedHeartbeatSchemaVersion)"
}

$fileAgeMs = [Math]::Max(0, ([DateTime]::UtcNow - $heartbeat.LastWriteTimeUtc).TotalMilliseconds)
$reportedAgeMs = ConvertTo-OptionalDouble (Get-ObservationProperty -Object $record -Name "sample_age_ms")
# The age is the file's own age plus the sample age the producer reported. A
# missing reported half is not `0` ms — it is a half that never arrived, so the
# sum is not a measured figure and the viewer must not claim LIVE on it.
$sampleAgeMs = if ($null -eq $reportedAgeMs) { $null } else { [Math]::Round($fileAgeMs + $reportedAgeMs) }
$staleThresholdMs = $StaleAfterSec * 1000
$state = if ($null -ne $sampleAgeMs) {
    if ($sampleAgeMs -gt $staleThresholdMs) { "STALE" } else { "LIVE" }
} elseif ($fileAgeMs -gt $staleThresholdMs) {
    # The file age alone already exceeds the threshold, so the sample is stale
    # regardless of the half that never arrived. That is a measured refutation
    # of freshness, not a guess.
    "STALE"
} else {
    # Freshness cannot be claimed without the reported half.
    "UNKNOWN"
}

$memory = Get-ObservationProperty -Object $record -Name "mem" -Default ([pscustomobject]@{})
$pressure = Get-ObservationProperty -Object $record -Name "control_plane" -Default ([pscustomobject]@{})
$gpu = Get-ObservationProperty -Object $record -Name "gpu"
$activation = Get-ObservationProperty -Object $record -Name "activation" -Default ([pscustomobject]@{})
$tiers = Get-ObservationProperty -Object $record -Name "tiers" -Default ([pscustomobject]@{})
$vramTier = Get-ObservationProperty -Object $tiers -Name "vram" -Default ([pscustomobject]@{})
$diskTier = Get-ObservationProperty -Object $tiers -Name "disk" -Default ([pscustomobject]@{})

$view = [ordered]@{
    state = $state
    sample_age_ms = $sampleAgeMs
    protection_state = Get-ObservationProperty -Object $record -Name "protection_state" -Default "UNKNOWN"
    protection_reason = Get-ObservationProperty -Object $record -Name "protection_reason" -Default "unknown"
    phase = Get-ObservationProperty -Object $record -Name "phase" -Default "Unknown"
    product_active = ConvertTo-OptionalBool (Get-ObservationProperty -Object $activation -Name "active")
    binary_version = Get-ObservationProperty -Object $activation -Name "binary_version" -Default "unknown"
    ram_available_mib = ConvertTo-OptionalMiB (Get-ObservationProperty -Object $memory -Name "available_kib")
    swap_free_mib = ConvertTo-OptionalMiB (Get-ObservationProperty -Object $memory -Name "swap_free_kib")
    memory_psi_full_avg10 = ConvertTo-OptionalDouble (Get-ObservationProperty -Object $pressure -Name "memory_psi_full_avg10")
    gpu_used_mib = if ($null -eq $gpu) { $null } else { Get-ObservationProperty -Object $gpu -Name "used_mib" }
    gpu_free_mib = if ($null -eq $gpu) { $null } else { Get-ObservationProperty -Object $gpu -Name "free_mib" }
    guaranteed_vram_kib = Get-ObservationProperty -Object (Get-ObservationProperty -Object $record -Name "capacity" -Default ([pscustomobject]@{})) -Name "guaranteed_kib"
    vram_swap_used_kib = Get-TierUsedKib -Tier $vramTier
    disk_swap_used_kib = Get-TierUsedKib -Tier $diskTier
    disk_baseline_kib = Get-ObservationProperty -Object $activation -Name "disk_baseline_kib"
    disk_growth_kib = Get-ObservationProperty -Object $activation -Name "disk_growth_kib"
    heartbeat = $HeartbeatPath
}

if ($Json) {
    $view | ConvertTo-Json -Depth 4
} else {
    [pscustomobject]$view
}
