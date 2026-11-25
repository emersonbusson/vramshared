#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-RamSharedWslStatusStatic.ps1
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$target = Join-Path $PSScriptRoot "Show-RamSharedWslStatus.ps1"
if (-not (Test-Path -LiteralPath $target -PathType Leaf)) {
    throw "ramshared_wsl_status: target script is missing"
}
$source = Get-Content -Raw -LiteralPath $target
foreach ($required in @(
    'state = "PLAN"',
    "ConvertFrom-Json",
    "LastWriteTimeUtc",
    "sample_age_ms",
    "disk_growth_kib",
    "protection_state",
    "memory_psi_full_avg10",
    "ConvertTo-OptionalMiB",
    "ConvertTo-OptionalDouble",
    "ConvertTo-OptionalBool",
    "Get-TierUsedKib",
    '"UNKNOWN"',
    "refusing to guess one",
    'SupportedHeartbeatSchemaVersion'
)) {
    if (-not $source.Contains($required)) {
        throw "ramshared_wsl_status: missing contract $required"
    }
}
foreach ($forbidden in @(
    "wsl.exe",
    "Start-Process",
    "Invoke-Expression",
    "--shutdown",
    "--terminate",
    "Set-Content",
    "Add-Content"
)) {
    if ($source.Contains($forbidden)) {
        throw "ramshared_wsl_status: forbidden host mutation $forbidden"
    }
}
foreach ($forbidden in @(
    # A measured-figure default is the fold: a key that never arrived is
    # published as the value the viewer chose. Object-container defaults
    # (`-Default ([pscustomobject]@{})`) and named absence words
    # (`-Default "UNKNOWN"`) are not folds and are allowed.
    "-Default 0",
    "-Default 0.0",
    "-Default `$false",
    "-Default `$true",
    # The cast fold one line below the accessor fix: `[double]$null` is 0.0
    # and `[bool]$null` is `$false`, so a bare cast rebuilds the fold the
    # named converter was added to remove.
    "[double](Get-ObservationProperty",
    "[bool](Get-ObservationProperty"
)) {
    if ($source.Contains($forbidden)) {
        throw "ramshared_wsl_status: forbidden measured-value fold $forbidden"
    }
}
# Coupling gate: the viewer's supported schema must be the schema the product
# publishes. The producer of truth is the `render_status_json` emit in
# `crates/ramshared-cli/src/cascade/lifecycle.rs` (SPEC
# `cascade-lifecycle-observability` DT-4). Reading the emit rather than
# restating the number here is what makes this a gate and not a second copy of
# the claim: if either side moves alone, this fails instead of the viewer
# silently refusing the live document again.
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$producer = Join-Path $repoRoot "crates/ramshared-cli/src/cascade/lifecycle.rs"
if (-not (Test-Path -LiteralPath $producer -PathType Leaf)) {
    throw "ramshared_wsl_status: producer emit is missing at $producer"
}
$producerSource = Get-Content -Raw -LiteralPath $producer
$producerMatch = [regex]::Match($producerSource, 'schema_version\\?":(\d+)')
if (-not $producerMatch.Success) {
    throw "ramshared_wsl_status: producer emit does not declare a schema_version"
}
$producerSchema = [int]$producerMatch.Groups[1].Value

$viewerMatch = [regex]::Match($source, '\$SupportedHeartbeatSchemaVersion\s*=\s*(\d+)')
if (-not $viewerMatch.Success) {
    throw "ramshared_wsl_status: viewer does not declare \$SupportedHeartbeatSchemaVersion"
}
$viewerSchema = [int]$viewerMatch.Groups[1].Value

if ($viewerSchema -ne $producerSchema) {
    throw "ramshared_wsl_status: viewer witnesses schema $viewerSchema but the producer publishes $producerSchema"
}
Write-Output "PASS ramshared_wsl_status_schema_matches_producer schema=$viewerSchema"
Write-Output "PASS ramshared_wsl_status_reads_heartbeat_only"
