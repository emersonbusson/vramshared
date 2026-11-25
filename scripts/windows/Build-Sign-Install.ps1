#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Build-Sign-Install.ps1
<#
.SYNOPSIS
  ITEM-11 / RF-8 / RNF-7 - build and validate the Windows driver package.

.DESCRIPTION
  Builds ramshared.sys via MSBuild and runs InfVerif on the shipped INF. The
  highest customer-release stage this command can prove is AWAITING_SIGNING:
  organisational attestation signing and the normal-Windows load are stages an
  operator proves independently with Verify-SignedReturn.ps1 and a live load.

  Stages (scripts/windows/ReleaseStage.ps1):
    BUILT                      MSBuild produced the driver
    VALIDATED                  InfVerif /w passed on the shipped INF
    AWAITING_SIGNING           package prepared for organisational signing
    SIGNED_RETURN_VERIFIED     organisation returned a signed package
    READY_FOR_CUSTOMER_INSTALL exact returned package loaded, test-signing OFF

  A preparation command may succeed only as preparation. Missing InfVerif
  fails customer-release qualification; it is never a warning the run
  continues past.

.PARAMETER Configuration
  MSBuild configuration (default Release).

.PARAMETER SkipSign
  Lab contract: build (and validate when InfVerif is present) for lab
  test-signing only. Reports LAB_PREPARED / LAB_VALIDATED and is never a
  customer-release qualification.
#>
[CmdletBinding()]
param(
    [ValidateSet("Release", "Debug")]
    [string]$Configuration = "Release",
    [string]$Project = "..\..\drivers\windows\ramshared\ramshared.vcxproj",
    [switch]$SkipSign,
    [string]$ArtifactDir = ".\artifacts\build-sign"
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "ReleaseStage.ps1")

$scope = Get-RamSharedOutcomeScope -SkipSign:$SkipSign
$proved = @{}

New-Item -ItemType Directory -Force -Path $ArtifactDir | Out-Null

$proj = Resolve-Path $Project -ErrorAction SilentlyContinue
if (-not $proj) {
    Write-Host "ERROR: project not found: $Project"
    Write-RamSharedOutcomeReport -Scope $scope -Stage "NOT_BUILT" -Detail "project missing"
    exit 2
}

Write-Host "MSBuild $proj ($Configuration|x64)"
msbuild $proj /p:Configuration=$Configuration /p:Platform=x64 /p:TreatWarningAsError=true /v:m
if ($LASTEXITCODE -ne 0) {
    Write-RamSharedOutcomeReport -Scope $scope -Stage "NOT_BUILT" -Detail "msbuild failed"
    exit $LASTEXITCODE
}
$proved["BUILT"] = $true

$inf = Join-Path (Split-Path $proj) "ramshared.inf"
if (-not (Test-Path -LiteralPath $inf -PathType Leaf)) {
    Write-Host "ERROR: shipped INF not found: $inf"
    Write-RamSharedOutcomeReport -Scope $scope -Stage "BUILT" -Detail "inf missing"
    exit 4
}

if (-not (Test-RamSharedReleaseTool -Tool "InfVerif.exe")) {
    if ($SkipSign) {
        Write-Warning "InfVerif.exe not on PATH - lab build only; customer-release qualification is blocked"
    } else {
        Write-Host "ERROR: InfVerif.exe not on PATH - customer-release qualification requires InfVerif (install WDK)"
        Write-RamSharedOutcomeReport -Scope $scope -Stage "BUILT" -Detail "InfVerif missing"
        exit 3
    }
} else {
    Write-Host "InfVerif /w $inf"
    InfVerif.exe /w $inf
    if ($LASTEXITCODE -ne 0) {
        Write-RamSharedOutcomeReport -Scope $scope -Stage "BUILT" -Detail "InfVerif failed"
        exit $LASTEXITCODE
    }
    $proved["VALIDATED"] = $true
}

if ($SkipSign) {
    $labStage = "LAB_PREPARED"
    if ($proved["VALIDATED"]) { $labStage = "LAB_VALIDATED" }
    Write-RamSharedOutcomeReport -Scope $scope -Stage $labStage
    Write-Host "Lab contract: ready for lab test-signing only. This is not a customer-release qualification."
    exit 0
}

$proved["AWAITING_SIGNING"] = $true
$stage = Resolve-RamSharedReleaseStage -Proved $proved
Write-RamSharedOutcomeReport -Scope $scope -Stage $stage

Write-Host @"
Attestation path (R9 - Partner Center, organizational):
  1. signtool sign /v /fd sha256 /tr http://timestamp.digicert.com /td sha256 ramshared.sys
  2. Submit package via Partner Center hardware dashboard (attestation).
  3. When the signed package returns, verify it: scripts\windows\Verify-SignedReturn.ps1
  4. Install the exact returned package on 26200.8655 with test-signing OFF; confirm load (RNF-7).
  5. Record in IMPL.md ITEM-11 evidence.

Abort if driver does not load on stable build and WHCP cost not justified (PRD #2a).
"@

Write-Host "DOWNSTREAM_REQUIRED=SIGNED_RETURN_VERIFIED,READY_FOR_CUSTOMER_INSTALL"
Write-Host "This command prepared the package. It did not sign it and it did not load it."
exit 0
