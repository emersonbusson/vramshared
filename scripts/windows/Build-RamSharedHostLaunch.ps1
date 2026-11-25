#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Build-RamSharedHostLaunch.ps1
<#
.SYNOPSIS
    Build and stage the windowless Windows host launcher (SPEC DT-11/DT-12).

.DESCRIPTION
    Builds `crates/ramshared-hostlaunch` for the Windows target, verifies that
    the produced image is a GUI-subsystem executable (no console is allocated
    for it), and stages it beside the guardian payload sources so the installed
    action can name one sealed binary.

    The launcher is a build product, never a checked-in artifact: `-StageDirectory`
    defaults to this script's directory, which is git-ignored for this file.

.PARAMETER RepoRoot
    Repository root. Defaults to two levels above this script.

.PARAMETER Configuration
    Cargo profile. Defaults to `release`.

.PARAMETER Target
    Rust target triple. Defaults to `x86_64-pc-windows-msvc`.

.PARAMETER Install
    Also copy the verified launcher to C:\ProgramData\RamShared\bin. Requires an
    elevated session.

.PARAMETER VerifyOnly
    Verify an already-built image without invoking Cargo.

.EXAMPLE
    .\Build-RamSharedHostLaunch.ps1 -Install
#>
[CmdletBinding()]
param(
    [string]$RepoRoot,
    [ValidateSet("debug", "release")][string]$Configuration = "release",
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$StageDirectory,
    [switch]$Install,
    [switch]$VerifyOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    $RepoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
}
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot -ErrorAction Stop).Path
if ([string]::IsNullOrWhiteSpace($StageDirectory)) { $StageDirectory = $PSScriptRoot }

$LauncherName = "ramshared-host-launch.exe"
$Executable = Join-Path $RepoRoot (Join-Path "target" (Join-Path $Target (Join-Path $Configuration $LauncherName)))
$WindowsGuiSubsystem = 2
$WindowsConsoleSubsystem = 3

function Get-PeSubsystem {
    <#
      Return the PE optional-header Subsystem value. The offset is 68 for both
      PE32 and PE32+, which is why the magic is still validated: a non-PE file
      that happens to have the bytes must be refused, not read as a subsystem.
    #>
    param([Parameter(Mandatory = $true)][string]$Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 512) { throw "host_launch_pe_too_short" }
    if ([BitConverter]::ToUInt16($bytes, 0) -ne 0x5A4D) { throw "host_launch_pe_dos_signature_missing" }
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    if ($peOffset -lt 0 -or ($peOffset + 24 + 70) -gt $bytes.Length) { throw "host_launch_pe_header_out_of_range" }
    if ([BitConverter]::ToUInt32($bytes, $peOffset) -ne 0x00004550) { throw "host_launch_pe_signature_missing" }
    $optionalHeader = $peOffset + 24
    $magic = [BitConverter]::ToUInt16($bytes, $optionalHeader)
    if ($magic -ne 0x10B -and $magic -ne 0x20B) { throw "host_launch_pe_optional_header_unknown" }
    return [BitConverter]::ToUInt16($bytes, $optionalHeader + 68)
}

if (-not $VerifyOnly) {
    if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot "Cargo.toml") -PathType Leaf)) {
        throw "host_launch_repo_root_invalid"
    }
    Push-Location $RepoRoot
    try {
        $cargoArguments = @("build", "-p", "ramshared-hostlaunch", "--target", $Target)
        if ($Configuration -eq "release") { $cargoArguments += "--release" }
        & cargo @cargoArguments
        if ($LASTEXITCODE -ne 0) { throw "host_launch_cargo_build_failed" }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) {
    throw ("host_launch_executable_missing: " + $Executable)
}
$subsystem = Get-PeSubsystem -Path $Executable
if ($subsystem -eq $WindowsConsoleSubsystem) {
    throw "host_launch_allocates_a_console: the launcher must be a GUI-subsystem image"
}
if ($subsystem -ne $WindowsGuiSubsystem) {
    throw ("host_launch_subsystem_unexpected: " + $subsystem)
}
Write-Output "PASS host_launch_is_gui_subsystem"

New-Item -ItemType Directory -Force -Path $StageDirectory | Out-Null
$staged = Join-Path $StageDirectory $LauncherName
if ([string]($Executable) -cne [string]($staged)) {
    [IO.File]::Copy($Executable, $staged, $true)
}
$stagedSubsystem = Get-PeSubsystem -Path $staged
if ($stagedSubsystem -ne $WindowsGuiSubsystem) { throw "host_launch_stage_verification_failed" }
Write-Output ("HOST_LAUNCH_STAGED=" + $staged)

if ($Install) {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "host_launch_install_requires_elevation"
    }
    $installRoot = "C:\ProgramData\RamShared\bin"
    New-Item -ItemType Directory -Force -Path $installRoot | Out-Null
    $installed = Join-Path $installRoot $LauncherName
    [IO.File]::Copy($staged, $installed, $true)
    if ((Get-PeSubsystem -Path $installed) -ne $WindowsGuiSubsystem) { throw "host_launch_install_verification_failed" }
    Write-Output ("HOST_LAUNCH_INSTALLED=" + $installed)
}

Write-Output "HOST_LAUNCH_BUILD=PASS"
