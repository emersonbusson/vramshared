#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Sign-Drivers.ps1
<#
.SYNOPSIS
  Lab test-signing of ramshared.sys / poolstress.sys for VM load (testsigning ON).

.DESCRIPTION
  Lab tool contract only. This is not a customer-release qualification: the
  outcome is LAB_SIGNED under scope lab-test-signing. Customer release is the
  BUILT -> VALIDATED -> AWAITING_SIGNING -> SIGNED_RETURN_VERIFIED ->
  READY_FOR_CUSTOMER_INSTALL ladder in ReleaseStage.ps1.

  A signed catalog is mandatory. A missing Inf2Cat or a missing catalog fails
  the lab package contract; neither continues into SIGN_OK.

.PARAMETER PfxPath
  Code-signing PFX. Create once with New-SelfSignedCertificate -Type CodeSigningCert.

.PARAMETER PfxPassword
  Or env RAMSHARED_TESTSIGN_PFX_PASSWORD.

.PARAMETER CertSubject
  Code-signing certificate subject used when no PFX password is supplied.

.PARAMETER CertStore
  Certificate store for subject-based signing.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = "C:\ramshared\src",
    [string]$PfxPath = "C:\ramshared\artifacts\certs\ramshared-test.pfx",
    [string]$PfxPassword = $env:RAMSHARED_TESTSIGN_PFX_PASSWORD,
    [string]$CertSubject = "RamShared Test Signing",
    [string]$PackageWorkDir = "C:\ramshared\artifacts\driver-package-build",
    [ValidateSet("CurrentUser", "LocalMachine")]
    [string]$CertStore = "LocalMachine",
    [switch]$BumpDriverVer,
    [string]$WinsvcExe = ""
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "ReleaseStage.ps1")
$signtool = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match "\\x64\\" } | Select-Object -First 1 -ExpandProperty FullName
if (-not $signtool) { throw "signtool.exe not found (install WDK)" }

function Invoke-SignTool {
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )
    $args = @("sign", "/fd", "SHA256")
    if (-not [string]::IsNullOrEmpty($PfxPassword)) {
        $args += @("/f", $PfxPath, "/p", $PfxPassword)
    } else {
        if ([string]::IsNullOrWhiteSpace($CertSubject)) {
            throw "Set -CertSubject or RAMSHARED_TESTSIGN_PFX_PASSWORD"
        }
        if ($CertStore -eq "LocalMachine") {
            $args += "/sm"
        }
        $args += @("/s", "My", "/n", $CertSubject)
    }
    $args += $Path
    & $signtool @args
    if ($LASTEXITCODE -ne 0) { throw "signtool failed $LASTEXITCODE" }
    & $signtool verify /pa $Path
    if ($LASTEXITCODE -ne 0) { throw "signtool verify failed $LASTEXITCODE" }
}

$files = @(
    (Join-Path $RepoRoot "drivers\windows\ramshared\x64\Release\ramshared.sys"),
    (Join-Path $RepoRoot "drivers\windows\tools\poolstress\x64\Release\poolstress.sys")
)
foreach ($f in $files) {
    if (-not (Test-Path $f)) { throw "missing $f - run Build-Drivers.ps1 first" }
    Write-Host "SIGN $f"
    Invoke-SignTool -Path $f
}

# DT-25: Inf2Cat + sign catalog so pnputil accepts the package under testsigning
$pkg = Join-Path $RepoRoot "drivers\windows\ramshared\x64\Release\package"
New-Item -ItemType Directory -Force -Path $pkg | Out-Null
$pkgLocal = $pkg
if ($pkg -like '\\*') {
    # UNC package staging: Inf2Cat cannot consume a WSL UNC directory.
    $pkgLocal = $PackageWorkDir
    New-Item -ItemType Directory -Force -Path $pkgLocal | Out-Null
}
Copy-Item (Join-Path $RepoRoot "drivers\windows\ramshared\x64\Release\ramshared.sys") (Join-Path $pkgLocal "ramshared.sys") -Force
Copy-Item (Join-Path $RepoRoot "drivers\windows\ramshared\ramshared.inf") (Join-Path $pkgLocal "ramshared.inf") -Force
if ($BumpDriverVer) {
    $winsvc = $WinsvcExe
    if ([string]::IsNullOrWhiteSpace($winsvc)) {
        $winsvc = Join-Path $RepoRoot "target\release\ramshared-winsvc.exe"
    }
    if (-not (Test-Path $winsvc)) {
        throw "ramshared-winsvc.exe not found at $winsvc - build ramshared-winsvc before -BumpDriverVer"
    }
    $infTarget = Join-Path $pkgLocal "ramshared.inf"
    & $winsvc package-bump-inf --inf $infTarget
    if ($LASTEXITCODE -ne 0) { throw "package-bump-inf failed $LASTEXITCODE" }
}

$inf2cat = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x86\Inf2Cat.exe"
if (-not (Test-Path $inf2cat)) {
    $inf2cat = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter Inf2Cat.exe -EA SilentlyContinue |
        Select-Object -First 1 -ExpandProperty FullName
}
# AUD-45: a lab package without a signed catalog is not a package pnputil
# accepts. A missing tool or a missing catalog fails this contract; neither is
# a warning the run continues past into SIGN_OK.
if (-not $inf2cat -or -not (Test-Path $inf2cat)) {
    Write-RamSharedOutcomeReport -Scope "lab-test-signing" -Stage "NOT_BUILT" -Detail "Inf2Cat missing"
    throw "Inf2Cat.exe not found - catalog generation is mandatory for the lab package contract"
}
Write-Host "INF2CAT $inf2cat"
Push-Location $pkgLocal
try {
    & $inf2cat /driver:. /os:10_X64 /verbose
    if ($LASTEXITCODE -ne 0) { throw "Inf2Cat failed $LASTEXITCODE" }
    $cat = Join-Path $pkgLocal "ramshared.cat"
    if (-not (Test-Path $cat)) {
        Write-RamSharedOutcomeReport -Scope "lab-test-signing" -Stage "NOT_BUILT" -Detail "catalog not produced"
        throw "ramshared.cat not produced - the lab package contract requires a catalog"
    }
    Invoke-SignTool -Path $cat
    Write-Host "CAT_OK $cat"
} finally {
    Pop-Location
}
if ($pkgLocal -ne $pkg) {
    Copy-Item $pkgLocal\* $pkg -Force
}
Write-Host "SIGN_OK package=$pkg"
Write-RamSharedOutcomeReport -Scope "lab-test-signing" -Stage "LAB_SIGNED"
