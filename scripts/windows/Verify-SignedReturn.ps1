#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Verify-SignedReturn.ps1
<#
.SYNOPSIS
  Verify an organisation-returned signed Windows driver package (AUD-45, AUD-44).

.DESCRIPTION
  Partner Center attestation signing is an organisational step. The package
  that comes back is a different artifact from the one that was submitted.

  SIGNED_RETURN_VERIFIED means two separate things were both proved:
  identity (these are the bytes the organisation returned) and customer-release
  trust (publisher, catalog membership and kernel-policy, and not test-signed).
  Identity alone is not trust; an Authenticode signature alone is the publisher
  question and is not the kernel-policy question (AUD-44).

  This command reports SIGNED_RETURN_VERIFIED and nothing further. The
  READY_FOR_CUSTOMER_INSTALL stage is a live load of the exact returned package
  on a normal Windows with test-signing OFF and HVCI qualification, and is
  proved separately.

.PARAMETER PackageDir
  Directory holding the returned ramshared.sys, ramshared.cat and ramshared.inf.

.PARAMETER ExpectedSysSha256
  Optional SHA-256 of the submitted ramshared.sys. When the caller recorded the
  digest of the exact bytes submitted, the returned bytes must match.

.PARAMETER ExpectedCatSha256
  Optional SHA-256 of the submitted ramshared.cat, same contract as
  ExpectedSysSha256.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$PackageDir,
    [string]$ExpectedSysSha256,
    [string]$ExpectedCatSha256
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "ReleaseStage.ps1")
. (Join-Path $PSScriptRoot "DriverTrust.ps1")

function Stop-RamSharedReturnVerification {
    param([Parameter(Mandatory = $true)][string]$Reason)
    Write-RamSharedOutcomeReport -Scope "customer-release" -Stage "AWAITING_SIGNING" -Detail $Reason
    throw $Reason
}

$resolved = Resolve-Path -LiteralPath $PackageDir -ErrorAction SilentlyContinue
if (-not $resolved) {
    Stop-RamSharedReturnVerification "returned package directory not found: $PackageDir"
}
$root = $resolved.Path

$required = @{
    "ramshared.sys" = $ExpectedSysSha256
    "ramshared.cat" = $ExpectedCatSha256
    "ramshared.inf" = $null
}
foreach ($name in $required.Keys) {
    $path = Join-Path $root $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        Stop-RamSharedReturnVerification "returned package is missing $name"
    }
}

# Identity first: a changed binary under the same name is a different package
# from the one that was attested. Trust is then asked of those exact bytes.
foreach ($name in @("ramshared.sys", "ramshared.cat")) {
    $expected = $required[$name]
    if ([string]::IsNullOrWhiteSpace($expected)) { continue }
    $actual = (Get-FileHash -LiteralPath (Join-Path $root $name) -Algorithm SHA256).Hash
    if ($actual -ne $expected) {
        Stop-RamSharedReturnVerification ("{0} digest {1} does not match the submitted digest {2}" -f $name, $actual, $expected)
    }
}

# Customer-release trust is the conjunction of the distinct kinds. A missing
# kind keeps the package at AWAITING_SIGNING; it is not papered over with the
# publisher signature (AUD-44).
try {
    $proved = Test-RamSharedCustomerReleaseTrust -PackageDir $root
} catch {
    Stop-RamSharedReturnVerification $_.Exception.Message
}

$stageProof = @{
    "BUILT"                  = $true
    "VALIDATED"              = $true
    "AWAITING_SIGNING"       = $true
    "SIGNED_RETURN_VERIFIED" = $true
}
$stage = Resolve-RamSharedReleaseStage -Proved $stageProof
Write-RamSharedOutcomeReport -Scope "customer-release" -Stage $stage -Detail ("trust_kinds={0}" -f ($proved -join ","))
Write-Host "TRUST_KINDS_PROVED=$($proved -join ',')"
Write-Host "DOWNSTREAM_REQUIRED=READY_FOR_CUSTOMER_INSTALL"
Write-Host "Load this exact package on a normal Windows with test-signing OFF and HVCI qualification to reach READY_FOR_CUSTOMER_INSTALL."
exit 0
