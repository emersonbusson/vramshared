#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-WindowsDriverTrustStatic.ps1
<#
.SYNOPSIS
  Windows driver trust-kind contracts (AUD-44).

.DESCRIPTION
  Application publisher trust, catalog membership, kernel-policy trust and
  test-signing are four different questions. A gate that answers the publisher
  question and reports a customer-release trust is the defect this suite pins.

  Named tests:
    driver_catalog_membership_and_kernel_policy_required
    test_signed_driver_cannot_enter_customer_release
    application_publisher_refusal_legitimate_pair
    normal_windows_secure_boot_hvci_lifecycle

  normal_windows_secure_boot_hvci_lifecycle asserts the contract: the live
  load is a separate stage that a signature check never reaches. Executing
  that load on a real normal Windows stays environment-bound and is not
  claimed here.
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
$trustPath = Join-Path $windowsDir "DriverTrust.ps1"
if (-not (Test-Path -LiteralPath $trustPath -PathType Leaf)) {
    throw "driver_catalog_membership_and_kernel_policy_required: DriverTrust.ps1 is missing"
}
$trust = Get-Content -LiteralPath $trustPath -Raw
$verifyPath = Join-Path $windowsDir "Verify-SignedReturn.ps1"
if (-not (Test-Path -LiteralPath $verifyPath -PathType Leaf)) {
    throw "driver_catalog_membership_and_kernel_policy_required: Verify-SignedReturn.ps1 is missing"
}
$verify = Get-Content -LiteralPath $verifyPath -Raw
$releaseStage = Get-Content -LiteralPath (Join-Path $windowsDir "ReleaseStage.ps1") -Raw

. (Join-Path $windowsDir "DriverTrust.ps1")
. (Join-Path $windowsDir "ReleaseStage.ps1")

# A comment that names a retired pattern in order to forbid it is not a
# violation; a live `/pa` standing in for `/kp` is. Strip both comment forms.
$trustCode = $trust -replace '(?s)<#.*?#>', ''
$trustCode = ($trustCode -split '\r?\n' | ForEach-Object { $_ -replace '#.*$', '' }) -join "`n"

# --- driver_catalog_membership_and_kernel_policy_required --------------------
# Kernel-policy is a SignTool /kp question. It is not answered by
# Get-AuthenticodeSignature and it is not answered by /pa. Catalog membership
# rides with it: three signed files can be three unrelated objects.

$kinds = Get-RamSharedTrustKinds
foreach ($required in @("application-publisher", "catalog-membership", "kernel-policy", "not-test-signed")) {
    if ($kinds -notcontains $required) {
        throw "driver_catalog_membership_and_kernel_policy_required: trust kind '$required' is not named"
    }
}

if ($trustCode -notmatch 'verify\s+/kp') {
    throw "driver_catalog_membership_and_kernel_policy_required: kernel policy is not checked with SignTool verify /kp"
}
if ($trustCode -match 'verify\s+/pa') {
    throw "driver_catalog_membership_and_kernel_policy_required: /pa is the publisher question and must not stand in for /kp"
}
if ($trust -notmatch 'SignTool\.exe') {
    throw "driver_catalog_membership_and_kernel_policy_required: kernel policy is not a SignTool question"
}
if ($trust -notmatch 'SignTool\.exe is not on PATH') {
    throw "driver_catalog_membership_and_kernel_policy_required: a missing SignTool must be a refusal, not a pass"
}

# The consumer must require the conjunction, not the publisher signature alone.
if ($verify -notmatch 'Test-RamSharedCustomerReleaseTrust') {
    throw "driver_catalog_membership_and_kernel_policy_required: the returned package is not asked for customer-release trust"
}
if ($verify -match 'Get-AuthenticodeSignature') {
    throw "driver_catalog_membership_and_kernel_policy_required: the verifier must not re-ask the publisher question in place of trust"
}

# Asserting only one kind must not be accepted as customer-release trust.
$trustProbeThrew = $false
try {
    Test-RamSharedCustomerReleaseTrust -PackageDir (Join-Path $windowsDir "does-not-exist") | Out-Null
} catch {
    $trustProbeThrew = $true
}
if (-not $trustProbeThrew) {
    throw "driver_catalog_membership_and_kernel_policy_required: a missing package is not a refusal"
}
Write-Output "PASS driver_catalog_membership_and_kernel_policy_required"

# --- test_signed_driver_cannot_enter_customer_release ------------------------
# A self-signed certificate is its own issuer. That is the shape of a
# test-signing certificate, and a test-signed driver never enters a customer
# release whatever its Authenticode status says.

if ($trustCode -notmatch '\$cert\.Subject\s*-ne\s*\$cert\.Issuer') {
    throw "test_signed_driver_cannot_enter_customer_release: a self-signed signer is not detected"
}
if ($trust -notmatch 'not-test-signed') {
    throw "test_signed_driver_cannot_enter_customer_release: the test-signing kind is not named"
}
if ($trust -notmatch 'Test-RamSharedMachineTestSigningOff') {
    throw "test_signed_driver_cannot_enter_customer_release: the machine test-signing state is not consulted"
}
# The pattern is written as a regex inside the product source, so it is a
# literal here rather than a regex that would look for real whitespace.
if (-not $trustCode.Contains('testsigning\s+Yes')) {
    throw "test_signed_driver_cannot_enter_customer_release: the machine test-signing bit is not read"
}

# The absence of a signer is a refusal, not a pass.
$selfSignedProbeThrew = $false
try {
    Test-RamSharedNotSelfSigned -Path (Join-Path $windowsDir "does-not-exist") | Out-Null
} catch {
    $selfSignedProbeThrew = $true
}
if (-not $selfSignedProbeThrew) {
    throw "test_signed_driver_cannot_enter_customer_release: a missing file is not a refusal"
}
Write-Output "PASS test_signed_driver_cannot_enter_customer_release"

# --- application_publisher_refusal_legitimate_pair ---------------------------
# The publisher question is answered by Get-AuthenticodeSignature and by
# nothing else. An unsigned or invalidly signed file is refused there; a
# valid publisher signature is necessary and never sufficient.

if ($trust -notmatch 'Get-AuthenticodeSignature') {
    throw "application_publisher_refusal_legitimate_pair: the publisher question is not asked"
}
if ($trust -notmatch 'SignatureStatus\]::Valid') {
    throw "application_publisher_refusal_legitimate_pair: a publisher signature is not required to be Valid"
}
if ($trust -notmatch 'application-publisher') {
    throw "application_publisher_refusal_legitimate_pair: the publisher kind is not named"
}

$publisherProbeThrew = $false
try {
    Test-RamSharedApplicationPublisherSignature -Path (Join-Path $windowsDir "does-not-exist") | Out-Null
} catch {
    $publisherProbeThrew = $true
}
if (-not $publisherProbeThrew) {
    throw "application_publisher_refusal_legitimate_pair: a missing file is not a refusal"
}

# The kinds stay distinct: nothing reports a single collapsed "signed" verdict.
foreach ($kind in (Get-RamSharedTrustKinds)) {
    if ($verify -match ("RELEASE_TRUST=" + [regex]::Escape($kind))) {
        throw "application_publisher_refusal_legitimate_pair: '$kind' is reported as a collapsed verdict"
    }
}
if ($verify -match 'TRUST_KINDS_PROVED') {
    # The verifier enumerates the kinds it proved; it does not collapse them.
} else {
    throw "application_publisher_refusal_legitimate_pair: the verifier does not enumerate the kinds it proved"
}
Write-Output "PASS application_publisher_refusal_legitimate_pair"

# --- normal_windows_secure_boot_hvci_lifecycle -------------------------------
# The live load is a separate stage. A signature check never reaches it.
# Asserting the contract here; the execution is environment-bound.

if ($releaseStage -notmatch 'READY_FOR_CUSTOMER_INSTALL') {
    throw "normal_windows_secure_boot_hvci_lifecycle: the live-load stage is not named"
}
if ($releaseStage -notmatch 'test-signing OFF') {
    throw "normal_windows_secure_boot_hvci_lifecycle: the live-load stage does not require test-signing OFF"
}
if ($verify -notmatch 'HVCI') {
    throw "normal_windows_secure_boot_hvci_lifecycle: HVCI qualification is not named on the downstream requirement"
}
if ($verify -notmatch 'DOWNSTREAM_REQUIRED=READY_FOR_CUSTOMER_INSTALL') {
    throw "normal_windows_secure_boot_hvci_lifecycle: the verifier does not name the live-load stage as downstream"
}

# A signature check must never itself claim READY_FOR_CUSTOMER_INSTALL.
$signatureStageClaim = $verify -match 'Resolve-RamSharedReleaseStage[\s\S]{0,400}READY_FOR_CUSTOMER_INSTALL\s*=\s*\$true'
if ($signatureStageClaim) {
    throw "normal_windows_secure_boot_hvci_lifecycle: a signature check claims the live-load stage"
}
$provedLadder = @{}
foreach ($name in @("BUILT", "VALIDATED", "AWAITING_SIGNING", "SIGNED_RETURN_VERIFIED")) {
    $provedLadder[$name] = $true
}
$resolvedWithoutLoad = Resolve-RamSharedReleaseStage -Proved $provedLadder
if ($resolvedWithoutLoad -ne "SIGNED_RETURN_VERIFIED") {
    throw ("normal_windows_secure_boot_hvci_lifecycle: proof without a live load resolved to '{0}', not SIGNED_RETURN_VERIFIED" -f $resolvedWithoutLoad)
}
if (Test-RamSharedReleaseQualified -Scope "customer-release" -Stage $resolvedWithoutLoad) {
    throw "normal_windows_secure_boot_hvci_lifecycle: SIGNED_RETURN_VERIFIED is reported as a customer-release qualification"
}
Write-Output "PASS normal_windows_secure_boot_hvci_lifecycle"

Write-Output "PASS Test-WindowsDriverTrustStatic"
