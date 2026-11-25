#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/ReleaseStage.ps1
<#
.SYNOPSIS
  Windows driver release-stage ladder (AUD-45).

.DESCRIPTION
  One ladder, one writer. A preparation command reports the highest stage it
  actually proved. It never reports a stage that needs an organisational
  signing return or a live normal-Windows load.

  Customer-release ladder (cumulative; a missing fact stops the ascent):
    BUILT                      MSBuild produced the driver
    VALIDATED                  InfVerif /w passed on the shipped INF
    AWAITING_SIGNING           package prepared for organisational signing
    SIGNED_RETURN_VERIFIED     organisation returned a signed package whose
                               identity was verified
    READY_FOR_CUSTOMER_INSTALL the exact returned package loaded on a normal
                               Windows with test-signing OFF

  Lab test-signing is a separate tool contract and is not on this ladder.
  Its outcomes are LAB_PREPARED and LAB_SIGNED. Neither is a customer-release
  qualification.

  RELEASE_QUALIFIED is "yes" only for scope customer-release at stage
  READY_FOR_CUSTOMER_INSTALL. Every other combination reports "no".
#>

$script:RamSharedReleaseLadder = @(
    "BUILT",
    "VALIDATED",
    "AWAITING_SIGNING",
    "SIGNED_RETURN_VERIFIED",
    "READY_FOR_CUSTOMER_INSTALL"
)

$script:RamSharedLabOutcomes = @(
    "NOT_BUILT",
    "LAB_PREPARED",
    "LAB_VALIDATED",
    "LAB_SIGNED"
)

function Get-RamSharedReleaseLadder {
    return @($script:RamSharedReleaseLadder)
}

function Get-RamSharedLabOutcomes {
    return @($script:RamSharedLabOutcomes)
}

function Get-RamSharedOutcomeScope {
    param([switch]$SkipSign)
    if ($SkipSign) { return "lab-test-signing" }
    return "customer-release"
}

<#
Returns the highest customer-release stage the caller proved. The ladder is
cumulative: a stage is reachable only when every stage below it is also
present. Reporting READY_FOR_CUSTOMER_INSTALL without VALIDATED is a claim
this function does not make.
#>
function Resolve-RamSharedReleaseStage {
    param([hashtable]$Proved = @{})
    $stage = "NOT_BUILT"
    foreach ($candidate in $script:RamSharedReleaseLadder) {
        if (-not $Proved[$candidate]) { break }
        $stage = $candidate
    }
    return $stage
}

function Test-RamSharedReleaseQualified {
    param(
        [Parameter(Mandatory = $true)][string]$Scope,
        [Parameter(Mandatory = $true)][string]$Stage
    )
    return ($Scope -eq "customer-release" -and $Stage -eq "READY_FOR_CUSTOMER_INSTALL")
}

<#
Emits the machine-readable outcome. Every release helper writes through this
so a reader never has to infer delivery from prose.
#>
function Write-RamSharedOutcomeReport {
    param(
        [Parameter(Mandatory = $true)][string]$Scope,
        [Parameter(Mandatory = $true)][string]$Stage,
        [string]$Detail = ""
    )
    if ($Scope -ne "customer-release" -and $Scope -ne "lab-test-signing") {
        throw "unknown release outcome scope '$Scope'"
    }
    $known = @($script:RamSharedReleaseLadder) + @($script:RamSharedLabOutcomes)
    if ($known -notcontains $Stage) {
        throw "unknown release outcome stage '$Stage'"
    }
    if ($Scope -eq "lab-test-signing" -and
        $script:RamSharedReleaseLadder -contains $Stage) {
        throw "lab-test-signing must not report customer-release stage '$Stage'"
    }
    if ($Scope -eq "customer-release" -and
        $script:RamSharedLabOutcomes -contains $Stage -and
        $Stage -ne "NOT_BUILT") {
        throw "customer-release must not report lab outcome '$Stage'"
    }
    $qualified = Test-RamSharedReleaseQualified -Scope $Scope -Stage $Stage
    Write-Output "RELEASE_SCOPE=$Scope"
    Write-Output "RELEASE_STAGE=$Stage"
    Write-Output "RELEASE_QUALIFIED=$(if ($qualified) { 'yes' } else { 'no' })"
    if (-not [string]::IsNullOrWhiteSpace($Detail)) {
        Write-Output "RELEASE_DETAIL=$Detail"
    }
}

function Test-RamSharedReleaseTool {
    param([Parameter(Mandatory = $true)][string]$Tool)
    return [bool](Get-Command $Tool -ErrorAction SilentlyContinue)
}

<#
Fail-closed tooling gate for a customer-release qualification. A missing
mandatory tool is a failure of the qualification, never a warning that the
run continues past.
#>
function Assert-RamSharedCustomerReleaseTool {
    param(
        [Parameter(Mandatory = $true)][string]$Tool,
        [Parameter(Mandatory = $true)][string]$Reason
    )
    if (Test-RamSharedReleaseTool -Tool $Tool) { return }
    throw "$Tool is not on PATH - $Reason"
}
