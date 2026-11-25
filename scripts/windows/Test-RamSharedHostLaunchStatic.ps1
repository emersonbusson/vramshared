#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-RamSharedHostLaunchStatic.ps1
<#
.SYNOPSIS
    Static contract for the windowless Windows host launcher (SPEC ITEM-4/ITEM-5).

.DESCRIPTION
    Asserts the properties that kept a visible PowerShell window in the boot
    path: one GUI-subsystem launcher, one absolute interpreter, one sealed
    launcher identity in the payload seal, a snapshot/rollback of the launcher
    bytes, and no direct `sudo.exe`/`powershell.exe` fallback in the guest or
    in the postmortem collector.

    The staged executable is optional. When present its PE subsystem is read;
    when absent the check reports SKIP instead of pretending the build ran.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    $RepoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
}
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot -ErrorAction Stop).Path

function Read-RequiredFile {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw ("host_launch_static: missing file " + $Path)
    }
    return Get-Content -Raw -LiteralPath $Path
}

function Require-Contract {
    param(
        [Parameter(Mandatory = $true)][string]$Text,
        [Parameter(Mandatory = $true)][string]$Contract,
        [Parameter(Mandatory = $true)][string]$SourceName
    )
    if (-not $Text.Contains($Contract)) {
        throw ("host_launch_static: " + $SourceName + " is missing contract " + $Contract)
    }
}

$crateRoot = Join-Path $RepoRoot "crates/ramshared-hostlaunch"
$cargo = Read-RequiredFile -Path (Join-Path $crateRoot "Cargo.toml")
$main = Read-RequiredFile -Path (Join-Path $crateRoot "src/main.rs")
$argv = Read-RequiredFile -Path (Join-Path $crateRoot "src/argv.rs")
$launch = Read-RequiredFile -Path (Join-Path $crateRoot "src/launch.rs")
$workspaceCargo = Read-RequiredFile -Path (Join-Path $RepoRoot "Cargo.toml")
$hostOrigin = Read-RequiredFile -Path (Join-Path $RepoRoot "crates/ramshared-cli/src/cascade/cascade_io/host_origin.rs")
$watchdog = Read-RequiredFile -Path (Join-Path $PSScriptRoot "Watch-RamSharedWsl.ps1")
$postmortem = Read-RequiredFile -Path (Join-Path $RepoRoot "scripts/safety/postmortem.sh")
$buildScript = Read-RequiredFile -Path (Join-Path $PSScriptRoot "Build-RamSharedHostLaunch.ps1")

foreach ($contract in @(
    'crates/ramshared-hostlaunch'
)) {
    Require-Contract -Text $workspaceCargo -Contract $contract -SourceName "Cargo.toml (workspace)"
}
foreach ($contract in @(
    'name = "ramshared-hostlaunch"',
    'name = "ramshared-host-launch"',
    'Win32_Foundation',
    'Win32_System_Threading'
)) {
    Require-Contract -Text $cargo -Contract $contract -SourceName "Cargo.toml"
}
foreach ($contract in @(
    'windows_subsystem = "windows"',
    'fn main() -> ExitCode',
    'fn parse_launch_args',
    'struct LaunchRequest',
    'fn resolve_child',
    'DEFAULT_TIMEOUT_MS',
    'MAX_TIMEOUT_MS',
    'the_option_surface_is_strict',
    'a_relative_target_without_sudo_is_refused'
)) {
    Require-Contract -Text $main -Contract $contract -SourceName "main.rs"
}
foreach ($contract in @(
    'fn quote_argument',
    'fn build_command_line',
    'fn to_windows_path',
    'MAX_COMMAND_LINE_UNITS',
    'windows_argument_quoting_matches_the_ms_rules',
    'an_argument_with_nul_is_refused',
    'a_mnt_drive_path_becomes_a_windows_path'
)) {
    Require-Contract -Text $argv -Contract $contract -SourceName "argv.rs"
}
foreach ($contract in @(
    'CreateProcessW',
    'CREATE_NO_WINDOW',
    'STARTF_USESTDHANDLES',
    'DuplicateHandle',
    'WaitForSingleObject',
    'TerminateProcess',
    'GetExitCodeProcess',
    'WindowsOnly'
)) {
    Require-Contract -Text $launch -Contract $contract -SourceName "launch.rs"
}

# The guest names the launcher, and a missing launcher is the stable refusal.
foreach ($contract in @(
    'WINDOWS_HOST_LAUNCHER',
    'HOST_LAUNCHER_WINDOWS_PATH',
    'VISIBLE_ELEVATION_REFUSED',
    'fn host_launcher_program',
    'host_launcher_argv',
    '"--timeout-ms"',
    '"--sudo"',
    '"--"'
)) {
    Require-Contract -Text $hostOrigin -Contract $contract -SourceName "host_origin.rs"
}
if ($hostOrigin.Contains('WINDOWS_POWERSHELL_PROGRAM: &str = "powershell.exe"')) {
    throw "host_launch_static: the guardian ensure call still resolves PowerShell through PATH"
}
if (-not $hostOrigin.Contains('WSL_WINDOWS_PROGRAM')) {
    throw "host_launch_static: the origin attach lost its wsl.exe child"
}

# The task action, the payload seal, and the transaction snapshot.
foreach ($contract in @(
    '$HostLauncherPath',
    'ramshared-host-launch.exe',
    '$WindowsPowerShellPath',
    'New-ScheduledTaskAction -Execute $HostLauncherPath',
    'launcher_sha256',
    'launcher_snapshot_path',
    'guardian_launcher_source_missing',
    '$GuardianLauncherTimeoutMs'
)) {
    Require-Contract -Text $watchdog -Contract $contract -SourceName "Watch-RamSharedWsl.ps1"
}
if ($watchdog.Contains('New-ScheduledTaskAction -Execute "PowerShell.exe"')) {
    throw "host_launch_static: the scheduled task still runs PowerShell.exe through PATH"
}
if ($watchdog -notmatch 'launcher_path\s*=\s*\$HostLauncherPath') {
    throw "host_launch_static: the payload seal does not name the installed launcher"
}
if ($watchdog -notmatch '\[IO\.File\]::WriteAllBytes\(\$transaction\.launcher_snapshot_path') {
    throw "host_launch_static: the install transaction does not snapshot the launcher bytes"
}

# The postmortem collector has no visible fallback either.
foreach ($contract in @(
    'HOST_LAUNCHER=',
    'PS_LAUNCH=',
    'VISIBLE_ELEVATION_REFUSED'
)) {
    Require-Contract -Text $postmortem -Contract $contract -SourceName "postmortem.sh"
}
if ($postmortem -match '(?m)^\s*"\$PS"\s+-NoProfile') {
    throw "host_launch_static: postmortem.sh still invokes PowerShell directly"
}

foreach ($contract in @(
    'Get-PeSubsystem',
    'WindowsGuiSubsystem',
    'host_launch_allocates_a_console',
    'cargo'
)) {
    Require-Contract -Text $buildScript -Contract $contract -SourceName "Build-RamSharedHostLaunch.ps1"
}

$staged = Join-Path $PSScriptRoot "ramshared-host-launch.exe"
if (Test-Path -LiteralPath $staged -PathType Leaf) {
    $bytes = [IO.File]::ReadAllBytes($staged)
    if ($bytes.Length -lt 512 -or [BitConverter]::ToUInt16($bytes, 0) -ne 0x5A4D) {
        throw "host_launch_static: the staged launcher is not a PE image"
    }
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    if ([BitConverter]::ToUInt32($bytes, $peOffset) -ne 0x00004550) {
        throw "host_launch_static: the staged launcher has no PE signature"
    }
    $subsystem = [BitConverter]::ToUInt16($bytes, $peOffset + 24 + 68)
    if ($subsystem -ne 2) {
        throw ("host_launch_static: the staged launcher is subsystem " + $subsystem + ", not GUI")
    }
    Write-Output "PASS host_launch_staged_image_is_gui_subsystem"
} else {
    Write-Output "SKIP host_launch_staged_image_absent"
}

Write-Output "PASS host_launch_static_contract"
