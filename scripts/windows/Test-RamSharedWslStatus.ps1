#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-RamSharedWslStatus.ps1
<#
.SYNOPSIS
  Live counterexamples for Show-RamSharedWslStatus.ps1.

.DESCRIPTION
  Writes synthetic heartbeats to a temp path and runs the viewer against each
  one. Each case pins one arm of a claim face: a measured zero must stay a
  zero, a figure that never arrived must stay absent, and freshness may not be
  claimed from half an age.

  The static contract lives in Test-RamSharedWslStatusStatic.ps1. This file is
  the executable side: it proves the arms, not the string shapes.
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$target = Join-Path $PSScriptRoot "Show-RamSharedWslStatus.ps1"
if (-not (Test-Path -LiteralPath $target -PathType Leaf)) {
    throw "ramshared_wsl_status: target script is missing"
}

$workDir = Join-Path ([System.IO.Path]::GetTempPath()) ("ramshared-wsl-status-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $workDir -Force | Out-Null
$caseCount = 0

function Write-Heartbeat {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Body,
        $LastWriteUtc = $null
    )
    $path = Join-Path $workDir "$Name.json"
    # The fixture writer is a test tool, not the viewer: the viewer itself is
    # forbidden from mutating the host, which the static gate pins.
    Set-Content -LiteralPath $path -Value $Body -Encoding UTF8
    if ($null -ne $LastWriteUtc) {
        [System.IO.File]::SetLastWriteTimeUtc($path, [DateTime]$LastWriteUtc)
    }
    return $path
}

function Invoke-Viewer {
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )
    $raw = (& $target -Run -Json -HeartbeatPath $Path -StaleAfterSec 5) -join "`n"
    return ($raw | ConvertFrom-Json)
}

function Get-ViewValue {
    # StrictMode makes a missing property throw; the viewer may publish an
    # absent figure either as a `null` key or by omitting the key, and both
    # mean "never arrived". Read through the property bag so the assertion
    # tests the fact, not the serializer's key policy.
    param(
        [Parameter(Mandatory = $true)]$View,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $property = $View.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    return $property.Value
}

function Assert-Null {
    param(
        [Parameter(Mandatory = $true)]$View,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Case
    )
    $value = Get-ViewValue -View $View -Name $Name
    if ($null -ne $value) {
        throw "ramshared_wsl_status: ${Case}: ${Name} must be null, got '$value'"
    }
}

function Assert-Equal {
    param(
        [Parameter(Mandatory = $true)]$View,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)]$Expected,
        [Parameter(Mandatory = $true)][string]$Case
    )
    $value = Get-ViewValue -View $View -Name $Name
    if ($value -ne $Expected) {
        throw "ramshared_wsl_status: ${Case}: ${Name} must be '$Expected', got '$value'"
    }
}

function Assert-AtLeast {
    # The published age is the file's own age plus the reported half, so the
    # reported half is a lower bound and the absolute value is not a constant.
    param(
        [Parameter(Mandatory = $true)]$View,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)]$Minimum,
        [Parameter(Mandatory = $true)][string]$Case
    )
    $value = Get-ViewValue -View $View -Name $Name
    if ($null -eq $value -or [double]$value -lt [double]$Minimum) {
        throw "ramshared_wsl_status: ${Case}: ${Name} must be >= '$Minimum', got '$value'"
    }
}

function Assert-Throws {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Needle,
        [Parameter(Mandatory = $true)][string]$Case
    )
    $threw = $false
    try {
        Invoke-Viewer -Path $Path | Out-Null
    } catch {
        $threw = $true
        if (-not $_.Exception.Message.Contains($Needle)) {
            throw "ramshared_wsl_status: ${Case}: expected error to mention '$Needle', got '$($_.Exception.Message)'"
        }
    }
    if (-not $threw) {
        throw "ramshared_wsl_status: ${Case}: viewer accepted a document it must refuse"
    }
}

# --- Full healthy document: the baseline every negative case degrades. ---
$fullBody = @'
{
  "schema_version": 5,
  "sample_age_ms": 12,
  "protection_state": "READY",
  "protection_reason": "measured_ok",
  "phase": "Armed",
  "activation": {
    "active": true,
    "binary_version": "0.15.0",
    "disk_baseline_kib": 1000,
    "disk_growth_kib": 25
  },
  "mem": { "available_kib": 2048, "swap_free_kib": 0 },
  "control_plane": { "memory_psi_full_avg10": 0.0 },
  "gpu": { "used_mib": 3, "free_mib": 5 },
  "capacity": { "guaranteed_kib": 4096 },
  "tiers": {
    "vram": { "present": true, "used_kib": 0 },
    "disk": { "present": true, "used_kib": 128 }
  }
}
'@

# --- 1. Measured zeros and measured figures all survive. ---
$path = Write-Heartbeat -Name "full" -Body $fullBody
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Equal -View $view -Name "state" -Expected "LIVE" -Case "full_document"
Assert-AtLeast -View $view -Name "sample_age_ms" -Minimum 12 -Case "full_document"
Assert-Equal -View $view -Name "product_active" -Expected $true -Case "full_document"
Assert-Equal -View $view -Name "ram_available_mib" -Expected 2.0 -Case "full_document"
Assert-Equal -View $view -Name "swap_free_mib" -Expected 0.0 -Case "full_document"
Assert-Equal -View $view -Name "memory_psi_full_avg10" -Expected 0.0 -Case "full_document"
Assert-Equal -View $view -Name "vram_swap_used_kib" -Expected 0 -Case "full_document"
Assert-Equal -View $view -Name "disk_swap_used_kib" -Expected 128 -Case "full_document"
Assert-Equal -View $view -Name "protection_state" -Expected "READY" -Case "full_document"

# --- 2. A measured zero is a zero, not an absence. ---
# `swap_free_kib: 0` and a measured-present tier's `used_kib: 0` are the
# positive arm of the same faces the next cases refute: 0 means the figure
# arrived and holds nothing.
$path = Write-Heartbeat -Name "measured-zero" -Body (@'
{
  "schema_version": 5,
  "sample_age_ms": 0,
  "mem": { "available_kib": 0, "swap_free_kib": 0 },
  "control_plane": { "memory_psi_full_avg10": 0.0 },
  "activation": { "active": false },
  "tiers": { "vram": { "present": true, "used_kib": 0 }, "disk": { "present": true, "used_kib": 0 } }
}
'@)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Equal -View $view -Name "ram_available_mib" -Expected 0.0 -Case "measured_zero"
Assert-Equal -View $view -Name "swap_free_mib" -Expected 0.0 -Case "measured_zero"
Assert-Equal -View $view -Name "memory_psi_full_avg10" -Expected 0.0 -Case "measured_zero"
Assert-Equal -View $view -Name "vram_swap_used_kib" -Expected 0 -Case "measured_zero"
Assert-Equal -View $view -Name "disk_swap_used_kib" -Expected 0 -Case "measured_zero"
Assert-Equal -View $view -Name "product_active" -Expected $false -Case "measured_zero"

# --- 2b. An absent tier publishes no hold at all. ---
# `present: false` is not a tier holding nothing — it is no tier — so a
# `used_kib: 0` beside it is a claim the wire never made. This is the fold
# the live product heartbeat still publishes (`tiers.vram.present: false`
# with `used_kib: 0`) and the viewer must not echo it as a measurement.
$path = Write-Heartbeat -Name "tier-absent" -Body (@'
{
  "schema_version": 5,
  "sample_age_ms": 1,
  "tiers": {
    "vram": { "present": false, "used_kib": 0 },
    "disk": { "present": false, "used_kib": 0 }
  }
}
'@)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Null -View $view -Name "vram_swap_used_kib" -Case "tier_absent"
Assert-Null -View $view -Name "disk_swap_used_kib" -Case "tier_absent"

# --- 2c. A present tier whose hold never arrived is not a zero. ---
$path = Write-Heartbeat -Name "tier-present-hold-absent" -Body (@'
{
  "schema_version": 5,
  "sample_age_ms": 1,
  "tiers": { "vram": { "present": true }, "disk": { "present": true, "used_kib": null } }
}
'@)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Null -View $view -Name "vram_swap_used_kib" -Case "tier_present_hold_absent"
Assert-Null -View $view -Name "disk_swap_used_kib" -Case "tier_present_hold_absent"

# --- 2d. A `present` this face cannot witness is never taken. ---
# `"true"` and `1` are not bools; PowerShell's `[bool]"true"` is `$true`, so
# a cast here would invent a present tier out of a token the wire did not
# mean as a bool. Same refusal the daemon-side parser applies.
$path = Write-Heartbeat -Name "tier-present-not-bool" -Body (@'
{
  "schema_version": 5,
  "sample_age_ms": 1,
  "tiers": {
    "vram": { "present": "true", "used_kib": 64 },
    "disk": { "present": 1, "used_kib": 64 }
  }
}
'@)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Null -View $view -Name "vram_swap_used_kib" -Case "tier_present_not_bool"
Assert-Null -View $view -Name "disk_swap_used_kib" -Case "tier_present_not_bool"

# --- 3. A key that never arrived is not a measured zero. ---
$path = Write-Heartbeat -Name "absent-keys" -Body (@'
{
  "schema_version": 5,
  "sample_age_ms": 5,
  "activation": {},
  "tiers": { "vram": {}, "disk": {} }
}
'@)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Null -View $view -Name "ram_available_mib" -Case "absent_keys"
Assert-Null -View $view -Name "swap_free_mib" -Case "absent_keys"
Assert-Null -View $view -Name "memory_psi_full_avg10" -Case "absent_keys"
Assert-Null -View $view -Name "product_active" -Case "absent_keys"
Assert-Null -View $view -Name "vram_swap_used_kib" -Case "absent_keys"
Assert-Null -View $view -Name "disk_swap_used_kib" -Case "absent_keys"
Assert-Null -View $view -Name "gpu_used_mib" -Case "absent_keys"

# --- 4. A present null is also not a measured value. ---
# This is the cast fold: `[double]$null` is 0.0 and `[bool]$null` is $false in
# PowerShell, so a converter that does not guard on $null rebuilds the fold
# one line later than the accessor it fixed.
$path = Write-Heartbeat -Name "null-values" -Body (@'
{
  "schema_version": 5,
  "sample_age_ms": null,
  "mem": { "available_kib": null, "swap_free_kib": null },
  "control_plane": { "memory_psi_full_avg10": null },
  "activation": { "active": null },
  "tiers": { "vram": { "present": true, "used_kib": null }, "disk": { "present": true, "used_kib": null } }
}
'@)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Null -View $view -Name "ram_available_mib" -Case "null_values"
Assert-Null -View $view -Name "swap_free_mib" -Case "null_values"
Assert-Null -View $view -Name "memory_psi_full_avg10" -Case "null_values"
Assert-Null -View $view -Name "product_active" -Case "null_values"
Assert-Null -View $view -Name "vram_swap_used_kib" -Case "null_values"
Assert-Null -View $view -Name "disk_swap_used_kib" -Case "null_values"
Assert-Null -View $view -Name "sample_age_ms" -Case "null_values"

# --- 5. Freshness needs both halves of the age. ---
# A missing `sample_age_ms` is not `0` ms. A fresh file alone cannot claim LIVE.
$path = Write-Heartbeat -Name "age-absent-fresh-file" -Body (@'
{
  "schema_version": 5,
  "mem": { "available_kib": 1024 }
}
'@) -LastWriteUtc ([DateTime]::UtcNow)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Equal -View $view -Name "state" -Expected "UNKNOWN" -Case "age_absent_fresh_file"
Assert-Null -View $view -Name "sample_age_ms" -Case "age_absent_fresh_file"

# --- 6. A stale file is stale even when the reported half never arrived. ---
# File age alone already refutes freshness; that is a measured refutation and
# must not be softened to UNKNOWN.
$path = Write-Heartbeat -Name "age-absent-stale-file" -Body (@'
{
  "schema_version": 5,
  "mem": { "available_kib": 1024 }
}
'@) -LastWriteUtc ([DateTime]::UtcNow.AddSeconds(-30))
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Equal -View $view -Name "state" -Expected "STALE" -Case "age_absent_stale_file"
Assert-Null -View $view -Name "sample_age_ms" -Case "age_absent_stale_file"

# --- 7. A reported age alone decides freshness when both halves are present. ---
$path = Write-Heartbeat -Name "age-reported-stale" -Body (@'
{
  "schema_version": 5,
  "sample_age_ms": 60000
}
'@) -LastWriteUtc ([DateTime]::UtcNow)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Equal -View $view -Name "state" -Expected "STALE" -Case "age_reported_stale"
Assert-AtLeast -View $view -Name "sample_age_ms" -Minimum 60000 -Case "age_reported_stale"

$path = Write-Heartbeat -Name "age-reported-fresh" -Body (@'
{
  "schema_version": 5,
  "sample_age_ms": 40
}
'@) -LastWriteUtc ([DateTime]::UtcNow)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Equal -View $view -Name "state" -Expected "LIVE" -Case "age_reported_fresh"
Assert-AtLeast -View $view -Name "sample_age_ms" -Minimum 40 -Case "age_reported_fresh"

# --- 8. A schema this viewer cannot witness is refused, not guessed. ---
$path = Write-Heartbeat -Name "schema-absent" -Body (@'
{ "mem": { "available_kib": 1024 } }
'@)
Assert-Throws -Path $path -Needle "no schema_version" -Case "schema_absent"
$caseCount++

$path = Write-Heartbeat -Name "schema-unsupported" -Body (@'
{ "schema_version": 6, "mem": { "available_kib": 1024 } }
'@)
Assert-Throws -Path $path -Needle "Unsupported RamShared heartbeat schema: 6" -Case "schema_unsupported"
$caseCount++

# --- 9. A missing whole section is an absent figure, not a zero. ---
$path = Write-Heartbeat -Name "sections-absent" -Body (@'
{ "schema_version": 5, "sample_age_ms": 1 }
'@)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Null -View $view -Name "ram_available_mib" -Case "sections_absent"
Assert-Null -View $view -Name "product_active" -Case "sections_absent"
Assert-Null -View $view -Name "vram_swap_used_kib" -Case "sections_absent"
Assert-Null -View $view -Name "gpu_used_mib" -Case "sections_absent"
Assert-Null -View $view -Name "guaranteed_vram_kib" -Case "sections_absent"

# --- 10. Absence words are for names, never for figures. ---
$path = Write-Heartbeat -Name "absence-words" -Body (@'
{
  "schema_version": 5,
  "sample_age_ms": 1
}
'@)
$view = Invoke-Viewer -Path $path
$caseCount++
Assert-Equal -View $view -Name "protection_state" -Expected "UNKNOWN" -Case "absence_words"
Assert-Equal -View $view -Name "protection_reason" -Expected "unknown" -Case "absence_words"
Assert-Equal -View $view -Name "phase" -Expected "Unknown" -Case "absence_words"
Assert-Equal -View $view -Name "binary_version" -Expected "unknown" -Case "absence_words"

Remove-Item -LiteralPath $workDir -Recurse -Force
Write-Output "PASS ramshared_wsl_status_witness_arms cases=$caseCount"
