#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/DriverTrust.ps1
<#
.SYNOPSIS
  Windows driver trust kinds (AUD-44).

.DESCRIPTION
  Application publisher trust, catalog membership, kernel-policy trust and
  test-signing are four different questions. A package that answers one has not
  answered the others.

  - application-publisher: the bytes carry an Authenticode signature from a
    certificate a Windows publisher trust accepts. This is the question
    Get-AuthenticodeSignature answers. It is not a kernel question.
  - catalog-membership: the catalog actually lists the exact driver and INF
    that were shipped. Three signed files can be three unrelated objects.
  - kernel-policy: SignTool verify /kp on the exact catalog/driver pair.
    SignTool verify /pa answers the publisher question and cannot substitute.
  - not-test-signed: the signer is not a self-signed or test certificate and
    the machine is not in test-signing mode. A test-signed driver never
    enters a customer release.

  Every consumer names the kinds it proved. Nothing here collapses them into a
  single "signed" verdict.
#>

$script:RamSharedTrustKinds = @(
    "application-publisher",
    "catalog-membership",
    "kernel-policy",
    "not-test-signed"
)

function Get-RamSharedTrustKinds {
    return @($script:RamSharedTrustKinds)
}

function Test-RamSharedApplicationPublisherSignature {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "cannot read publisher signature: file not found: $Path"
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    return ($signature.Status -eq [System.Management.Automation.SignatureStatus]::Valid)
}

<#
A self-signed or ad-hoc certificate is its own issuer. That is the shape of a
test-signing certificate, and a test-signed driver is not a customer-release
candidate whatever its Authenticode status says.
#>
function Test-RamSharedNotSelfSigned {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "cannot read signer certificate: file not found: $Path"
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    $cert = $signature.SignerCertificate
    if ($null -eq $cert) {
        return $false
    }
    return ($cert.Subject -ne $cert.Issuer)
}

<#
Kernel-policy verification is a SignTool question. It is not answered by
Get-AuthenticodeSignature and it is not answered by /pa. When SignTool is
absent the kind stays unproved; the caller is told so rather than being handed
a substitute.
#>
function Test-RamSharedKernelPolicySignature {
    param(
        [Parameter(Mandatory = $true)][string]$Catalog,
        [Parameter(Mandatory = $true)][string]$Driver
    )
    foreach ($path in @($Catalog, $Driver)) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "cannot verify kernel policy: file not found: $path"
        }
    }
    $signTool = Get-Command "SignTool.exe" -ErrorAction SilentlyContinue
    if (-not $signTool) {
        throw "SignTool.exe is not on PATH - kernel-policy verification cannot be claimed"
    }
    & $signTool.Source verify /kp /v /c $Catalog $Driver 2>&1 | Out-Null
    return ($LASTEXITCODE -eq 0)
}

<#
True when the machine is configured to accept test-signed drivers. A customer
release is never qualified on such a machine.
#>
function Test-RamSharedMachineTestSigningOff {
    $bcdedit = Get-Command "bcdedit.exe" -ErrorAction SilentlyContinue
    if (-not $bcdedit) {
        throw "bcdedit.exe is not on PATH - the test-signing state cannot be determined"
    }
    $output = & $bcdedit.Source /enum "{current}" 2>&1 | Out-String
    return ($output -notmatch '(?im)^\s*testsigning\s+Yes\s*$')
}

<#
Customer-release trust is the conjunction. A missing kind is an unmet gate,
never a warning the caller continues past. Returns the kinds actually proved;
throws naming the kinds that were not.
#>
function Test-RamSharedCustomerReleaseTrust {
    param([Parameter(Mandatory = $true)][string]$PackageDir)
    $resolved = Resolve-Path -LiteralPath $PackageDir -ErrorAction SilentlyContinue
    if (-not $resolved) {
        throw "package directory not found: $PackageDir"
    }
    $root = $resolved.Path
    $sys = Join-Path $root "ramshared.sys"
    $cat = Join-Path $root "ramshared.cat"
    $inf = Join-Path $root "ramshared.inf"
    foreach ($path in @($sys, $cat, $inf)) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "package is missing $(Split-Path -Leaf $path)"
        }
    }

    $proved = @()
    $missing = @()

    foreach ($name in @("ramshared.sys", "ramshared.cat")) {
        $path = Join-Path $root $name
        if (-not (Test-RamSharedApplicationPublisherSignature -Path $path)) {
            $missing += "application-publisher:$name"
        }
    }
    if ($missing.Count -eq 0) { $proved += "application-publisher" }

    if (Test-RamSharedNotSelfSigned -Path $sys) {
        $proved += "not-test-signed"
    } else {
        $missing += "not-test-signed:signer is self-signed or absent"
    }

    try {
        if (Test-RamSharedKernelPolicySignature -Catalog $cat -Driver $sys) {
            $proved += "catalog-membership"
            $proved += "kernel-policy"
        } else {
            $missing += "kernel-policy:SignTool verify /kp failed"
            $missing += "catalog-membership:catalog does not cover the driver"
        }
    } catch {
        $missing += "kernel-policy:$($_.Exception.Message)"
        $missing += "catalog-membership:kernel-policy verification unavailable"
    }

    if ($missing.Count -gt 0) {
        throw ("customer-release trust not established; missing: {0}" -f ($missing -join "; "))
    }
    return $proved
}
