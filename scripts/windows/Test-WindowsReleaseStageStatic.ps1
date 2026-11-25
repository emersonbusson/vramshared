#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-WindowsReleaseStageStatic.ps1
<#
.SYNOPSIS
  Windows driver release-stage contracts (AUD-45).

.DESCRIPTION
  A preparation command that prints organisational signing steps has not
  delivered a package. A missing mandatory tool or catalog is a failure of the
  qualification it claims, not a warning the run continues past. Lab
  test-signing is a separate tool contract and is never a customer-release
  qualification.

  Named tests:
    missing_infverif_blocks_release
    missing_catalog_blocks_release
    manual_signing_instructions_do_not_qualify_package
    returned_signed_package_identity_verified
    release_ladder_is_cumulative
    lab_contract_is_not_a_release_qualification

  The real normal-Windows load of the exact returned package stays
  environment-bound and is not claimed here. The trust-kind separation
  (application publisher vs catalog membership vs kernel policy vs
  test-signing) is a separate contract, asserted in
  Test-WindowsDriverTrustStatic.ps1.
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
$buildSign = Get-Content -LiteralPath (Join-Path $windowsDir "Build-Sign-Install.ps1") -Raw
$signDrivers = Get-Content -LiteralPath (Join-Path $windowsDir "Sign-Drivers.ps1") -Raw
$verifyPath = Join-Path $windowsDir "Verify-SignedReturn.ps1"
if (-not (Test-Path -LiteralPath $verifyPath -PathType Leaf)) {
    throw "returned_signed_package_identity_verified: Verify-SignedReturn.ps1 is missing"
}
$verify = Get-Content -LiteralPath $verifyPath -Raw

. (Join-Path $windowsDir "ReleaseStage.ps1")

# --- missing_infverif_blocks_release -----------------------------------------
# The predicate must fail closed. InfVerif absence is a qualification failure
# on the customer-release path; the only warning allowed is the lab one.

$probeThrew = $false
try {
    Assert-RamSharedCustomerReleaseTool -Tool "RamShared-Absent-Probe-Tool.exe" -Reason "probe" | Out-Null
} catch {
    $probeThrew = $true
}
if (-not $probeThrew) {
    throw "missing_infverif_blocks_release: a missing mandatory tool does not fail closed"
}

if ($buildSign -match 'Write-Warning[^\n]*InfVerif[^\n]*- skip') {
    throw "missing_infverif_blocks_release: InfVerif absence is a warning-and-continue"
}
if ($buildSign -notmatch 'Test-RamSharedReleaseTool\s+-Tool\s+"InfVerif\.exe"') {
    throw "missing_infverif_blocks_release: InfVerif is not consulted as a release tool"
}
if ($buildSign -notmatch 'Detail "InfVerif missing"') {
    throw "missing_infverif_blocks_release: the refusal does not name the missing InfVerif"
}
$infverifRefusal = $buildSign -match 'Detail "InfVerif missing"\s*\r?\n\s*exit\s+(\d+)'
if (-not $infverifRefusal) {
    throw "missing_infverif_blocks_release: the missing-InfVerif path does not exit explicitly"
}
if ([int]$Matches[1] -eq 0) {
    throw "missing_infverif_blocks_release: the missing-InfVerif path exits zero"
}
if ($buildSign -notmatch '\$ErrorActionPreference\s*=\s*"Stop"') {
    throw "missing_infverif_blocks_release: the script does not terminate on a thrown refusal"
}
Write-Output "PASS missing_infverif_blocks_release"

# --- missing_catalog_blocks_release ------------------------------------------
# A lab package without a signed catalog is not a package pnputil accepts.
# SIGN_OK is the lab success token and is reachable only after that catalog.

if ($signDrivers -match 'Write-Warning[^\n]*Inf2Cat') {
    throw "missing_catalog_blocks_release: Inf2Cat absence is only a warning"
}
if ($signDrivers -match 'Write-Warning[^\n]*ramshared\.cat') {
    throw "missing_catalog_blocks_release: catalog absence is only a warning"
}
if ($signDrivers -notmatch 'throw "Inf2Cat\.exe not found') {
    throw "missing_catalog_blocks_release: Inf2Cat absence does not fail the lab package contract"
}
if ($signDrivers -notmatch 'throw "ramshared\.cat not produced') {
    throw "missing_catalog_blocks_release: catalog absence does not fail the lab package contract"
}
$signOkMatch = [regex]::Match($signDrivers, 'Write-Host\s+"SIGN_OK')
$catOkMatch = [regex]::Match($signDrivers, 'Write-Host\s+"CAT_OK')
if (-not $signOkMatch.Success -or -not $catOkMatch.Success) {
    throw "missing_catalog_blocks_release: SIGN_OK/CAT_OK emission points are missing"
}
if ($catOkMatch.Index -gt $signOkMatch.Index) {
    throw "missing_catalog_blocks_release: SIGN_OK is emitted before the signed catalog"
}
$catalogSign = [regex]::Match($signDrivers, 'Invoke-SignTool\s+-Path\s+\$cat')
if (-not $catalogSign.Success -or $catalogSign.Index -gt $signOkMatch.Index) {
    throw "missing_catalog_blocks_release: SIGN_OK is not gated on a signed catalog"
}
Write-Output "PASS missing_catalog_blocks_release"

# --- manual_signing_instructions_do_not_qualify_package ----------------------
# Printing organisational signing steps is preparation. The ladder must not
# hand a preparation run the downstream stages.

$preparation = Resolve-RamSharedReleaseStage -Proved @{
    "BUILT"        = $true
    "VALIDATED"    = $true
    "AWAITING_SIGNING" = $true
}
if ($preparation -ne "AWAITING_SIGNING") {
    throw "manual_signing_instructions_do_not_qualify_package: preparation resolves to $preparation"
}
if ($preparation -eq "SIGNED_RETURN_VERIFIED" -or $preparation -eq "READY_FOR_CUSTOMER_INSTALL") {
    throw "manual_signing_instructions_do_not_qualify_package: preparation reaches a downstream stage"
}
if (Test-RamSharedReleaseQualified -Scope "customer-release" -Stage $preparation) {
    throw "manual_signing_instructions_do_not_qualify_package: AWAITING_SIGNING is release-qualified"
}

if ($buildSign -notmatch 'AWAITING_SIGNING') {
    throw "manual_signing_instructions_do_not_qualify_package: the preparation stage is never reported"
}
if ($buildSign -match 'Write-RamSharedOutcomeReport[^\n]*"SIGNED_RETURN_VERIFIED"') {
    throw "manual_signing_instructions_do_not_qualify_package: the preparation command claims a signing return"
}
if ($buildSign -match 'Write-RamSharedOutcomeReport[^\n]*"READY_FOR_CUSTOMER_INSTALL"') {
    throw "manual_signing_instructions_do_not_qualify_package: the preparation command claims a customer install"
}
if ($buildSign -match 'Write-Host\s+"SIGN_OK') {
    throw "manual_signing_instructions_do_not_qualify_package: the preparation command emits SIGN_OK"
}
if ($buildSign -match '(?i)package ready for customer') {
    throw "manual_signing_instructions_do_not_qualify_package: delivery language without a load"
}
if ($buildSign -notmatch 'DOWNSTREAM_REQUIRED=SIGNED_RETURN_VERIFIED,READY_FOR_CUSTOMER_INSTALL') {
    throw "manual_signing_instructions_do_not_qualify_package: downstream stages are not named as required"
}
$instruction = [regex]::Match($buildSign, 'Attestation path')
$downstream = [regex]::Match($buildSign, 'DOWNSTREAM_REQUIRED=')
if (-not $instruction.Success -or -not $downstream.Success -or $downstream.Index -lt $instruction.Index) {
    throw "manual_signing_instructions_do_not_qualify_package: the signing steps are not followed by the downstream requirement"
}
Write-Output "PASS manual_signing_instructions_do_not_qualify_package"

# --- returned_signed_package_identity_verified -------------------------------
# The returned package is a different artifact. This test is about identity:
# the exact bytes the organisation returned are the ones verified, and the
# stage is claimed only after that check. The trust question is a separate
# contract (AUD-44) owned by DriverTrust.ps1 and asserted in
# Test-WindowsDriverTrustStatic.ps1; folding it in here is how a publisher
# signature came to stand in for kernel trust.

foreach ($token in @('"ramshared.sys"', '"ramshared.cat"', 'Get-FileHash')) {
    if ($verify -notmatch [regex]::Escape($token)) {
        throw "returned_signed_package_identity_verified: $token is not checked"
    }
}
if ($verify -notmatch 'Get-FileHash') {
    throw "returned_signed_package_identity_verified: the returned bytes are not hashed"
}
if ($verify -notmatch 'does not match the submitted digest') {
    throw "returned_signed_package_identity_verified: a digest mismatch is not a refusal"
}
if ($verify -notmatch 'Test-RamSharedCustomerReleaseTrust') {
    throw "returned_signed_package_identity_verified: identity is not followed by the separate trust gate"
}
$stageClaim = [regex]::Match($verify, '"SIGNED_RETURN_VERIFIED"\s*=\s*\$true')
if (-not $stageClaim.Success) {
    throw "returned_signed_package_identity_verified: the stage is not claimed through the ladder"
}
$digestCheck = $verify.IndexOf('does not match the submitted digest')
if ($digestCheck -lt 0 -or $stageClaim.Index -lt $digestCheck) {
    throw "returned_signed_package_identity_verified: the stage is claimed before the identity check"
}
if ($buildSign -match 'Write-RamSharedOutcomeReport[^\n]*"SIGNED_RETURN_VERIFIED"') {
    throw "returned_signed_package_identity_verified: the preparation command claims the signing return"
}

$withoutReturn = Resolve-RamSharedReleaseStage -Proved @{
    "BUILT"            = $true
    "VALIDATED"        = $true
    "AWAITING_SIGNING" = $true
}
if ($withoutReturn -ne "AWAITING_SIGNING") {
    throw "returned_signed_package_identity_verified: a prepared package resolves to $withoutReturn"
}
$returned = Resolve-RamSharedReleaseStage -Proved @{
    "BUILT"                  = $true
    "VALIDATED"              = $true
    "AWAITING_SIGNING"       = $true
    "SIGNED_RETURN_VERIFIED" = $true
}
if ($returned -ne "SIGNED_RETURN_VERIFIED") {
    throw "returned_signed_package_identity_verified: a verified return resolves to $returned"
}
if (Test-RamSharedReleaseQualified -Scope "customer-release" -Stage $returned) {
    throw "returned_signed_package_identity_verified: a signing return alone is release-qualified"
}
$loaded = Resolve-RamSharedReleaseStage -Proved @{
    "BUILT"                      = $true
    "VALIDATED"                  = $true
    "AWAITING_SIGNING"           = $true
    "SIGNED_RETURN_VERIFIED"     = $true
    "READY_FOR_CUSTOMER_INSTALL" = $true
}
if (-not (Test-RamSharedReleaseQualified -Scope "customer-release" -Stage $loaded)) {
    throw "returned_signed_package_identity_verified: a loaded return is not release-qualified"
}
Write-Output "PASS returned_signed_package_identity_verified"

# --- release_ladder_is_cumulative --------------------------------------------
# A stage is reachable only when every stage below it is present.

$skipValidation = Resolve-RamSharedReleaseStage -Proved @{
    "BUILT"                      = $true
    "READY_FOR_CUSTOMER_INSTALL" = $true
}
if ($skipValidation -ne "BUILT") {
    throw "release_ladder_is_cumulative: a later stage is granted without VALIDATED"
}
$ladder = Get-RamSharedReleaseLadder
if ($ladder.Count -ne 5 -or $ladder[0] -ne "BUILT" -or $ladder[4] -ne "READY_FOR_CUSTOMER_INSTALL") {
    throw "release_ladder_is_cumulative: the ladder order is not the release contract"
}
foreach ($stage in $ladder) {
    $step = Resolve-RamSharedReleaseStage -Proved @{ $stage = $true }
    if ($step -ne "NOT_BUILT" -and $ladder.IndexOf($step) -gt $ladder.IndexOf($stage)) {
        throw "release_ladder_is_cumulative: $stage ascends past itself"
    }
}
Write-Output "PASS release_ladder_is_cumulative"

# --- lab_contract_is_not_a_release_qualification -----------------------------
# Lab test-signing is an explicitly separate tool contract.

if (Test-RamSharedReleaseQualified -Scope "lab-test-signing" -Stage "LAB_SIGNED") {
    throw "lab_contract_is_not_a_release_qualification: lab signing is release-qualified"
}
if (Test-RamSharedReleaseQualified -Scope "customer-release" -Stage "AWAITING_SIGNING") {
    throw "lab_contract_is_not_a_release_qualification: preparation is release-qualified"
}
if ($buildSign -notmatch 'Get-RamSharedOutcomeScope\s+-SkipSign:\$SkipSign') {
    throw "lab_contract_is_not_a_release_qualification: -SkipSign does not select the lab scope"
}
# The scope literal lives once, in the shared ladder. The caller obtains it
# through the shared function rather than restating it.
$releaseStageSource = Get-Content -LiteralPath (Join-Path $windowsDir "ReleaseStage.ps1") -Raw
if ($releaseStageSource -notmatch '"lab-test-signing"') {
    throw "lab_contract_is_not_a_release_qualification: the lab scope is not defined in the shared ladder"
}
if ($buildSign -notmatch '\$labStage') {
    throw "lab_contract_is_not_a_release_qualification: the lab path does not name its own outcome"
}
if ($buildSign -notmatch 'Write-RamSharedOutcomeReport -Scope \$scope -Stage \$labStage') {
    throw "lab_contract_is_not_a_release_qualification: the lab outcome is not reported through the shared writer"
}
if ($signDrivers -notmatch 'Write-RamSharedOutcomeReport -Scope "lab-test-signing"') {
    throw "lab_contract_is_not_a_release_qualification: lab signing does not report the lab scope"
}
if ($signDrivers -match 'Write-RamSharedOutcomeReport -Scope "customer-release"') {
    throw "lab_contract_is_not_a_release_qualification: lab signing claims a customer-release outcome"
}
$labClaimThrew = $false
try {
    Write-RamSharedOutcomeReport -Scope "lab-test-signing" -Stage "READY_FOR_CUSTOMER_INSTALL" | Out-Null
} catch {
    $labClaimThrew = $true
}
if (-not $labClaimThrew) {
    throw "lab_contract_is_not_a_release_qualification: a lab scope may claim a customer-release stage"
}
Write-Output "PASS lab_contract_is_not_a_release_qualification"

Write-Output "PASS Test-WindowsReleaseStageStatic"
