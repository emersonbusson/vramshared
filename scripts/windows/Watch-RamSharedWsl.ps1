#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Watch-RamSharedWsl.ps1
<#
.SYNOPSIS
  Plan-first, host-side guardian for one sealed WSL distro.

.DESCRIPTION
  The guardian is outside the guest failure domain. It never reboots Windows or
  shuts down all WSL distros. A termination requires the heartbeat, two guest
  probes, an independent WSL/HCS probe, and a closed host snapshot. Mutating
  actions require -Run and are source-only until separately approved.
#>
[CmdletBinding()]
param(
    [ValidateSet("install", "status", "capture", "uninstall", "watch", "activate", "ensure", "test")]
    [string]$Action = "status",
    [switch]$Run,
    [string]$ApproveGuardianAction = "",
    [string]$ApproveGuardianActivation = "",
    [ValidatePattern('^[A-Za-z0-9._-]+$')]
    [string]$Distro = "Ubuntu-24.04",
    [ValidatePattern('^S-1-[0-9-]+$')]
    [string]$UserSid = ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value),
    # DT-19: bindable so a guest start can name the task its sealed identity
    # authorized.  The default is the registration constant -- one home here --
    # and `Test-SealedGuardianIdentity` refuses any value the sealed document
    # does not also name, so a start site never invents a name.
    [ValidatePattern('^[A-Za-z0-9._-]+$')]
    [string]$TaskName = "RamSharedWslGuardian.v1",
    [string]$HeartbeatPath = "C:\wsl-forensics\ramshared-heartbeat.json",
    [string]$ArtifactRoot = "C:\ramshared\artifacts",
    [ValidateRange(15, 60)][int]$StaleAfterSec = 15,
    [ValidateRange(1, 30)][int]$PollSec = 1,
    [ValidateRange(1, 30)][int]$GuestCommandTimeoutSec = 5
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
Import-Module (Join-Path $PSScriptRoot "SharedWslHostMemoryGate.psm1") -Force

# $TaskName is a parameter (DT-19): a guest start names the task its sealed
# identity authorized, and the registration default lives only in the param
# block above.  Re-assigning it here would silently drop the caller's name.
$ProgramDataRoot = "C:\ProgramData\RamShared"
# The product-owned host payload.  The scheduled task's action binds to this
# path and never to $PSScriptRoot: a caller's launch directory is a source
# location, not an authority.  Registering against whatever directory the
# operator happened to run from is what let a stale hand-promotion become the
# registered authority.
$GuardianPayloadRoot = Join-Path $ProgramDataRoot "guardian"
$GuardianPayloadScriptName = "Watch-RamSharedWsl.ps1"
$GuardianPayloadModuleName = "SharedWslHostMemoryGate.psm1"
$GuardianPayloadScriptPath = Join-Path $GuardianPayloadRoot $GuardianPayloadScriptName
$GuardianPayloadModulePath = Join-Path $GuardianPayloadRoot $GuardianPayloadModuleName
$GuardianPayloadSealPath = Join-Path $GuardianPayloadRoot "payload.seal.json"
# SPEC DT-11/DT-12: the one windowless launcher and the absolute interpreter.
# The task action names both; `PowerShell.exe` through PATH is a hijack
# surface and an unsealed identity.
$HostLauncherRoot = Join-Path $ProgramDataRoot "bin"
$HostLauncherName = "ramshared-host-launch.exe"
$HostLauncherPath = Join-Path $HostLauncherRoot $HostLauncherName
$HostLauncherSourcePath = Join-Path $PSScriptRoot $HostLauncherName
$WindowsPowerShellPath = "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
# The guardian producer is long-lived (task ExecutionTimeLimit PT0S): the
# launcher must not bound it on a timer. `0` is the explicit no-deadline value.
$GuardianLauncherTimeoutMs = 0
$SafeModeRoot = Join-Path $ProgramDataRoot "safe-mode"
$GuardianStateRoot = Join-Path $ProgramDataRoot "guardian-state"
$TaskBackupRoot = Join-Path $ProgramDataRoot "guardian-backup"
$GuardianConfigPath = Join-Path $ProgramDataRoot "guardian-config.json"
$OriginManifestPath = Join-Path $ProgramDataRoot "ramshared-origin-manifest.json"
$GuardianTaskBackupPath = Join-Path $TaskBackupRoot ($TaskName + ".xml")
$GuardianTaskSealPath = Join-Path $TaskBackupRoot ($TaskName + ".seal.json")
$GuardianTaskAbsentBackupPath = Join-Path $TaskBackupRoot ($TaskName + ".absent")
$GuardianConfigBackupPath = Join-Path $TaskBackupRoot ($TaskName + ".config.json")
$GuardianConfigAbsentBackupPath = Join-Path $TaskBackupRoot ($TaskName + ".config.absent")
$GuardianHealthPath = Join-Path $GuardianStateRoot ($Distro + ".health.json")
# The running watch publishes its exact process instance and the payload bytes
# it loaded.  A payload swap is only safe when the instance that loaded the
# prior bytes is stopped before the new bytes land; the record is how the
# swap owner proves which process that is without matching a generic
# powershell.exe name.
$GuardianWatchInstancePath = Join-Path $GuardianStateRoot ($Distro + ".watch-instance.json")
$ResumeLeasePath = "/run/ramshared/host-resume-lease.json"
$GuardianActionApproval = "RAMSHARED_ATTENDED_GUARDIAN_ACTION"
$GuardianActivationApproval = "RAMSHARED_ATTENDED_GUARDIAN_ACTIVATION"
# System.ServiceProcess.ServiceControllerStatus::Running serializes as enum value 4.
$HcsServiceRunningValue = 4
$HcsServiceStatusQuery = 'Get-Service -Name vmcompute | Select-Object Name, @{ Name = "Status"; Expression = { $_.Status.ToString() } } | ConvertTo-Json -Compress'
$script:LastGuestBootProbe = $null

function Resolve-GuardianTaskUserName {
    param([Parameter(Mandatory = $true)][string]$Sid)
    try {
        $taskUserName = [System.Security.Principal.SecurityIdentifier]::new($Sid).Translate([System.Security.Principal.NTAccount]).Value
    } catch {
        throw ("guardian policy SID cannot resolve to a task account: " + $Sid)
    }
    if ([string]::IsNullOrWhiteSpace($taskUserName)) {
        throw "guardian policy SID resolved to an empty task account"
    }
    return $taskUserName
}

$TaskUserName = Resolve-GuardianTaskUserName -Sid $UserSid

function Test-AbsoluteWindowsPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    return $Path -match '^[A-Za-z]:\\'
}

function Move-GuardianFileReplacing {
    # Move-Item -Force does not replace an existing destination file on
    # Windows: it throws ERROR_ALREADY_EXISTS ("Cannot create a file when that
    # file already exists"). Every atomic write and every snapshot restore
    # lands on a path that may already be there, so the replace is owned here
    # once instead of being re-invented at each call site.
    #
    # File.Replace is the atomic same-volume replace and it consumes the
    # source. Its third argument is the backup path and it must be a real
    # path: Windows PowerShell coerces $null to "" for a string parameter,
    # Path.GetFullPathInternal("") then throws ArgumentException "The path
    # has an invalid format", and the write looks like a bad path rather
    # than a bad argument. The backup is therefore named explicitly, kept
    # only for the duration of the call, and removed afterwards; the
    # product's own rollback owns its durable snapshots. File.Replace is
    # same-volume only, which every caller satisfies by construction (the
    # atomic writer stages in the destination's own directory, and snapshots
    # are created beside the product under the same root).
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "guardian_move_source_missing" }
    if (Test-Path -LiteralPath $Destination -PathType Leaf) {
        $destinationDirectory = Split-Path -Parent $Destination
        $backup = Join-Path $destinationDirectory ((Split-Path -Leaf $Destination) + "." + [Guid]::NewGuid().ToString("N") + ".replaced")
        try {
            [IO.File]::Replace($Source, $Destination, $backup)
        } finally {
            if (Test-Path -LiteralPath $backup -PathType Leaf) {
                Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
            }
        }
    } else {
        [IO.File]::Move($Source, $Destination)
    }
}

function Clear-GuardianTransientResidue {
    # Move-GuardianFileReplacing names its backup beside the destination and
    # removes it in a finally. That is the only cleanup, so a process killed
    # inside the call -- or a remove Windows refuses because the file is
    # briefly locked -- leaves the transient behind with nothing to sweep it.
    # Observed live: a guardian-config.json.<guid>.replaced survived on the
    # product root while the config it backed had already been replaced.
    #
    # The product owns these roots and these two patterns are exactly the
    # names its own helpers mint, so the install transaction clears them
    # before it writes anything new. Anything else in the root is operator or
    # product state and is never touched.
    param([Parameter(Mandatory = $true)][string[]]$Roots)
    $removed = 0
    foreach ($root in $Roots) {
        if (-not (Test-Path -LiteralPath $root -PathType Container)) { continue }
        foreach ($pattern in @("*.replaced", "*.tmp")) {
            foreach ($file in @(Get-ChildItem -LiteralPath $root -Filter $pattern -File -ErrorAction SilentlyContinue)) {
                Remove-Item -LiteralPath $file.FullName -Force -ErrorAction SilentlyContinue
                if (-not (Test-Path -LiteralPath $file.FullName -PathType Leaf)) { $removed++ }
            }
        }
    }
    return $removed
}

function Write-AtomicJson {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][object]$Value)
    $directory = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $temporary = Join-Path $directory ((Split-Path -Leaf $Path) + "." + [Guid]::NewGuid().ToString("N") + ".tmp")
    try {
        [IO.File]::WriteAllText($temporary, ($Value | ConvertTo-Json -Depth 8), [Text.Encoding]::UTF8)
        Move-GuardianFileReplacing -Source $temporary -Destination $Path
    } finally {
        if (Test-Path -LiteralPath $temporary -PathType Leaf) { Remove-Item -LiteralPath $temporary -Force }
    }
}

function Get-GuardianFileSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "guardian_payload_file_missing" }
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $stream = [IO.File]::OpenRead($Path)
        try { return ([BitConverter]::ToString($sha256.ComputeHash($stream)) -replace '-', '') }
        finally { $stream.Dispose() }
    } finally { $sha256.Dispose() }
}

function Get-GuardianPayloadIdentity {
    # Observations only: what is deployed, and what is running.  The comparison
    # and the refusal live in Assert-GuardianPayloadIdentity so a caller cannot
    # accidentally treat a mismatch as a value.
    return [ordered]@{
        deployed_script_sha256 = $(if (Test-Path -LiteralPath $GuardianPayloadScriptPath -PathType Leaf) { Get-GuardianFileSha256 -Path $GuardianPayloadScriptPath } else { $null })
        deployed_module_sha256 = $(if (Test-Path -LiteralPath $GuardianPayloadModulePath -PathType Leaf) { Get-GuardianFileSha256 -Path $GuardianPayloadModulePath } else { $null })
        deployed_launcher_sha256 = $(if (Test-Path -LiteralPath $HostLauncherPath -PathType Leaf) { Get-GuardianFileSha256 -Path $HostLauncherPath } else { $null })
        self_script_sha256 = Get-GuardianFileSha256 -Path (Join-Path $PSScriptRoot $GuardianPayloadScriptName)
        self_module_sha256 = Get-GuardianFileSha256 -Path (Join-Path $PSScriptRoot $GuardianPayloadModuleName)
    }
}

function Write-GuardianPayloadSeal {
    param(
        [Parameter(Mandatory = $true)][string]$ScriptSha256,
        [Parameter(Mandatory = $true)][string]$ModuleSha256,
        [Parameter(Mandatory = $true)][string]$LauncherSha256
    )
    if ($ScriptSha256 -notmatch '^[0-9A-F]{64}$' -or $ModuleSha256 -notmatch '^[0-9A-F]{64}$' -or $LauncherSha256 -notmatch '^[0-9A-F]{64}$') {
        throw "guardian_payload_seal_invalid"
    }
    # Schema 2 adds the launcher identity: the action names a sealed binary and
    # an absolute interpreter, so both belong to the payload seal.
    Write-AtomicJson -Path $GuardianPayloadSealPath -Value ([ordered]@{
        schema_version = 2; task_name = $TaskName
        script_name = $GuardianPayloadScriptName; module_name = $GuardianPayloadModuleName
        launcher_name = $HostLauncherName; launcher_path = $HostLauncherPath
        powershell_path = $WindowsPowerShellPath
        script_sha256 = $ScriptSha256; module_sha256 = $ModuleSha256
        launcher_sha256 = $LauncherSha256
    })
}

function Test-GuardianPayloadIdentityMatch {
    # Pure decision: given a seal and four observed hashes, is this the sealed
    # payload?  Kept free of filesystem and of side effects so the manufactured
    # cases can drive every branch without touching the host.
    param(
        [Parameter(Mandatory = $true)][object]$Seal,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$SelfScriptSha256,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$SelfModuleSha256,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$DeployedScriptSha256,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$DeployedModuleSha256,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$LauncherSha256
    )
    if ([string]$Seal.script_sha256 -notmatch '^[0-9A-F]{64}$' -or [string]$Seal.module_sha256 -notmatch '^[0-9A-F]{64}$' -or [string]$Seal.launcher_sha256 -notmatch '^[0-9A-F]{64}$') { return $false }
    if ([string]$SelfScriptSha256 -cne [string]$Seal.script_sha256) { return $false }
    if ([string]$SelfModuleSha256 -cne [string]$Seal.module_sha256) { return $false }
    if ([string]$DeployedScriptSha256 -cne [string]$Seal.script_sha256) { return $false }
    if ([string]$DeployedModuleSha256 -cne [string]$Seal.module_sha256) { return $false }
    if ([string]$LauncherSha256 -cne [string]$Seal.launcher_sha256) { return $false }
    return $true
}

function Assert-GuardianPayloadIdentity {
    # Refuse to act as a copy that is not the one the install transaction
    # deployed and sealed.  Identity is about bytes, not about location: a
    # byte-identical source tree matches, an edited or superseded copy does
    # not.  This is what stops a stale hand-promotion from running as the
    # authority again.
    if (-not (Test-Path -LiteralPath $GuardianPayloadSealPath -PathType Leaf)) { throw "guardian_payload_seal_missing" }
    try { $seal = Get-Content -Raw -LiteralPath $GuardianPayloadSealPath | ConvertFrom-Json -ErrorAction Stop }
    catch { throw "guardian_payload_seal_invalid" }
    if ($seal.schema_version -ne 2 -or $seal.task_name -cne $TaskName -or
        [string]$seal.script_name -cne $GuardianPayloadScriptName -or
        [string]$seal.module_name -cne $GuardianPayloadModuleName -or
        [string]$seal.launcher_name -cne $HostLauncherName -or
        [string]$seal.launcher_path -cne $HostLauncherPath -or
        [string]$seal.powershell_path -cne $WindowsPowerShellPath) {
        throw "guardian_payload_seal_invalid"
    }
    $identity = Get-GuardianPayloadIdentity
    if (-not (Test-GuardianPayloadIdentityMatch -Seal $seal `
        -SelfScriptSha256 ([string]$identity.self_script_sha256) `
        -SelfModuleSha256 ([string]$identity.self_module_sha256) `
        -DeployedScriptSha256 ([string]$identity.deployed_script_sha256) `
        -DeployedModuleSha256 ([string]$identity.deployed_module_sha256) `
        -LauncherSha256 ([string]$identity.deployed_launcher_sha256))) {
        throw "guardian_payload_identity_mismatch"
    }
}

function Install-GuardianPayload {
    # Deploy the sealed source to the product-owned path, then seal exactly
    # what landed.  The launch directory is the source; the product path is
    # the authority.  When the two are the same directory the copy is a no-op
    # and the seal is recomputed, so a retry over already-deployed bytes is
    # idempotent.
    param([Parameter(Mandatory = $true)][System.Collections.IDictionary]$Transaction)
    $sourceScript = Join-Path $PSScriptRoot $GuardianPayloadScriptName
    $sourceModule = Join-Path $PSScriptRoot $GuardianPayloadModuleName
    # The launcher is a build product staged beside the payload sources. A
    # missing one is a refusal: registering an action that names nothing would
    # fail at the next logon, after the transaction already mutated the host.
    if (-not (Test-Path -LiteralPath $HostLauncherSourcePath -PathType Leaf)) { throw "guardian_launcher_source_missing" }
    $sourceScriptSha256 = Get-GuardianFileSha256 -Path $sourceScript
    $sourceModuleSha256 = Get-GuardianFileSha256 -Path $sourceModule
    $sourceLauncherSha256 = Get-GuardianFileSha256 -Path $HostLauncherSourcePath
    $Transaction.payload_may_be_modified = $true
    New-Item -ItemType Directory -Force -Path $GuardianPayloadRoot | Out-Null
    if ([string]($sourceScript) -cne [string]($GuardianPayloadScriptPath)) {
        Copy-Item -LiteralPath $sourceScript -Destination $GuardianPayloadScriptPath -Force -ErrorAction Stop
    }
    if ([string]($sourceModule) -cne [string]($GuardianPayloadModulePath)) {
        Copy-Item -LiteralPath $sourceModule -Destination $GuardianPayloadModulePath -Force -ErrorAction Stop
    }
    New-Item -ItemType Directory -Force -Path $HostLauncherRoot | Out-Null
    if ([string]($HostLauncherSourcePath) -cne [string]($HostLauncherPath)) {
        Copy-Item -LiteralPath $HostLauncherSourcePath -Destination $HostLauncherPath -Force -ErrorAction Stop
    }
    $deployedScriptSha256 = Get-GuardianFileSha256 -Path $GuardianPayloadScriptPath
    $deployedModuleSha256 = Get-GuardianFileSha256 -Path $GuardianPayloadModulePath
    $deployedLauncherSha256 = Get-GuardianFileSha256 -Path $HostLauncherPath
    if ($deployedScriptSha256 -cne $sourceScriptSha256 -or $deployedModuleSha256 -cne $sourceModuleSha256 -or $deployedLauncherSha256 -cne $sourceLauncherSha256) {
        throw "guardian_payload_deploy_verification_failed"
    }
    Write-GuardianPayloadSeal -ScriptSha256 $deployedScriptSha256 -ModuleSha256 $deployedModuleSha256 -LauncherSha256 $deployedLauncherSha256
}

function Restore-GuardianPayloadSnapshot {
    param([Parameter(Mandatory = $true)][System.Collections.IDictionary]$Transaction)
    if (-not $Transaction.payload_may_be_modified) { return }
    foreach ($entry in @(
        @{ snapshot = $Transaction.payload_script_snapshot_path; destination = $GuardianPayloadScriptPath; had = $Transaction.had_payload_script },
        @{ snapshot = $Transaction.payload_module_snapshot_path; destination = $GuardianPayloadModulePath; had = $Transaction.had_payload_module },
        @{ snapshot = $Transaction.launcher_snapshot_path; destination = $HostLauncherPath; had = $Transaction.had_launcher }
    )) {
        if ($entry.had) {
            Move-GuardianFileReplacing -Source $entry.snapshot -Destination $entry.destination
        } elseif (Test-Path -LiteralPath $entry.destination -PathType Leaf) {
            Remove-Item -LiteralPath $entry.destination -Force
        }
    }
    if ($Transaction.had_payload_seal) {
        Move-GuardianFileReplacing -Source $Transaction.payload_seal_snapshot_path -Destination $GuardianPayloadSealPath
    } elseif (Test-Path -LiteralPath $GuardianPayloadSealPath -PathType Leaf) {
        Remove-Item -LiteralPath $GuardianPayloadSealPath -Force
    }
}

function Write-GuardianEvent {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Event, [hashtable]$Data = @{})
    $record = [ordered]@{ schema_version = 1; timestamp_utc = [DateTime]::UtcNow.ToString("o"); event = $Event; distro = $Distro; data = $Data }
    Add-Content -LiteralPath $Path -Encoding UTF8 -Value ($record | ConvertTo-Json -Compress -Depth 6)
}

function Get-SealedGuardianPolicy {
    return [ordered]@{
        heartbeat_path = $HeartbeatPath
        artifact_root = $ArtifactRoot
        stale_after_seconds = $StaleAfterSec
        poll_seconds = $PollSec
        guest_command_timeout_seconds = $GuestCommandTimeoutSec
    }
}

function Test-SealedGuardianPolicy {
    param([Parameter(Mandatory = $true)][object]$Policy)
    $expected = Get-SealedGuardianPolicy
    $expectedKeys = @($expected.Keys | Sort-Object)
    $actualKeys = @($Policy.PSObject.Properties.Name | Sort-Object)
    if (($actualKeys -join "`n") -cne ($expectedKeys -join "`n")) { return $false }
    try {
        return [string]$Policy.heartbeat_path -ceq [string]$expected.heartbeat_path -and
            [string]$Policy.artifact_root -ceq [string]$expected.artifact_root -and
            [int]$Policy.stale_after_seconds -eq [int]$expected.stale_after_seconds -and
            [int]$Policy.poll_seconds -eq [int]$expected.poll_seconds -and
            [int]$Policy.guest_command_timeout_seconds -eq [int]$expected.guest_command_timeout_seconds
    } catch {
        return $false
    }
}

function Publish-GuardianState {
    param(
        [Parameter(Mandatory = $true)][ValidateSet("HEALTHY", "SAFE_MODE", "BLOCKED")][string]$State,
        [string]$Reason = "watching",
        [AllowNull()][string]$BootId = $null
    )
    if ($State -eq "HEALTHY" -and -not (Test-CanonicalGuestBootId -BootId $BootId)) {
        throw "healthy guardian state requires a canonical guest boot ID"
    }
    Write-AtomicJson -Path $GuardianHealthPath -Value ([ordered]@{ schema_version = 3; timestamp_utc = [DateTime]::UtcNow.ToString("o"); distro = $Distro; user_sid = $UserSid; state = $State; reason = $Reason; boot_id = $BootId; guardian_policy = (Get-SealedGuardianPolicy) })
}

function Protect-GuardianConsoleLifetime {
    <#
      DT-17: a console-control event aimed at a probe child is delivered to
      every process attached to that console. PowerShell's default handler
      turns CTRL_C into STATUS_CONTROL_C_EXIT (0xC000013A), which is exactly
      the exit that left the task `Ready` and the product down after a guest
      restart.
      The durable gate must not be terminable that way at all. Attended stop
      remains Stop-ScheduledTask or -Action uninstall.

      Returns exactly one of:
        "console_taken" — the process had a console and control-C is now input.
        "no_console"    — the process has no console handle at all, so a
                          console-control event cannot be delivered to it
                          either; the gate is already outside the child
                          control-event group.
        "unset"         — a console exists and control-C is still a terminate
                          signal. This is the only unsafe answer.

      Callers must refuse "unset". The distinction matters because the setter
      itself throws when there is no console handle: silence there is not a
      successful claim of control.
    #>
    try {
        [Console]::TreatControlCAsInput = $true
    } catch {
        return "no_console"
    }
    if ([Console]::TreatControlCAsInput) { return "console_taken" }
    return "unset"
}

function Test-CanonicalGuestBootId {
    param([AllowNull()][string]$BootId)
    if ([string]::IsNullOrWhiteSpace($BootId) -or $BootId -cne $BootId.Trim()) { return $false }
    if ($BootId -notmatch '^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$') { return $false }
    $parsed = [guid]::Empty
    if (-not [guid]::TryParseExact($BootId, 'D', [ref]$parsed)) { return $false }
    return $parsed.ToString('D') -ceq $BootId
}

function Limit-GuardianDiagnostics {
    param([AllowNull()][string]$Text)
    if ($null -eq $Text) { return "" }
    $limit = 32768
    $clean = $Text.Replace("`0", "")
    if ($clean.Length -le $limit) { return $clean.Trim() }
    return ($clean.Substring(0, $limit) + "`n[diagnostic truncated]").Trim()
}

function Stop-GuardianProcessInstanceSafely {
    param([Parameter(Mandatory = $true)][object]$Process)
    # Bind the original OS handle before revalidating creation time. Kill() on
    # this Process object acts on that bound handle, never a reopened PID.
    try {
        $Process.Refresh()
        if ($Process.HasExited) { return [pscustomobject]@{ stopped = $true; reason = "process_already_exited" } }
        $boundHandle = $Process.Handle
        $originalStart = $Process.StartTime.ToUniversalTime().Ticks
        $Process.Refresh()
        if ($Process.HasExited) { return [pscustomobject]@{ stopped = $true; reason = "process_already_exited" } }
        if ($Process.StartTime.ToUniversalTime().Ticks -ne $originalStart) {
            return [pscustomobject]@{ stopped = $false; reason = "process_instance_identity_changed" }
        }
        $Process.Kill()
        if (-not $Process.WaitForExit(5000)) { return [pscustomobject]@{ stopped = $false; reason = "process_instance_kill_unreaped" } }
        return [pscustomobject]@{ stopped = $true; reason = "process_instance_handle_terminated" }
    } catch {
        return [pscustomobject]@{ stopped = $false; reason = "process_instance_identity_unproven" }
    }
}

function Test-GuardianProcessStartIdentity {
    param([Parameter(Mandatory = $true)][long]$ExpectedUtcTicks, [Parameter(Mandatory = $true)][long]$ObservedUtcTicks)
    # Win32_Process.CreationDate and Process.StartTime can differ by a few
    # 100 ns ticks for the same instance.  One millisecond admits that
    # representation difference and still refuses a PID reused later.
    return ([Math]::Abs($ExpectedUtcTicks - $ObservedUtcTicks) -le 10000)
}

function Test-GuardianWatchCommandLineMatch {
    # Pure predicate: does this command line name exactly this product's
    # watch instance?  The launcher's own command line and the PowerShell
    # child it starts both carry the sealed action.  The matcher binds the
    # exact script path, the action, the distro, and the configured SID; a
    # generic powershell.exe name is never enough to stop a process.
    #
    # The launcher quotes only what Windows requires, so the child it starts
    # carries `-File C:\...\Watch-RamSharedWsl.ps1` with no quotes at all while
    # the task XML names the same path quoted.  Both spellings are the same
    # instance; refusing the unquoted one would stop the waiting launcher and
    # leave the PowerShell process that actually holds the prior bytes alive.
    param(
        [AllowNull()][string]$CommandLine,
        [Parameter(Mandatory = $true)][string]$ScriptPath,
        [Parameter(Mandatory = $true)][string]$Sid,
        [Parameter(Mandatory = $true)][string]$DistroName
    )
    if ([string]::IsNullOrWhiteSpace($CommandLine)) { return $false }
    foreach ($pair in @(
        @{ Option = "-File"; Value = $ScriptPath },
        @{ Option = "-UserSid"; Value = $Sid },
        @{ Option = "-Distro"; Value = $DistroName }
    )) {
        $escaped = [regex]::Escape($pair.Value)
        $pattern = '(?i)(?:^|\s)' + [regex]::Escape($pair.Option) + '\s+(?:"' + $escaped + '"|' + $escaped + ')(?=\s|$)'
        if (-not [regex]::IsMatch($CommandLine, $pattern)) { return $false }
    }
    return [regex]::IsMatch($CommandLine, '(?i)(?:^|\s)-Action\s+watch(?=\s|$)')
}

function Get-GuardianWatchInstanceProcesses {
    # Every candidate carries the owner SID and the command line.  A command
    # line that cannot be read is returned as null so the caller refuses
    # instead of treating an unprovable process as absent.
    try {
        $raw = @(Get-CimInstance Win32_Process -Filter "Name='powershell.exe' OR Name='ramshared-host-launch.exe'" -ErrorAction Stop)
    } catch {
        throw "guardian_watch_instance_enumeration_failed"
    }
    $candidates = @()
    foreach ($process in $raw) {
        $ownerSid = $null
        try { $ownerSid = [string]($process | Invoke-CimMethod -MethodName GetOwnerSid -ErrorAction Stop).Sid } catch { $ownerSid = $null }
        $created = $null
        if ($null -ne $process.CreationDate) {
            try { $created = ([DateTime]$process.CreationDate).ToUniversalTime() } catch { $created = $null }
        }
        $candidates += [pscustomobject]@{
            process_id = [int]$process.ProcessId
            parent_process_id = [int]$process.ParentProcessId
            name = [string]$process.Name
            creation_date_utc = $created
            owner_sid = $ownerSid
            command_line = [string]$process.CommandLine
        }
    }
    return @($candidates)
}

function Get-GuardianWatchInstanceMatch {
    # Pure selection over observations.  Only a process owned by the sealed
    # policy SID whose command line names exactly the product payload may be
    # a candidate; everything else is left alone.
    #
    # "Unprovable" is narrow on purpose: a PowerShell process with no readable
    # owner or no readable command line could be the watch, so the caller must
    # refuse instead of treating it as absent.  A process owned by a different
    # SID is provably not the task's instance even when its command line is
    # unreadable, and must not block installs on hosts with SYSTEM agents.
    param(
        [AllowNull()][object[]]$Processes,
        [Parameter(Mandatory = $true)][string]$ScriptPath,
        [Parameter(Mandatory = $true)][string]$Sid,
        [Parameter(Mandatory = $true)][string]$DistroName
    )
    $matches = @()
    $unprovable = @()
    foreach ($process in @($Processes)) {
        if ($null -eq $process) { continue }
        $ownerKnown = -not [string]::IsNullOrWhiteSpace([string]$process.owner_sid)
        if ($ownerKnown -and [string]$process.owner_sid -cne $Sid) { continue }
        if ([string]::IsNullOrWhiteSpace([string]$process.command_line)) {
            if ([string]$process.name -ieq "powershell.exe") { $unprovable += $process }
            continue
        }
        if (-not (Test-GuardianWatchCommandLineMatch -CommandLine ([string]$process.command_line) -ScriptPath $ScriptPath -Sid $Sid -DistroName $DistroName)) {
            continue
        }
        if (-not $ownerKnown) { $unprovable += $process; continue }
        $matches += $process
    }
    return [pscustomobject]@{ matches = @($matches); unprovable = @($unprovable) }
}

function Get-GuardianProcessObservation {
    param([Parameter(Mandatory = $true)][int]$ProcessId)
    try {
        $process = Get-Process -Id $ProcessId -ErrorAction Stop
        $startUtc = $process.StartTime.ToUniversalTime()
        $ownerSid = $null
        try {
            $ownerSid = [string](Get-CimInstance Win32_Process -Filter ("ProcessId=" + $ProcessId) -ErrorAction Stop | Invoke-CimMethod -MethodName GetOwnerSid -ErrorAction Stop).Sid
        } catch { $ownerSid = $null }
        return [pscustomobject]@{ alive = $true; process_id = $ProcessId; start_utc_ticks = $startUtc.Ticks; owner_sid = $ownerSid; process = $process }
    } catch {
        return [pscustomobject]@{ alive = $false; process_id = $ProcessId; start_utc_ticks = $null; owner_sid = $null; process = $null }
    }
}

function Read-GuardianWatchInstanceRecord {
    if (-not (Test-Path -LiteralPath $GuardianWatchInstancePath -PathType Leaf)) { return $null }
    try { $record = Get-Content -Raw -LiteralPath $GuardianWatchInstancePath | ConvertFrom-Json -ErrorAction Stop }
    catch { throw "guardian_watch_instance_record_invalid" }
    foreach ($name in @("schema_version", "pid", "process_start_utc_ticks", "user_sid", "task_name", "distro", "script_path", "script_sha256", "module_sha256", "launcher_sha256")) {
        if ($null -eq $record.PSObject.Properties[$name]) { throw "guardian_watch_instance_record_invalid" }
    }
    if ([int]$record.schema_version -ne 1) { throw "guardian_watch_instance_record_invalid" }
    return $record
}

function New-GuardianWatchInstanceRecord {
    param(
        [Parameter(Mandatory = $true)][object]$PayloadIdentity,
        [Parameter(Mandatory = $true)][DateTime]$StartedUtc,
        [Parameter(Mandatory = $true)][int]$ProcessId
    )
    return [ordered]@{
        schema_version = 1
        pid = $ProcessId
        process_start_utc_ticks = $StartedUtc.ToUniversalTime().Ticks
        user_sid = $UserSid
        task_name = $TaskName
        distro = $Distro
        script_path = $GuardianPayloadScriptPath
        script_sha256 = [string]$PayloadIdentity.deployed_script_sha256
        module_sha256 = [string]$PayloadIdentity.deployed_module_sha256
        launcher_sha256 = [string]$PayloadIdentity.deployed_launcher_sha256
        started_utc = $StartedUtc.ToUniversalTime().ToString("o")
    }
}

function Get-GuardianWatchInstanceDecision {
    # Pure decision.  The record is authority only after its SID, task, path,
    # payload shape, and process instance all match.  A record whose process
    # is gone or whose PID was reused is cleared without signaling anything.
    param(
        [AllowNull()][object]$Record,
        [AllowNull()][object]$Observation,
        [Parameter(Mandatory = $true)][string]$ExpectedSid,
        [Parameter(Mandatory = $true)][string]$ExpectedTaskName,
        [Parameter(Mandatory = $true)][string]$ExpectedScriptPath,
        [Parameter(Mandatory = $true)][string]$IncomingScriptSha256
    )
    if ($null -eq $Record) { return [ordered]@{ action = "ABSENT"; reason = "no_recorded_instance" } }
    if ([string]$Record.user_sid -cne $ExpectedSid) { return [ordered]@{ action = "REFUSE"; reason = "recorded_instance_user_sid_mismatch" } }
    if ([string]$Record.task_name -cne $ExpectedTaskName) { return [ordered]@{ action = "REFUSE"; reason = "recorded_instance_task_mismatch" } }
    if ([string]$Record.script_path -ine $ExpectedScriptPath) { return [ordered]@{ action = "REFUSE"; reason = "recorded_instance_script_path_mismatch" } }
    if ([string]$Record.script_sha256 -notmatch '^[0-9A-F]{64}$' -or [string]$Record.module_sha256 -notmatch '^[0-9A-F]{64}$' -or [string]$Record.launcher_sha256 -notmatch '^[0-9A-F]{64}$') {
        return [ordered]@{ action = "REFUSE"; reason = "recorded_instance_payload_identity_invalid" }
    }
    if ($null -eq $Observation -or -not [bool]$Observation.alive) { return [ordered]@{ action = "CLEAR"; reason = "recorded_instance_not_running" } }
    if ([string]$Observation.owner_sid -cne $ExpectedSid) { return [ordered]@{ action = "REFUSE"; reason = "recorded_instance_owner_sid_mismatch" } }
    if (-not (Test-GuardianProcessStartIdentity -ExpectedUtcTicks ([long]$Record.process_start_utc_ticks) -ObservedUtcTicks ([long]$Observation.start_utc_ticks))) {
        return [ordered]@{ action = "CLEAR"; reason = "recorded_instance_pid_reused" }
    }
    if ([string]$Record.script_sha256 -cne $IncomingScriptSha256) { return [ordered]@{ action = "STOP"; reason = "payload_changed" } }
    return [ordered]@{ action = "CURRENT"; reason = "recorded_instance_runs_the_incoming_payload" }
}

function Stop-GuardianWatchInstance {
    param([Parameter(Mandatory = $true)][int]$ProcessId, [Parameter(Mandatory = $true)][long]$StartUtcTicks)
    $process = $null
    try { $process = Get-Process -Id $ProcessId -ErrorAction Stop } catch { return [pscustomobject]@{ stopped = $true; reason = "process_already_exited" } }
    try {
        if (-not (Test-GuardianProcessStartIdentity -ExpectedUtcTicks $StartUtcTicks -ObservedUtcTicks $process.StartTime.ToUniversalTime().Ticks)) {
            return [pscustomobject]@{ stopped = $false; reason = "process_instance_identity_changed" }
        }
    } catch {
        return [pscustomobject]@{ stopped = $false; reason = "process_instance_identity_unproven" }
    }
    return Stop-GuardianProcessInstanceSafely -Process $process
}

function Register-GuardianWatchInstance {
    # A watch refuses to become a second instance.  A record naming a live
    # current instance is a duplicate; a record naming live prior bytes is a
    # swap the install owner did not finish; a dead or reused record is
    # cleared and replaced.  This is the runtime half of the payload-swap
    # lifecycle, and it never signals another process by itself.
    param([Parameter(Mandatory = $true)][object]$PayloadIdentity, [Parameter(Mandatory = $true)][DateTime]$StartedUtc)
    $existing = Read-GuardianWatchInstanceRecord
    if ($null -ne $existing) {
        $observation = Get-GuardianProcessObservation -ProcessId ([int]$existing.pid)
        $decision = Get-GuardianWatchInstanceDecision -Record $existing -Observation $observation `
            -ExpectedSid $UserSid -ExpectedTaskName $TaskName -ExpectedScriptPath $GuardianPayloadScriptPath `
            -IncomingScriptSha256 ([string]$PayloadIdentity.deployed_script_sha256)
        if ($decision.action -eq "CURRENT") { throw "guardian_watch_instance_already_running" }
        if ($decision.action -eq "STOP") { throw "guardian_stale_watch_instance_not_reaped" }
        if ($decision.action -eq "REFUSE") { throw ("guardian_watch_instance_record_untrusted: " + $decision.reason) }
        Remove-Item -LiteralPath $GuardianWatchInstancePath -Force -ErrorAction SilentlyContinue
    }
    $record = New-GuardianWatchInstanceRecord -PayloadIdentity $PayloadIdentity -StartedUtc $StartedUtc -ProcessId $PID
    Write-AtomicJson -Path $GuardianWatchInstancePath -Value $record
    return $record
}

function Remove-GuardianWatchInstanceRecord {
    param([AllowNull()][object]$Record)
    if ($null -eq $Record) { return }
    try { $current = Read-GuardianWatchInstanceRecord } catch { return }
    if ($null -eq $current) { return }
    if ([int]$current.pid -ne [int]$Record.pid) { return }
    if (-not (Test-GuardianProcessStartIdentity -ExpectedUtcTicks ([long]$Record.process_start_utc_ticks) -ObservedUtcTicks ([long]$current.process_start_utc_ticks))) { return }
    Remove-Item -LiteralPath $GuardianWatchInstancePath -Force -ErrorAction SilentlyContinue
}

function Invoke-GuardianWatchInstanceOwner {
    <#
      One owner for the payload swap.  The record written by the running
      watch names the exact instance and the bytes it loaded; when those
      bytes differ from the incoming payload, this stops that instance before
      the new payload starts.  A legacy instance (started before the record
      existed) is matched by its exact sealed command line and owner SID.  A
      running task with no provable instance is a refusal, never a silent
      pass.
    #>
    param(
        [Parameter(Mandatory = $true)][ValidateSet("install", "activate", "ensure")][string]$Operation,
        [Parameter(Mandatory = $true)][string]$IncomingScriptSha256,
        [Parameter(Mandatory = $true)][string]$IncomingModuleSha256,
        [Parameter(Mandatory = $true)][string]$IncomingLauncherSha256,
        [Parameter(Mandatory = $true)][bool]$PayloadChanged,
        [AllowNull()][object]$ScriptWriteUtc = $null,
        [AllowNull()][System.Collections.IDictionary]$Transaction = $null
    )
    $record = Read-GuardianWatchInstanceRecord
    if ($null -ne $record) {
        $observation = Get-GuardianProcessObservation -ProcessId ([int]$record.pid)
        $decision = Get-GuardianWatchInstanceDecision -Record $record -Observation $observation `
            -ExpectedSid $UserSid -ExpectedTaskName $TaskName -ExpectedScriptPath $GuardianPayloadScriptPath `
            -IncomingScriptSha256 $IncomingScriptSha256
        switch ($decision.action) {
            "CURRENT" { return [ordered]@{ action = "KEEP"; reason = $decision.reason; pid = [int]$record.pid } }
            "CLEAR" {
                Remove-Item -LiteralPath $GuardianWatchInstancePath -Force -ErrorAction SilentlyContinue
                return [ordered]@{ action = "CLEARED"; reason = $decision.reason }
            }
            "STOP" {
                if ($null -eq $observation -or -not [bool]$observation.alive) {
                    Remove-Item -LiteralPath $GuardianWatchInstancePath -Force -ErrorAction SilentlyContinue
                    return [ordered]@{ action = "CLEARED"; reason = "recorded_instance_not_running" }
                }
                $outcome = Stop-GuardianWatchInstance -ProcessId ([int]$record.pid) -StartUtcTicks ([long]$record.process_start_utc_ticks)
                if (-not [bool]$outcome.stopped) { throw ("guardian_watch_instance_stop_failed: " + $outcome.reason) }
                if ($null -ne $Transaction) { $Transaction["prior_watch_instance_stopped"] = $true }
                Remove-Item -LiteralPath $GuardianWatchInstancePath -Force -ErrorAction SilentlyContinue
                return [ordered]@{ action = "STOPPED"; reason = $decision.reason; pid = [int]$record.pid }
            }
            default { throw ("guardian_watch_instance_owner_refused: " + $decision.reason) }
        }
    }
    # No record: a legacy instance, or one that died without cleaning up.
    $candidates = @(Get-GuardianWatchInstanceProcesses)
    $selection = Get-GuardianWatchInstanceMatch -Processes $candidates -ScriptPath $GuardianPayloadScriptPath -Sid $UserSid -DistroName $Distro
    if (@($selection.unprovable).Count -gt 0) {
        # A PowerShell process whose command line or owner cannot be read
        # might be the watch.  Stopping the provable matches and leaving this
        # one behind would make the swap look complete while the prior bytes
        # keep running.
        throw ("guardian_prior_watch_instance_unproven: " + ((@($selection.unprovable) | ForEach-Object { [string]$_.process_id }) -join ","))
    }
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $taskRunning = $null -ne $task -and [string]$task.State -eq "Running"
    $stale = @()
    foreach ($process in @($selection.matches)) {
        if ($PayloadChanged) { $stale += $process; continue }
        if ($null -ne $ScriptWriteUtc -and $null -ne $process.creation_date_utc -and
            $process.creation_date_utc -lt ([DateTime]$ScriptWriteUtc).ToUniversalTime()) {
            $stale += $process
        }
    }
    if ($stale.Count -eq 0) {
        if ($taskRunning -and @($selection.matches).Count -eq 0) {
            # Task Scheduler may report Running for a moment after the process
            # exits. Re-read once before calling the instance unprovable.
            Start-Sleep -Milliseconds 500
            $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
            $taskRunning = $null -ne $task -and [string]$task.State -eq "Running"
            if ($taskRunning) { throw "guardian_prior_watch_instance_unproven" }
        }
        if (@($selection.matches).Count -gt 0) {
            return [ordered]@{ action = "KEEP"; reason = "legacy_instance_runs_the_current_payload"; pids = @($selection.matches | ForEach-Object { [int]$_.process_id }) }
        }
        return [ordered]@{ action = "NONE"; reason = "no_prior_instance_found" }
    }
    $stopped = @()
    # Children first: the launcher only waits, so a killed launcher would
    # leave the PowerShell process that holds the old script in memory.
    foreach ($process in @($stale | Where-Object { $_.name -ieq "powershell.exe" })) {
        if ($null -eq $process.creation_date_utc) { throw ("guardian_prior_watch_instance_unproven: pid " + [string]$process.process_id) }
        $outcome = Stop-GuardianWatchInstance -ProcessId ([int]$process.process_id) -StartUtcTicks ([long]$process.creation_date_utc.Ticks)
        if (-not [bool]$outcome.stopped) { throw ("guardian_prior_watch_instance_stop_failed: " + $outcome.reason) }
        if ($null -ne $Transaction) { $Transaction["prior_watch_instance_stopped"] = $true }
        $stopped += [int]$process.process_id
    }
    Start-Sleep -Milliseconds 250
    foreach ($process in @($stale | Where-Object { $_.name -ieq "ramshared-host-launch.exe" })) {
        if ($null -eq $process.creation_date_utc) { throw ("guardian_prior_watch_instance_unproven: pid " + [string]$process.process_id) }
        if ($null -eq (Get-Process -Id ([int]$process.process_id) -ErrorAction SilentlyContinue)) { continue }
        $outcome = Stop-GuardianWatchInstance -ProcessId ([int]$process.process_id) -StartUtcTicks ([long]$process.creation_date_utc.Ticks)
        if (-not [bool]$outcome.stopped) { throw ("guardian_prior_watch_instance_stop_failed: " + $outcome.reason) }
        if ($null -ne $Transaction) { $Transaction["prior_watch_instance_stopped"] = $true }
        $stopped += [int]$process.process_id
    }
    Remove-Item -LiteralPath $GuardianWatchInstancePath -Force -ErrorAction SilentlyContinue
    return [ordered]@{ action = "STOPPED"; reason = "legacy_instance_matched_the_sealed_command_line"; pids = @($stopped) }
}

function Wait-GuardianTaskNotRunning {
    param([int]$TimeoutSeconds = 5)
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ($true) {
        $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        if ($null -eq $task -or [string]$task.State -ne "Running") { return $true }
        if ([DateTime]::UtcNow -ge $deadline) { return $false }
        Start-Sleep -Milliseconds 200
    }
}

function Invoke-BoundedProcess {
    param([Parameter(Mandatory = $true)][string]$FileName, [Parameter(Mandatory = $true)][string]$Arguments, [Parameter(Mandatory = $true)][int]$TimeoutSeconds)
    # Every external call is bounded and never throws. A process that cannot
    # be created, started, or read is the same class of outcome as a timeout:
    # the external call did not run to a verdict. It returns the bounded
    # failure shape with reason "spawn_failed" so the caller can classify it;
    # the termination decision already refuses to act on an incomplete probe.
    # Throwing here instead was how a transient capacity failure on a loaded
    # host ended the producer and left the product down with a stale proof
    # (observed 2026-10-05). Construction sits inside the same boundary
    # because New-Object fails under the same pressure.
    $process = $null
    try {
        $start = New-Object System.Diagnostics.ProcessStartInfo
        $start.FileName = $FileName
        $start.Arguments = $Arguments
        $start.UseShellExecute = $false
        $start.CreateNoWindow = $true
        $start.RedirectStandardOutput = $true
        $start.RedirectStandardError = $true
        $process = New-Object System.Diagnostics.Process
        $process.StartInfo = $start
        if (-not $process.Start()) { return [ordered]@{ completed = $false; exit_code = $null; stdout = ""; stderr = ""; reason = "spawn_failed"; process_tree_terminated = $false } }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $completed = $process.WaitForExit($TimeoutSeconds * 1000)
        $processTreeTerminated = $false
        $reason = "success"
        if (-not $completed) {
            $stopped = Stop-GuardianProcessInstanceSafely -Process $process
            if ($stopped.stopped -and $process.WaitForExit(5000)) {
                $processTreeTerminated = $true
                $reason = "timeout"
            } else {
                $reason = "timeout_process_instance_uncontained:" + $stopped.reason
            }
        }
        $streamsDrained = $false
        try {
            $streamsDrained = [Threading.Tasks.Task]::WaitAll(
                [Threading.Tasks.Task[]]@($stdoutTask, $stderrTask), 5000)
        } catch {
            $reason = "stream_drain_failed"
        }
        if (-not $streamsDrained -and $reason -eq "success") {
            $reason = "stream_drain_timeout"
        }
        if ($completed -and $streamsDrained) {
            $reason = if ($process.ExitCode -eq 0) { "success" } else { "nonzero_exit" }
        }
        # A faulted read task still counts as "completed" to WaitAll, so
        # reading .Result must not be able to throw out of a bounded call.
        $stdoutText = ""
        $stderrText = ""
        if ($streamsDrained) {
            try { $stdoutText = Limit-GuardianDiagnostics $stdoutTask.Result } catch { $stdoutText = "" }
            try { $stderrText = Limit-GuardianDiagnostics $stderrTask.Result } catch { $stderrText = "" }
        }
        return [ordered]@{
            completed = [bool]($completed -and $streamsDrained)
            exit_code = if ($completed -and $streamsDrained) { [int]$process.ExitCode } else { $null }
            stdout = $stdoutText
            stderr = $stderrText
            reason = $reason
            process_tree_terminated = [bool]$processTreeTerminated
        }
    } catch {
        return [ordered]@{ completed = $false; exit_code = $null; stdout = ""; stderr = ""; reason = "spawn_failed"; process_tree_terminated = $false }
    } finally {
        if ($null -ne $process) { $process.Dispose() }
    }
}

function Invoke-BoundedJsonQuery {
    param(
        [Parameter(Mandatory = $true)][string]$Query,
        [Parameter(Mandatory = $true)][int]$TimeoutSeconds
    )
    $powerShellPath = Join-Path $PSHOME "powershell.exe"
    if (-not (Test-Path -LiteralPath $powerShellPath -PathType Leaf)) {
        return [ordered]@{ completed = $false; data = $null; reason = "query_host_unavailable" }
    }
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes(
            '$ErrorActionPreference = "Stop"; ' + $Query))
    $child = Invoke-BoundedProcess -FileName $powerShellPath `
        -Arguments ("-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand " + $encoded) `
        -TimeoutSeconds $TimeoutSeconds
    if (-not $child.completed) {
        return [ordered]@{ completed = $false; data = $null; reason = "query_deadline_exceeded"; child = $child }
    }
    if ($child.exit_code -ne 0) {
        return [ordered]@{ completed = $false; data = $null; reason = "query_failed"; child = $child }
    }
    try {
        return [ordered]@{ completed = $true; data = ($child.stdout | ConvertFrom-Json -ErrorAction Stop); reason = "complete"; child = $child }
    } catch {
        return [ordered]@{ completed = $false; data = $null; reason = "query_output_invalid"; child = $child }
    }
}

function Get-HeartbeatState {
    $item = Get-Item -LiteralPath $HeartbeatPath -ErrorAction SilentlyContinue
    $age = if ($null -eq $item) { [double]::PositiveInfinity } else { ([DateTime]::UtcNow - $item.LastWriteTimeUtc).TotalSeconds }
    return [ordered]@{ stale = ($age -ge $StaleAfterSec); age_seconds = $age }
}

function Get-GuardianWslCommandPrefix {
    # Distro is constrained by the top-level parameter validation.  Passing it
    # without quotes avoids preserving literal quotes through ProcessStartInfo.
    return ("-d " + $Distro + " -u root --")
}

function Get-GuardianBootProbeSummary {
    param([AllowNull()][object]$Probe)
    if ($null -eq $Probe) {
        return [pscustomobject]@{ completed = $false; exit_code = $null; reason = "missing" }
    }
    return [pscustomobject]@{
        completed = [bool]$Probe.completed
        exit_code = if ($null -eq $Probe.exit_code) { $null } else { [int]$Probe.exit_code }
        reason = [string]$Probe.reason
    }
}

function Get-GuardianBootProbeEventData {
    param([AllowNull()][object]$Probe)
    $summary = Get-GuardianBootProbeSummary -Probe $Probe
    return @{
        probe_completed = [bool]$summary.completed
        probe_exit_code = $summary.exit_code
        probe_reason = [string]$summary.reason
    }
}

function Invoke-GuestProbe {
    $first = Invoke-BoundedProcess -FileName "wsl.exe" `
        -Arguments ((Get-GuardianWslCommandPrefix) + " /bin/true") -TimeoutSeconds $GuestCommandTimeoutSec
    $second = Invoke-BoundedProcess -FileName "wsl.exe" `
        -Arguments ((Get-GuardianWslCommandPrefix) + " cat /proc/sys/kernel/random/boot_id") -TimeoutSeconds $GuestCommandTimeoutSec
    $probes = @(
        [ordered]@{ name = "guest_process_probe"; result = $first },
        [ordered]@{ name = "guest_boot_identity_probe"; result = $second }
    )
    $result = [ordered]@{ probes = $probes; first = $first; second = $second }
    $result.failure_count = Get-GuestProbeFailureCount -GuestProbe $result
    $result.failed = ($result.failure_count -eq 2)
    return $result
}

function Get-GuestProbeFailureCount {
    param([Parameter(Mandatory = $true)][object]$GuestProbe)
    try { $probes = @($GuestProbe.probes) } catch { return 0 }
    if ($probes.Count -ne 2) { return 0 }
    $names = @($probes | ForEach-Object { [string]$_.name })
    if (($names | Select-Object -Unique).Count -ne 2 -or $names -contains "") { return 0 }
    $failed = 0
    foreach ($probe in $probes) {
        $result = $probe.result
        if ($null -eq $result -or -not [bool]$result.completed -or $result.exit_code -ne 0) { $failed++ }
    }
    return $failed
}

function Test-HcsServiceRunning {
    param([AllowNull()][object]$Status)
    if ($null -eq $Status) { return $false }

    if ($Status -is [string]) {
        $textStatus = $Status.Trim()
        if ($textStatus -ieq "Running") { return $true }
        $numericStatus = 0
        if ([int]::TryParse($textStatus, [ref]$numericStatus)) {
            return $numericStatus -eq $HcsServiceRunningValue
        }
        return $false
    }

    $statusType = $Status.GetType()
    if ($statusType.IsEnum -or $Status -is [byte] -or $Status -is [sbyte] -or
        $Status -is [int16] -or $Status -is [uint16] -or $Status -is [int32] -or
        $Status -is [uint32] -or $Status -is [int64] -or $Status -is [uint64]) {
        try { return ([long]$Status -eq $HcsServiceRunningValue) } catch { return $false }
    }
    return $false
}

function Test-GuardianStagingDefinitionDisabled {
    param([AllowNull()][object]$Task)
    # Staging is a property of the registration, not of a runtime projection.
    # Disable-ScheduledTask flips the definition to disabled; a prior guardian
    # instance that is still finishing keeps State at Running.  Reading State
    # here refused a correctly staged task whenever a previous watch was still
    # exiting, which is the normal case at an attended reinstall.
    if ($null -eq $Task) { return $false }
    $settingsProperty = $Task.PSObject.Properties['Settings']
    if ($null -eq $settingsProperty) { return $false }
    $settings = $settingsProperty.Value
    if ($null -eq $settings) { return $false }
    $enabledProperty = $settings.PSObject.Properties['Enabled']
    if ($null -eq $enabledProperty) { return $false }
    return (-not [bool]$enabledProperty.Value)
}

function Test-GuardianStagingRegistrationShape {
    param([AllowNull()][object]$Task)
    # Reinstall is not authority to overwrite a registration an operator
    # changed.  It is a retry over the product's own staging output, so a
    # registration that is already exactly the staged shape is idempotent
    # under reinstall.  Everything observable that the staged task pins is
    # checked: the sealed arguments, both trigger classes, StartWhenAvailable,
    # the battery policy, and the disabled definition.  A divergent registration
    # is never adopted.
    if ($null -eq $Task) { return $false }
    if (-not (Test-GuardianStagingDefinitionDisabled -Task $Task)) { return $false }
    $actionsProperty = $Task.PSObject.Properties['Actions']
    $triggersProperty = $Task.PSObject.Properties['Triggers']
    $settingsProperty = $Task.PSObject.Properties['Settings']
    if ($null -eq $actionsProperty -or $null -eq $triggersProperty -or $null -eq $settingsProperty) { return $false }
    $actions = @($actionsProperty.Value)
    $triggers = @($triggersProperty.Value)
    $settings = $settingsProperty.Value
    if ($actions.Count -ne 1) { return $false }
    $executeProperty = $actions[0].PSObject.Properties['Execute']
    $argumentsProperty = $actions[0].PSObject.Properties['Arguments']
    if ($null -eq $executeProperty -or $null -eq $argumentsProperty) { return $false }
    if ([string]$executeProperty.Value -cne $HostLauncherPath) { return $false }
    if ([string]$argumentsProperty.Value -cne (Get-SealedGuardianTaskArguments)) { return $false }
    $triggerClasses = @($triggers | ForEach-Object {
        $cimClassProperty = $_.PSObject.Properties['CimClass']
        if ($null -eq $cimClassProperty -or $null -eq $cimClassProperty.Value) { return }
        $nameProperty = $cimClassProperty.Value.PSObject.Properties['CimClassName']
        if ($null -eq $nameProperty) { return }
        [string]$nameProperty.Value
    })
    if ($triggerClasses.Count -ne 2) { return $false }
    if ($triggerClasses -notcontains 'MSFT_TaskLogonTrigger' -or $triggerClasses -notcontains 'MSFT_TaskBootTrigger') { return $false }
    $startWhenAvailableProperty = $settings.PSObject.Properties['StartWhenAvailable']
    if ($null -eq $startWhenAvailableProperty -or -not [bool]$startWhenAvailableProperty.Value) { return $false }
    # Battery policy must be the product's, not Task Scheduler's default. The
    # cmdlet switch names are the positive form; the serialized properties are
    # the negative form. A registration carrying the defaults is a definition
    # that can be stopped by a power-source change and never restarted, so it
    # is never adopted.
    $disallowStartOnBatteryProperty = $settings.PSObject.Properties['DisallowStartIfOnBatteries']
    $stopOnBatteryProperty = $settings.PSObject.Properties['StopIfGoingOnBatteries']
    if ($null -eq $disallowStartOnBatteryProperty -or [bool]$disallowStartOnBatteryProperty.Value) { return $false }
    if ($null -eq $stopOnBatteryProperty -or [bool]$stopOnBatteryProperty.Value) { return $false }
    return $true
}

function Invoke-IndependentHostProbe {
    $wsl = Invoke-BoundedProcess -FileName "wsl.exe" -Arguments "--status" -TimeoutSeconds $GuestCommandTimeoutSec
    $hcs = Invoke-BoundedJsonQuery -Query $HcsServiceStatusQuery -TimeoutSeconds $GuestCommandTimeoutSec
    $hcsFailed = (-not $hcs.completed) -or $null -eq $hcs.data -or -not (Test-HcsServiceRunning -Status $hcs.data.Status)
    $wslFailed = (-not $wsl.completed) -or $wsl.exit_code -ne 0
    return [ordered]@{ failed = ($wslFailed -and $hcsFailed); wsl_failed = $wslFailed; hcs_failed = $hcsFailed; wsl = $wsl; hcs = $hcs; hcs_status = if ($null -eq $hcs.data) { "missing" } else { [string]$hcs.data.Status } }
}

function Invoke-HostSnapshot {
    param([Parameter(Mandatory = $true)][string]$RunDirectory)
    $vmcompute = Invoke-BoundedJsonQuery -Query $HcsServiceStatusQuery -TimeoutSeconds $GuestCommandTimeoutSec
    $snapshot = [ordered]@{ timestamp_utc = [DateTime]::UtcNow.ToString("o"); heartbeat = Get-HeartbeatState; vmcompute = $vmcompute; closed = [bool]$vmcompute.completed }
    Write-AtomicJson -Path (Join-Path $RunDirectory "host-snapshot.json") -Value $snapshot
    return $snapshot
}

function Invoke-GuardianTelemetryStep {
    param([hashtable]$Context, [string]$Path)
    try {
        if ($null -ne $Context.observer) {
            $result = Receive-SharedWslTelemetry -Observer $Context.observer
            if ($result.state -eq 'PENDING') { return }
            $Context.observer = $null
        }
        $Context.observer = New-SharedWslTelemetryObserver -TelemetryPath $Path
    } catch {
        # Diagnostic failure cannot end the independent heartbeat producer.
        $Context.last_error = 'diagnostic_collection_or_publication_failed'
        if ($null -ne $Context.observer -and $Context.observer.disposed) {
            $Context.observer = $null
        }
    }
}

function Get-SafeModePath { return Join-Path $SafeModeRoot ($Distro + ".json") }
function Get-TerminationPath { return Join-Path $GuardianStateRoot ($Distro + ".termination.json") }

function Complete-TerminationRecord {
    param([Parameter(Mandatory = $true)][string]$IncidentId, [Parameter(Mandatory = $true)][string]$NewBootId)
    $active = Get-TerminationPath
    $history = Join-Path $GuardianStateRoot ($Distro + "." + $IncidentId + ".termination.json")
    Write-AtomicJson -Path $history -Value ([ordered]@{ incident_id = $IncidentId; distro = $Distro; new_boot_id = $NewBootId; completed_utc = [DateTime]::UtcNow.ToString("o") })
    Remove-Item -LiteralPath $active -Force
}

function Write-HostSafeModeGate {
    param([Parameter(Mandatory = $true)][string]$IncidentId, [Parameter(Mandatory = $true)][string]$PriorBootId)
    $gate = [ordered]@{ schema_version = 1; distro = $Distro; incident_id = $IncidentId; prior_boot_id = $PriorBootId; reason = "guardian_proven_guest_inaccessible"; timestamp_utc = [DateTime]::UtcNow.ToString("o") }
    Write-AtomicJson -Path (Get-SafeModePath) -Value $gate
    return $gate
}

function Write-SealedGuardianConfig {
    $config = [ordered]@{
        schema_version = 2
        distro = $Distro
        user_sid = $UserSid
        task_name = $TaskName
        guardian_policy = (Get-SealedGuardianPolicy)
    }
    Write-AtomicJson -Path $GuardianConfigPath -Value $config
}

function Test-SealedGuardianIdentity {
    if (-not (Test-Path -LiteralPath $GuardianConfigPath -PathType Leaf)) { throw "guardian sealed configuration is missing" }
    $config = Get-Content -Raw -LiteralPath $GuardianConfigPath | ConvertFrom-Json
    $expectedKeys = @("schema_version", "distro", "user_sid", "task_name", "guardian_policy")
    $actualKeys = @($config.PSObject.Properties.Name | Sort-Object)
    if (($actualKeys -join "`n") -cne (@($expectedKeys | Sort-Object) -join "`n") -or
        $config.schema_version -ne 2 -or $config.distro -cne $Distro -or $config.user_sid -cne $UserSid -or $config.task_name -cne $TaskName -or
        $null -eq $config.guardian_policy -or -not (Test-SealedGuardianPolicy -Policy $config.guardian_policy)) {
        throw "guardian invocation does not match the sealed distro, user SID, and policy"
    }
}

function New-GuardianInstallTransaction {
    $transactionId = [Guid]::NewGuid().ToString("N")
    $transaction = [ordered]@{
        task_snapshot_path = Join-Path $TaskBackupRoot ($TaskName + "." + $transactionId + ".rollback.xml")
        config_snapshot_path = Join-Path $TaskBackupRoot ($TaskName + "." + $transactionId + ".rollback.config")
        had_task = $false
        had_config = $false
        task_may_be_modified = $false
        config_may_be_modified = $false
        task_seal_snapshot_path = Join-Path $TaskBackupRoot ($TaskName + "." + $transactionId + ".rollback.seal.json")
        had_task_seal = $false
        task_seal_may_be_modified = $false
        payload_script_snapshot_path = Join-Path $TaskBackupRoot ($TaskName + "." + $transactionId + ".rollback.payload.ps1")
        payload_module_snapshot_path = Join-Path $TaskBackupRoot ($TaskName + "." + $transactionId + ".rollback.payload.psm1")
        payload_seal_snapshot_path = Join-Path $TaskBackupRoot ($TaskName + "." + $transactionId + ".rollback.payload.seal.json")
        launcher_snapshot_path = Join-Path $TaskBackupRoot ($TaskName + "." + $transactionId + ".rollback.host-launch.exe")
        had_payload_script = $false
        had_payload_module = $false
        had_payload_seal = $false
        had_launcher = $false
        payload_may_be_modified = $false
        prior_watch_instance_stopped = $false
        prior_task_enabled = $false
        owned_task_xml_fingerprint = $null
        phase = "begin"
    }
    $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($null -ne $existing) {
        $transaction.prior_task_enabled = -not (Test-GuardianStagingDefinitionDisabled -Task $existing)
        $xml = Export-ScheduledTask -TaskName $TaskName
        if ([string]::IsNullOrWhiteSpace($xml)) { throw "guardian existing task backup is empty" }
        [IO.File]::WriteAllText($transaction.task_snapshot_path, [string]$xml, [Text.Encoding]::UTF8)
        $transaction.had_task = $true
    }
    if (Test-Path -LiteralPath $GuardianConfigPath -PathType Leaf) {
        [IO.File]::WriteAllBytes($transaction.config_snapshot_path, [IO.File]::ReadAllBytes($GuardianConfigPath))
        $transaction.had_config = $true
    }
    if (Test-Path -LiteralPath $GuardianTaskSealPath -PathType Leaf) {
        [IO.File]::WriteAllBytes($transaction.task_seal_snapshot_path, [IO.File]::ReadAllBytes($GuardianTaskSealPath))
        $transaction.had_task_seal = $true
    }
    if (Test-Path -LiteralPath $GuardianPayloadScriptPath -PathType Leaf) {
        [IO.File]::WriteAllBytes($transaction.payload_script_snapshot_path, [IO.File]::ReadAllBytes($GuardianPayloadScriptPath))
        $transaction.had_payload_script = $true
    }
    if (Test-Path -LiteralPath $GuardianPayloadModulePath -PathType Leaf) {
        [IO.File]::WriteAllBytes($transaction.payload_module_snapshot_path, [IO.File]::ReadAllBytes($GuardianPayloadModulePath))
        $transaction.had_payload_module = $true
    }
    if (Test-Path -LiteralPath $GuardianPayloadSealPath -PathType Leaf) {
        [IO.File]::WriteAllBytes($transaction.payload_seal_snapshot_path, [IO.File]::ReadAllBytes($GuardianPayloadSealPath))
        $transaction.had_payload_seal = $true
    }
    if (Test-Path -LiteralPath $HostLauncherPath -PathType Leaf) {
        [IO.File]::WriteAllBytes($transaction.launcher_snapshot_path, [IO.File]::ReadAllBytes($HostLauncherPath))
        $transaction.had_launcher = $true
    }
    return $transaction
}

function Get-GuardianTaskXmlFingerprint {
    param([Parameter(Mandatory = $true)][string]$Xml)
    if ([string]::IsNullOrWhiteSpace($Xml)) { throw "guardian_task_xml_empty" }
    $bytes = [Text.Encoding]::UTF8.GetBytes($Xml.Replace("`r`n", "`n"))
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha256.ComputeHash($bytes)) -replace '-', '') }
    finally { $sha256.Dispose() }
}

function Write-GuardianTaskSeal {
    param([Parameter(Mandatory = $true)][string]$Xml, [Parameter(Mandatory = $true)][string]$Phase)
    Write-AtomicJson -Path $GuardianTaskSealPath -Value ([ordered]@{
        schema_version = 1; task_name = $TaskName; phase = $Phase
        guardian_task_xml_fingerprint = Get-GuardianTaskXmlFingerprint -Xml $Xml
    })
}

function Set-GuardianOwnedTaskMutation {
    param(
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Transaction,
        [Parameter(Mandatory = $true)][string]$Xml,
        [Parameter(Mandatory = $true)][string]$Phase
    )
    # Every owned mutation of the registration records the XML it produced,
    # before the next operation can fail.  Rollback proves ownership against
    # this fingerprint: a mutation that is never recorded reads as an operator
    # edit, and the restore is refused over the transaction's own work.
    $Transaction.task_may_be_modified = $true
    $Transaction.task_seal_may_be_modified = $true
    $Transaction.owned_task_xml_fingerprint = Get-GuardianTaskXmlFingerprint -Xml $Xml
    Write-GuardianTaskSeal -Xml $Xml -Phase $Phase
}

function Assert-GuardianTaskXmlSeal {
    if (-not (Test-Path -LiteralPath $GuardianTaskSealPath -PathType Leaf)) { throw "guardian_task_xml_seal_missing" }
    try { $seal = Get-Content -Raw -LiteralPath $GuardianTaskSealPath | ConvertFrom-Json -ErrorAction Stop }
    catch { throw "guardian_task_xml_seal_invalid" }
    if ($seal.schema_version -ne 1 -or $seal.task_name -cne $TaskName -or
        [string]$seal.guardian_task_xml_fingerprint -notmatch '^[0-9A-F]{64}$') { throw "guardian_task_xml_seal_invalid" }
    $actual = Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    if ((Get-GuardianTaskXmlFingerprint -Xml $actual) -cne [string]$seal.guardian_task_xml_fingerprint) {
        throw "guardian_task_operator_edit_detected"
    }
}

function Save-GuardianOriginalBackup {
    param([Parameter(Mandatory = $true)][System.Collections.IDictionary]$Transaction)
    if (-not (Test-Path -LiteralPath $GuardianTaskBackupPath -PathType Leaf) -and -not (Test-Path -LiteralPath $GuardianTaskAbsentBackupPath -PathType Leaf)) {
        if ($Transaction.had_task) {
            Copy-Item -LiteralPath $Transaction.task_snapshot_path -Destination $GuardianTaskBackupPath -ErrorAction Stop
        } else {
            [IO.File]::WriteAllText($GuardianTaskAbsentBackupPath, "absent`n", [Text.Encoding]::UTF8)
        }
    }
    if (-not (Test-Path -LiteralPath $GuardianConfigBackupPath -PathType Leaf) -and -not (Test-Path -LiteralPath $GuardianConfigAbsentBackupPath -PathType Leaf)) {
        if ($Transaction.had_config) {
            Copy-Item -LiteralPath $Transaction.config_snapshot_path -Destination $GuardianConfigBackupPath -ErrorAction Stop
        } else {
            [IO.File]::WriteAllText($GuardianConfigAbsentBackupPath, "absent`n", [Text.Encoding]::UTF8)
        }
    }
}

function Restore-GuardianTaskSnapshot {
    param([Parameter(Mandatory = $true)][System.Collections.IDictionary]$Transaction)
    $current = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($null -ne $current) { Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction Stop }
    if ($Transaction.had_task) {
        $xml = Get-Content -Raw -LiteralPath $Transaction.task_snapshot_path
        Register-ScheduledTask -TaskName $TaskName -Xml $xml -Force | Out-Null
    }
}

function Rollback-GuardianInstallTransaction {
    param([Parameter(Mandatory = $true)][System.Collections.IDictionary]$Transaction)
    $rollbackErrors = @()
    $taskRestored = -not $Transaction.task_may_be_modified
    if ($Transaction.task_may_be_modified) {
        try {
            if ($null -eq $Transaction.owned_task_xml_fingerprint) { throw "guardian_task_rollback_owner_unknown" }
            $currentXml = Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop
            if ((Get-GuardianTaskXmlFingerprint -Xml $currentXml) -cne [string]$Transaction.owned_task_xml_fingerprint) { throw "guardian_task_operator_edit_detected" }
            Restore-GuardianTaskSnapshot -Transaction $Transaction
            $taskRestored = $true
        } catch { $rollbackErrors += $_.Exception.Message }
    }
    if ($Transaction.task_seal_may_be_modified) {
        try {
            if ($taskRestored) {
                if ($Transaction.had_task_seal) {
                    Move-GuardianFileReplacing -Source $Transaction.task_seal_snapshot_path -Destination $GuardianTaskSealPath
                } elseif (Test-Path -LiteralPath $GuardianTaskSealPath -PathType Leaf) {
                    Remove-Item -LiteralPath $GuardianTaskSealPath -Force
                }
            } else {
                # The registration was not restored.  The seal must describe
                # the registration that is there, never one that is not: a seal
                # naming an absent task locks every later attended action out
                # behind guardian_task_operator_edit_detected, and the product
                # would have no way back in over its own half-applied work.
                $xml = Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop
                Write-GuardianTaskSeal -Xml $xml -Phase "guardian_task_rollback_failed_reconciliation_required"
            }
        } catch { $rollbackErrors += $_.Exception.Message }
    }
    if ($Transaction.config_may_be_modified) {
        try {
            if ($Transaction.had_config) {
                Move-GuardianFileReplacing -Source $Transaction.config_snapshot_path -Destination $GuardianConfigPath
            } elseif (Test-Path -LiteralPath $GuardianConfigPath -PathType Leaf) {
                Remove-Item -LiteralPath $GuardianConfigPath -Force
            }
        } catch { $rollbackErrors += $_.Exception.Message }
    }
    # Payload last: it was deployed first, so it is undone last.
    if ($Transaction.payload_may_be_modified) {
        try { Restore-GuardianPayloadSnapshot -Transaction $Transaction }
        catch { $rollbackErrors += $_.Exception.Message }
    }
    # A failed install must not be the reason the host lost its watch.  The
    # stop owner ran before the payload swap; when the transaction unwinds,
    # restore the prior registration's enabled state as well as its bytes.
    if ($Transaction.Contains("prior_watch_instance_stopped") -and [bool]$Transaction["prior_watch_instance_stopped"] -and
        $Transaction.Contains("prior_task_enabled") -and [bool]$Transaction["prior_task_enabled"]) {
        try {
            if (-not $taskRestored) { throw "guardian_task_rollback_owner_unknown" }
            Start-ScheduledTask -TaskName $TaskName -ErrorAction Stop
        } catch { $rollbackErrors += ("guardian prior watch could not be restarted: " + $_.Exception.Message) }
    }
    if ($rollbackErrors.Count -gt 0) { throw ("guardian install rollback incomplete: " + ($rollbackErrors -join "; ")) }
}

function Complete-GuardianInstallTransaction {
    param([Parameter(Mandatory = $true)][System.Collections.IDictionary]$Transaction)
    foreach ($path in @($Transaction.task_snapshot_path, $Transaction.config_snapshot_path, $Transaction.task_seal_snapshot_path, $Transaction.payload_script_snapshot_path, $Transaction.payload_module_snapshot_path, $Transaction.payload_seal_snapshot_path, $Transaction.launcher_snapshot_path)) {
        if (Test-Path -LiteralPath $path -PathType Leaf) { Remove-Item -LiteralPath $path -Force }
    }
}

function Invoke-GuardianInstallTransaction {
    param([Parameter(Mandatory = $true)][hashtable]$Operations)
    $begin = $Operations["begin"]
    if ($null -eq $begin) { throw "guardian install transaction has no begin operation" }
    $transaction = & $begin
    try {
        foreach ($phase in @("backup", "stop_prior", "deploy", "config", "register", "disable", "verify_disabled")) {
            $transaction.phase = $phase
            $operation = $Operations[$phase]
            if ($null -eq $operation) { throw "guardian install transaction has no $phase operation" }
            & $operation $transaction
        }
        $transaction.phase = "complete"
        $complete = $Operations["complete"]
        if ($null -ne $complete) { & $complete $transaction }
        return $transaction
    } catch {
        $failure = $_
        $rollback = $Operations["rollback"]
        if ($null -eq $rollback) { throw "guardian install transaction has no rollback operation" }
        try { & $rollback $transaction } catch {
            $rollbackFailure = $_.Exception.Message
            throw ("guardian install transaction failed at " + $transaction.phase + ": " + $failure.Exception.Message + "; rollback incomplete: " + $rollbackFailure)
        }
        throw ("guardian install transaction failed at " + $transaction.phase + ": " + $failure.Exception.Message)
    }
}

function New-GuardianInstallOperations {
    return @{
        begin = { New-GuardianInstallTransaction }
        backup = { param($transaction) Save-GuardianOriginalBackup -Transaction $transaction }
        stop_prior = {
            param($transaction)
            # The incoming bytes are not deployed yet.  A prior instance that
            # loaded different bytes would keep the old script in memory after
            # the deploy, which is the payload-swap defect this phase owns.
            $sourceScriptSha256 = Get-GuardianFileSha256 -Path (Join-Path $PSScriptRoot $GuardianPayloadScriptName)
            $sourceModuleSha256 = Get-GuardianFileSha256 -Path (Join-Path $PSScriptRoot $GuardianPayloadModuleName)
            $sourceLauncherSha256 = Get-GuardianFileSha256 -Path $HostLauncherSourcePath
            $deployed = Get-GuardianPayloadIdentity
            $payloadChanged = ($null -eq $deployed.deployed_script_sha256) -or
                ([string]$deployed.deployed_script_sha256 -cne $sourceScriptSha256) -or
                ([string]$deployed.deployed_module_sha256 -cne $sourceModuleSha256) -or
                ([string]$deployed.deployed_launcher_sha256 -cne $sourceLauncherSha256)
            $transaction["prior_payload_swap"] = $payloadChanged
            Invoke-GuardianWatchInstanceOwner -Operation "install" `
                -IncomingScriptSha256 $sourceScriptSha256 `
                -IncomingModuleSha256 $sourceModuleSha256 `
                -IncomingLauncherSha256 $sourceLauncherSha256 `
                -PayloadChanged $payloadChanged `
                -Transaction $transaction | Out-Null
        }
        deploy = { param($transaction) Install-GuardianPayload -Transaction $transaction }
        config = { param($transaction) $transaction.config_may_be_modified = $true; Write-SealedGuardianConfig }
        register = {
            param($transaction)
            Register-ScheduledTask -TaskName $TaskName -InputObject (New-GuardianTask) -Force | Out-Null
            # A successful registration is already a task mutation.  Seal the
            # exact XML before the next operation can fail so rollback can
            # prove ownership rather than guessing after a disable failure.
            $xml = Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop
            Set-GuardianOwnedTaskMutation -Transaction $transaction -Xml $xml -Phase "guardian_task_registration_seal_pending_disable"
        }
        disable = {
            param($transaction)
            Disable-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
            # Disable mutates the definition.  Record that mutation as owned
            # before the next operation can fail, or the rollback compares the
            # disabled registration to the register-time fingerprint, reads its
            # own disable as guardian_task_operator_edit_detected, and refuses
            # to restore the prior registration.
            $xml = Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop
            Set-GuardianOwnedTaskMutation -Transaction $transaction -Xml $xml -Phase "guardian_task_disable_seal_pending_verify"
        }
        verify_disabled = {
            param($transaction)
            $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
            if (-not (Test-GuardianStagingDefinitionDisabled -Task $task)) { throw "guardian staging task was not disabled" }
            $xml = Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop
            Set-GuardianOwnedTaskMutation -Transaction $transaction -Xml $xml -Phase "staging_disabled"
        }
        complete = { param($transaction) Complete-GuardianInstallTransaction -Transaction $transaction }
        rollback = { param($transaction) Rollback-GuardianInstallTransaction -Transaction $transaction }
    }
}

function Test-GuardianProcessElevated {
    # Observation only: never throws. The register phase runs at Highest run
    # level and the deploy phase writes into the product host payload root,
    # so both need an elevated token.
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Assert-GuardianProcessElevated {
    param([Parameter(Mandatory = $true)][string]$Operation)
    if (-not (Test-GuardianProcessElevated)) {
        throw ("guardian_" + $Operation + "_requires_elevation")
    }
}

function Install-GuardianTaskTransaction {
    # Authority before effects. A non-elevated attempt used to mutate through
    # backup, deploy and config and only then discover at register that it
    # could not create the task -- leaving a half-applied transaction for a
    # rollback that lacks the same authority and fails the same way.
    Assert-GuardianProcessElevated -Operation "install"
    New-Item -ItemType Directory -Force -Path $TaskBackupRoot, $SafeModeRoot, $GuardianStateRoot | Out-Null
    # Clear the product's own transients before the backup snapshot, so a
    # leaked .replaced/.tmp from an earlier call cannot be mistaken for state
    # and cannot accumulate across attended reinstalls.
    Clear-GuardianTransientResidue -Roots @($ProgramDataRoot, $GuardianPayloadRoot, $GuardianStateRoot) | Out-Null
    $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($null -ne $existing) {
        # Reinstall is not authority to overwrite a registration an operator
        # changed.  It is a retry over the product's own staging output, so a
        # registration that is already exactly the staged shape is idempotent
        # under reinstall and needs no seal to adopt.
        if (-not (Test-GuardianStagingRegistrationShape -Task $existing)) {
            Assert-GuardianTaskXmlSeal
        }
    }
    Invoke-GuardianInstallTransaction -Operations (New-GuardianInstallOperations) | Out-Null
}

function Restore-SealedGuardianBackup {
    $restored = $false
    Assert-GuardianTaskXmlSeal
    if (Test-Path -LiteralPath $GuardianTaskBackupPath -PathType Leaf) {
        $xml = Get-Content -Raw -LiteralPath $GuardianTaskBackupPath
        $current = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
        if ($null -ne $current) { Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction Stop }
        Register-ScheduledTask -TaskName $TaskName -Xml $xml -Force | Out-Null
        $restored = $true
    } elseif (Test-Path -LiteralPath $GuardianTaskAbsentBackupPath -PathType Leaf) {
        # Do not delete the seal/config state until task removal is proven.
        # A scheduler/provider failure leaves the currently sealed task and
        # forensic state intact for a later attended reconciliation.
        try {
            Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction Stop
        } catch {
            throw "guardian_uninstall_unregister_failed: $($_.Exception.Message)"
        }
        if ($null -ne (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue)) {
            throw "guardian_uninstall_unregister_failed: task_still_present"
        }
        $restored = $true
    }
    if (Test-Path -LiteralPath $GuardianConfigBackupPath -PathType Leaf) {
        Copy-Item -LiteralPath $GuardianConfigBackupPath -Destination $GuardianConfigPath -Force -ErrorAction Stop
        $restored = $true
    } elseif (Test-Path -LiteralPath $GuardianConfigAbsentBackupPath -PathType Leaf) {
        Remove-Item -LiteralPath $GuardianConfigPath -Force -ErrorAction SilentlyContinue
        $restored = $true
    }
    if ($restored -and (Test-Path -LiteralPath $GuardianTaskSealPath -PathType Leaf)) {
        Remove-Item -LiteralPath $GuardianTaskSealPath -Force -ErrorAction Stop
    }
    return $restored
}

function Get-GuestBootId {
    $boot = Invoke-BoundedProcess -FileName "wsl.exe" -Arguments ((Get-GuardianWslCommandPrefix) + " cat /proc/sys/kernel/random/boot_id") -TimeoutSeconds $GuestCommandTimeoutSec
    $script:LastGuestBootProbe = Get-GuardianBootProbeSummary -Probe $boot
    $candidate = if ($null -eq $boot.stdout) { $null } else { $boot.stdout.Trim().ToLowerInvariant() }
    if (-not $boot.completed -or $boot.exit_code -ne 0 -or -not (Test-CanonicalGuestBootId -BootId $candidate)) { return $null }
    return $candidate
}

function Test-GuestDistroRunning {
    <#
      DT-5: the guardian must never launch `wsl.exe` as a guest command. Any
      `wsl.exe -d <distro> ...` invocation starts a stopped distro, which is
      how a deliberate `wsl.exe --shutdown` used to be undone by the stale
      heartbeat probe (the heartbeat stays fresh for StaleAfterSec, then the
      guest probe boots the VM again). `--list --running` only enumerates
      running distros and never boots one.
    #>
    $result = Invoke-BoundedProcess -FileName "wsl.exe" -Arguments "--list --running --quiet" -TimeoutSeconds $GuestCommandTimeoutSec
    if (-not $result.completed -or $result.exit_code -ne 0 -or $null -eq $result.stdout) { return $false }
    # wsl.exe writes UTF-16LE; strip NULs so an ASCII distro name matches
    # under both the PowerShell 5.1 and PowerShell 7 decoders.
    $text = ($result.stdout -replace "`0", "") -replace "`r", ""
    foreach ($line in ($text -split "`n")) {
        if ($line.Trim() -ieq $Distro) { return $true }
    }
    return $false
}

function Get-GuardianWatchGate {
    <#
      The loop must decide whether the guest is even running before it reads a
      boot identity or probes the guest. A stopped distro is a deliberate
      operator state, not a watchdog incident; no guest command may run.
    #>
    param(
        [Parameter(Mandatory = $true)][bool]$HeartbeatStale,
        [Parameter(Mandatory = $true)][bool]$GuestRunning
    )
    if (-not $GuestRunning) { return [ordered]@{ action = "WAIT"; reason = "guest_not_running" } }
    if ($HeartbeatStale) { return [ordered]@{ action = "PROBE"; reason = "heartbeat_stale" } }
    return [ordered]@{ action = "PROBE"; reason = "heartbeat_fresh" }
}

function Mirror-GuestSafeMode {
    param([Parameter(Mandatory = $true)][string]$IncidentId, [Parameter(Mandatory = $true)][string]$BootId)
    $payload = [ordered]@{ schema_version = 1; incident_id = $IncidentId; distro = $Distro; boot_id = $BootId } | ConvertTo-Json -Compress
    $encoded = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($payload))
    $command = "install -d -m 0700 /var/lib/ramshared; printf '%s' '$encoded' | base64 -d > /var/lib/ramshared/.safe-mode.tmp; chmod 0600 /var/lib/ramshared/.safe-mode.tmp; mv /var/lib/ramshared/.safe-mode.tmp /var/lib/ramshared/safe-mode.json"
    return Invoke-BoundedProcess -FileName "wsl.exe" -Arguments ((Get-GuardianWslCommandPrefix) + " sh -c `"$command`"") -TimeoutSeconds $GuestCommandTimeoutSec
}

function Invoke-TargetedTerminate {
    param([Parameter(Mandatory = $true)][string]$IncidentId, [Parameter(Mandatory = $true)][string]$PriorBootId)
    $terminationPath = Get-TerminationPath
    if (Test-Path -LiteralPath $terminationPath -PathType Leaf) { return [ordered]@{ started = $false; completed = $false; reason = "termination_already_recorded" } }
    Write-AtomicJson -Path $terminationPath -Value ([ordered]@{ incident_id = $IncidentId; distro = $Distro; prior_boot_id = $PriorBootId; started_utc = [DateTime]::UtcNow.ToString("o") })
    $result = Invoke-BoundedProcess -FileName "wsl.exe" -Arguments ("--terminate " + $Distro) -TimeoutSeconds $GuestCommandTimeoutSec
    if (-not $result.completed) { return [ordered]@{ started = $true; completed = $false; reason = "terminate_timeout"; detail = $result } }
    return [ordered]@{ started = $true; completed = ($result.exit_code -eq 0); reason = $result.reason; detail = $result }
}

function Get-GuardianTerminationDecision {
    param(
        [Parameter(Mandatory = $true)][bool]$HeartbeatStale,
        [Parameter(Mandatory = $true)][object]$GuestProbe,
        [Parameter(Mandatory = $true)][object]$HostProbe,
        [Parameter(Mandatory = $true)][object]$Snapshot,
        [Parameter(Mandatory = $true)][bool]$TerminationRecorded
    )
    if (-not $HeartbeatStale) { return [ordered]@{ action = "WAIT"; reason = "heartbeat_fresh" } }
    if ((Get-GuestProbeFailureCount -GuestProbe $GuestProbe) -ne 2) {
        return [ordered]@{ action = "REFUSE"; reason = "two_distinct_guest_probe_failures_required" }
    }
    if (-not [bool]$HostProbe.failed -or -not [bool]$HostProbe.wsl_failed -or -not [bool]$HostProbe.hcs_failed) {
        return [ordered]@{ action = "REFUSE"; reason = "dual_wsl_hcs_corroboration_required" }
    }
    if (-not [bool]$Snapshot.closed) { return [ordered]@{ action = "REFUSE"; reason = "snapshot_open" } }
    if ($TerminationRecorded) { return [ordered]@{ action = "REFUSE"; reason = "termination_already_recorded" } }
    return [ordered]@{ action = "TERMINATE"; reason = "all_guardian_gates_proven" }
}

function Get-SealedGuardianTaskArguments {
    param([switch]$ActivationAuthorized)
    # The product-owned deployed path, never $PSScriptRoot.  Every caller —
    # install, activate, status, the staging shape test — must agree on one
    # authority, and that authority cannot be a property of who is asking.
    $scriptPath = $GuardianPayloadScriptPath
    # DT-12: the launcher is the action program; its arguments name the
    # absolute PowerShell, the no-deadline bound, and then the sealed payload.
    # `--` keeps every following token a child argument the launcher never
    # re-parses as its own option.
    $arguments = '--timeout-ms {0} -- "{1}" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{2}" -Action watch -Run -Distro "{3}" -UserSid "{4}" -HeartbeatPath "{5}" -ArtifactRoot "{6}" -StaleAfterSec {7} -PollSec {8} -GuestCommandTimeoutSec {9}' -f $GuardianLauncherTimeoutMs, $WindowsPowerShellPath, $scriptPath, $Distro, $UserSid, $HeartbeatPath, $ArtifactRoot, $StaleAfterSec, $PollSec, $GuestCommandTimeoutSec
    if ($ActivationAuthorized) { $arguments += (' -ApproveGuardianAction "{0}"' -f $GuardianActionApproval) }
    return $arguments
}

function New-GuardianTask {
    param([switch]$ActivationAuthorized)
    $taskArguments = Get-SealedGuardianTaskArguments -ActivationAuthorized:$ActivationAuthorized
    # DT-12: the action names the sealed launcher and the absolute PowerShell.
    # The launcher owns the console decision; `PowerShell.exe` through PATH is
    # a hijack surface and an unsealed identity.
    $taskAction = New-ScheduledTaskAction -Execute $HostLauncherPath -Argument $taskArguments
    # DT-17: both trigger classes. A single -AtLogOn cannot fire when Windows
    # stayed up and only the guest restarted — the observed case. -AtStartup
    # covers the Windows reboot class, and StartWhenAvailable covers a trigger
    # that was missed while the machine was off or the account was not on.
    $taskTrigger = @(
        (New-ScheduledTaskTrigger -AtLogOn -User $TaskUserName),
        (New-ScheduledTaskTrigger -AtStartup)
    )
    # Task Scheduler serializes the zero duration as PT0S (no execution limit).
    # Battery policy is explicit, not inherited: the Task Scheduler defaults are
    # DisallowStartIfOnBatteries and StopIfGoingOnBatteries, so on a machine on
    # battery the guardian would be stopped and, because a battery stop is a
    # clean stop, RestartOnFailure would never bring it back. The producer is a
    # safety monitor; the host's power source is not a reason to stop watching.
    $taskSettings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds 0) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    $taskPrincipal = New-ScheduledTaskPrincipal -UserId $TaskUserName -LogonType Interactive -RunLevel Highest
    return New-ScheduledTask -Action $taskAction -Trigger $taskTrigger -Settings $taskSettings -Principal $taskPrincipal
}

function Activate-GuardianTask {
    # Authority before effects, same reason as the install transaction: the
    # activation re-registers at Highest run level, and discovering that only
    # after the mutation has started is what turns a refused activation into a
    # half-applied one.
    Assert-GuardianProcessElevated -Operation "activation"
    Test-SealedGuardianIdentity
    Assert-GuardianPayloadIdentity
    if ($ApproveGuardianActivation -cne $GuardianActivationApproval) { throw "guardian activation requires exact attended approval" }
    Assert-GuardianTaskXmlSeal
    # Activate may be the first attended action after a manual payload drop.
    # Stop any instance that loaded bytes older than the deployed payload
    # before the registration is re-sealed around them.
    $activationIdentity = Get-GuardianPayloadIdentity
    $activationScriptWriteUtc = (Get-Item -LiteralPath $GuardianPayloadScriptPath -ErrorAction Stop).LastWriteTimeUtc
    Invoke-GuardianWatchInstanceOwner -Operation "activate" `
        -IncomingScriptSha256 ([string]$activationIdentity.deployed_script_sha256) `
        -IncomingModuleSha256 ([string]$activationIdentity.deployed_module_sha256) `
        -IncomingLauncherSha256 ([string]$activationIdentity.deployed_launcher_sha256) `
        -PayloadChanged $false `
        -ScriptWriteUtc $activationScriptWriteUtc | Out-Null
    $current = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    # Staging is a property of the registration, and the definition predicate
    # is the only authority for it.  Reading State here refused every attended
    # reinstall whose previous watch was still exiting -- the normal case --
    # and is the same trap Test-GuardianStagingDefinitionDisabled documents.
    if (-not (Test-GuardianStagingDefinitionDisabled -Task $current)) { throw "guardian activation requires disabled staging state" }
    $priorXml = Export-ScheduledTask -TaskName $TaskName
    $activationOwnedFingerprint = $null
    $activationMayHaveModifiedTask = $false
    $activationRegistrationCompleted = $false
    try {
        $activationMayHaveModifiedTask = $true
        Register-ScheduledTask -TaskName $TaskName -InputObject (New-GuardianTask -ActivationAuthorized) -Force | Out-Null
        $activationRegistrationCompleted = $true
        $activationOwnedFingerprint = Get-GuardianTaskXmlFingerprint -Xml (Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop)
        Enable-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
        $verified = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
        if (Test-GuardianStagingDefinitionDisabled -Task $verified) { throw "guardian activation did not enable the scheduled task" }
        $activatedXml = Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop
        $activationOwnedFingerprint = Get-GuardianTaskXmlFingerprint -Xml $activatedXml
        Write-GuardianTaskSeal -Xml $activatedXml -Phase "activated"
    } catch {
        $failure = $_
        try {
            if (-not $activationMayHaveModifiedTask -or -not $activationRegistrationCompleted) { throw "guardian activation mutation ownership is unknown" }
            if ($null -ne $activationOwnedFingerprint) {
                $currentXml = Export-ScheduledTask -TaskName $TaskName -ErrorAction Stop
                if ((Get-GuardianTaskXmlFingerprint -Xml $currentXml) -cne $activationOwnedFingerprint) { throw "guardian_task_operator_edit_detected" }
            }
            Register-ScheduledTask -TaskName $TaskName -Xml $priorXml -Force | Out-Null
            Write-GuardianTaskSeal -Xml $priorXml -Phase "staging_disabled"
        } catch { throw ("guardian activation failed and rollback was incomplete: " + $_.Exception.Message) }
        throw ("guardian activation failed; disabled staging restored: " + $failure.Exception.Message)
    }
}

function Get-GuardianEnsureDecision {
    # Pure decision (DT-19): given what Task Scheduler reports about the sealed
    # named task, what may the guest boot path do?  Kept free of filesystem and
    # of side effects so the manufactured cases can drive every branch without
    # touching the host, the same shape as Get-GuardianTerminationDecision.
    param([AllowNull()][object]$Task)
    if ($null -eq $Task) {
        # Install owns registration.  Creating the task here would let a guest
        # boot path register something the install transaction never sealed.
        return [ordered]@{ action = "REFUSE"; reason = "task_absent" }
    }
    if (Test-GuardianStagingDefinitionDisabled -Task $Task) {
        # A disabled definition is either pre-activation staging or an attended
        # stop.  Both are operator authority; a boot path may not override it.
        return [ordered]@{ action = "REFUSE"; reason = "task_definition_disabled" }
    }
    $stateProperty = $Task.PSObject.Properties['State']
    $state = if ($null -ne $stateProperty) { [string]$stateProperty.Value } else { "" }
    if ($state -eq "Running") {
        # Idempotence.  The registration is MultipleInstances IgnoreNew, so a
        # second start is refused by Task Scheduler anyway; reporting success
        # is what makes a warm boot a true no-op.
        return [ordered]@{ action = "NOOP"; reason = "already_running"; state = $state }
    }
    return [ordered]@{ action = "START"; reason = "producer_not_running"; state = $state }
}

function Ensure-GuardianTaskRunning {
    # DT-19.  The guest starts the sealed named producer before its gate
    # demands a proof, and never writes a proof itself -- this is a start, not
    # a registration and not a verification.  Authority before effects: the
    # sealed identity, the payload bytes, and an elevated token are checked
    # before Task Scheduler is touched.  Proof freshness stays the host-gate's
    # job (DT-18): preparation never outranks verification.
    Assert-GuardianProcessElevated -Operation "ensure"
    Test-SealedGuardianIdentity
    Assert-GuardianPayloadIdentity
    # The guest boot path is also a payload-swap owner: a watch started before
    # the deployed bytes landed would otherwise make the task look Running and
    # the decision a NOOP.  Stop the stale instance, then let the sealed task
    # start the current bytes.
    $ensureIdentity = Get-GuardianPayloadIdentity
    $ensureScriptWriteUtc = (Get-Item -LiteralPath $GuardianPayloadScriptPath -ErrorAction Stop).LastWriteTimeUtc
    $instanceOwner = Invoke-GuardianWatchInstanceOwner -Operation "ensure" `
        -IncomingScriptSha256 ([string]$ensureIdentity.deployed_script_sha256) `
        -IncomingModuleSha256 ([string]$ensureIdentity.deployed_module_sha256) `
        -IncomingLauncherSha256 ([string]$ensureIdentity.deployed_launcher_sha256) `
        -PayloadChanged $false `
        -ScriptWriteUtc $ensureScriptWriteUtc
    if ($instanceOwner.action -eq "STOPPED" -or $instanceOwner.action -eq "CLEARED") {
        if (-not (Wait-GuardianTaskNotRunning -TimeoutSeconds 5)) { throw "guardian_prior_instance_did_not_release_the_task" }
    }
    $current = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $decision = Get-GuardianEnsureDecision -Task $current
    if ($decision.action -eq "REFUSE") {
        if ($decision.reason -eq "task_absent") { throw "guardian task is not registered; ensure starts the producer and never creates it" }
        throw "guardian task definition is disabled; ensure never enables a task"
    }
    if ($decision.action -eq "NOOP") {
        Write-Output "state=running"
        return
    }
    Start-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    Write-Output "state=started"
}

function Invoke-GuardianWatchIteration {
    <#
      This is the production watch ordering made injectable for source-only
      regression fixtures.  It never invokes termination itself: callers may
      reach that action only after this returns TERMINATE from fresh evidence.
    #>
    param([Parameter(Mandatory = $true)][hashtable]$Operations)
    foreach ($name in @("get_heartbeat", "guest_probe", "host_probe", "snapshot", "termination_recorded")) {
        if ($null -eq $Operations[$name]) { throw "guardian_watch_operation_missing:$name" }
    }
    $heartbeat = & $Operations.get_heartbeat
    if (-not [bool]$heartbeat.stale) {
        return [ordered]@{ action = "WAIT"; reason = "heartbeat_fresh"; heartbeat = $heartbeat }
    }
    $firstGuest = & $Operations.guest_probe
    if ((Get-GuestProbeFailureCount -GuestProbe $firstGuest) -ne 2) {
        return [ordered]@{ action = "REFUSE"; reason = "two_distinct_guest_probe_failures_required"; heartbeat = $heartbeat; guest = $firstGuest }
    }
    # Re-probe after the first pair failed. A recovered distro is not a
    # termination candidate even when host-side evidence remains unhealthy.
    $revalidatedGuest = & $Operations.guest_probe
    if ((Get-GuestProbeFailureCount -GuestProbe $revalidatedGuest) -ne 2) {
        return [ordered]@{ action = "REFUSE"; reason = "guest_recovered_before_termination"; heartbeat = $heartbeat; guest = $firstGuest; revalidated_guest = $revalidatedGuest }
    }
    $hostProbe = & $Operations.host_probe
    if (-not [bool]$hostProbe.failed -or -not [bool]$hostProbe.wsl_failed -or -not [bool]$hostProbe.hcs_failed) {
        return [ordered]@{ action = "REFUSE"; reason = "dual_wsl_hcs_corroboration_required"; heartbeat = $heartbeat; guest = $revalidatedGuest; host = $hostProbe }
    }
    $snapshot = & $Operations.snapshot
    $decision = Get-GuardianTerminationDecision -HeartbeatStale $true -GuestProbe $revalidatedGuest -HostProbe $hostProbe -Snapshot $snapshot -TerminationRecorded ([bool](& $Operations.termination_recorded))
    return [ordered]@{ action = $decision.action; reason = $decision.reason; heartbeat = $heartbeat; guest = $revalidatedGuest; host = $hostProbe; snapshot = $snapshot }
}

function Invoke-GuardianWatchTerminationTail {
    <# The executable tail of Invoke-GuardianWatch. It takes a sealed watch
       iteration and rechecks guest liveness immediately before any safe-mode
       write or targeted terminate. #>
    param(
        [Parameter(Mandatory = $true)][object]$WatchIteration,
        [Parameter(Mandatory = $true)][hashtable]$Operations
    )
    foreach ($name in @("guest_probe", "get_boot_id", "write_safe_mode_gate", "targeted_terminate", "mirror_safe_mode", "complete_record", "publish", "write_event")) {
        if ($null -eq $Operations[$name]) { throw "guardian_tail_operation_missing:$name" }
    }
    if ($WatchIteration.action -ne "TERMINATE") { return [ordered]@{ action = "REFUSE"; reason = "guardian_tail_not_authorized" } }
    $tailGuest = & $Operations.guest_probe
    if ((Get-GuestProbeFailureCount -GuestProbe $tailGuest) -ne 2) {
        & $Operations.write_event "guardian_tail_refuses_recovered_guest" $tailGuest
        return [ordered]@{ action = "REFUSE"; reason = "guardian_tail_refuses_recovered_guest"; guest = $tailGuest }
    }
    $priorBootId = & $Operations.get_boot_id
    if ($null -eq $priorBootId) {
        & $Operations.publish "BLOCKED" "prior_boot_id_not_proven" $null
        & $Operations.write_event "prior_boot_id_not_proven" @{}
        return [ordered]@{ action = "REFUSE"; reason = "prior_boot_id_not_proven" }
    }
    $incidentId = [Guid]::NewGuid().ToString("N")
    $gate = & $Operations.write_safe_mode_gate $incidentId $priorBootId
    & $Operations.write_event "host_safe_mode_gate_written" $gate
    $termination = & $Operations.targeted_terminate $incidentId $priorBootId
    & $Operations.write_event "targeted_terminate" $termination
    if (-not $termination.completed) {
        & $Operations.publish "BLOCKED" $termination.reason $null
        return [ordered]@{ action = "ERROR"; reason = $termination.reason }
    }
    $newBootId = & $Operations.get_boot_id
    if ($null -eq $newBootId -or $newBootId -eq $priorBootId) {
        & $Operations.publish "BLOCKED" "new_boot_id_not_proven" $null
        & $Operations.write_event "new_boot_id_not_proven" @{ prior_boot_id = $priorBootId; new_boot_id = $newBootId }
        return [ordered]@{ action = "ERROR"; reason = "new_boot_id_not_proven" }
    }
    $mirror = & $Operations.mirror_safe_mode $incidentId $newBootId
    & $Operations.write_event "guest_safe_mode_mirrored" $mirror
    if (-not $mirror.completed -or $mirror.exit_code -ne 0) {
        & $Operations.publish "BLOCKED" "guest_safe_mode_mirror_failed" $null
        return [ordered]@{ action = "ERROR"; reason = "guest_safe_mode_mirror_failed" }
    }
    & $Operations.complete_record $incidentId $newBootId
    & $Operations.publish "SAFE_MODE" "recovery_waiting_for_operator_resume" $newBootId
    & $Operations.write_event "guardian_recovery_safe_mode" @{ incident_id = $incidentId; new_boot_id = $newBootId }
    return [ordered]@{ action = "SAFE_MODE"; reason = "recovery_waiting_for_operator_resume"; incident_id = $incidentId }
}

function Invoke-GuardianWatch {
    # Authority before effect: refuse to act as a copy the install transaction
    # did not deploy and seal.  A stale or foreign copy must never reach the
    # termination gates.
    Assert-GuardianPayloadIdentity
    if (-not (Test-AbsoluteWindowsPath -Path $ArtifactRoot)) { throw "ArtifactRoot must be an absolute Windows path" }
    $consoleLifetime = Protect-GuardianConsoleLifetime
    if ($consoleLifetime -eq "unset") { throw "guardian console lifetime could not be taken out of the child control-event group" }
    Test-SealedGuardianIdentity
    Assert-GuardianTaskXmlSeal
    New-Item -ItemType Directory -Force -Path $ArtifactRoot | Out-Null
    $runDirectory = Join-Path $ArtifactRoot ("ramshared-guardian-" + (Get-Date -Format "yyyyMMdd-HHmmss"))
    New-Item -ItemType Directory -Path $runDirectory | Out-Null
    $eventPath = Join-Path $runDirectory "guardian-events.jsonl"
    $telemetryPath = Join-Path $ArtifactRoot "windows-telemetry.jsonl"
    Write-GuardianEvent -Path $eventPath -Event "guardian_started" -Data @{ heartbeat = $HeartbeatPath; stale_after_seconds = $StaleAfterSec }
    $telemetryContext = @{ observer = $null }
    # Publish this exact process instance and the payload bytes it loaded.  A
    # duplicate current instance refuses to start; a live instance of older
    # bytes is left for the install/activate/ensure owner to stop.
    $watchIdentity = Get-GuardianPayloadIdentity
    $watchStartedUtc = (Get-Process -Id $PID -ErrorAction Stop).StartTime.ToUniversalTime()
    $watchInstance = Register-GuardianWatchInstance -PayloadIdentity $watchIdentity -StartedUtc $watchStartedUtc
    try {
    while ($true) {
        # A current heartbeat must be observed before any HEALTHY proof can be
        # published. Never let a boot probe race ahead of stale-heartbeat
        # detection and advertise health during a watchdog incident.
        $heartbeat = Get-HeartbeatState
        # DT-5: ask whether the distro is running without starting it. A stopped
        # distro (an operator `wsl.exe --shutdown`) is not a watchdog incident;
        # probing it with a guest command would boot it again and consume RAM.
        $guestRunning = Test-GuestDistroRunning
        $watchGate = Get-GuardianWatchGate -HeartbeatStale ([bool]$heartbeat.stale) -GuestRunning $guestRunning
        if ($watchGate.action -eq "WAIT") {
            Publish-GuardianState -State "BLOCKED" -Reason $watchGate.reason -BootId $null
            Start-Sleep -Seconds $PollSec
            continue
        }
        if (-not $heartbeat.stale) {
            $publishedBootId = Get-GuestBootId
            if (Test-Path -LiteralPath (Get-SafeModePath) -PathType Leaf) {
                Publish-GuardianState -State "SAFE_MODE" -Reason "host_safe_mode_gate_present" -BootId $publishedBootId
            } elseif ($null -eq $publishedBootId) {
                Publish-GuardianState -State "BLOCKED" -Reason "boot_identity_unavailable" -BootId $null
                Write-GuardianEvent -Path $eventPath -Event "guardian_boot_identity_unavailable" -Data (Get-GuardianBootProbeEventData -Probe $script:LastGuestBootProbe)
            } else {
                Publish-GuardianState -State "HEALTHY" -Reason "watching" -BootId $publishedBootId
            }
            Invoke-GuardianTelemetryStep -Context $telemetryContext -Path $telemetryPath
            Start-Sleep -Seconds $PollSec
            continue
        }
        Invoke-GuardianTelemetryStep -Context $telemetryContext -Path $telemetryPath
        $watchIteration = Invoke-GuardianWatchIteration -Operations @{
            get_heartbeat = { $heartbeat }
            guest_probe = { Invoke-GuestProbe }
            host_probe = { Invoke-IndependentHostProbe }
            snapshot = { Invoke-HostSnapshot -RunDirectory $runDirectory }
            termination_recorded = { Test-Path -LiteralPath (Get-TerminationPath) -PathType Leaf }
        }
        Write-GuardianEvent -Path $eventPath -Event "heartbeat_stale" -Data $heartbeat
        if ($watchIteration.action -ne "TERMINATE") {
            Write-GuardianEvent -Path $eventPath -Event ("guardian_refused_" + $watchIteration.reason) -Data $watchIteration
            Start-Sleep -Seconds $PollSec
            continue
        }
        $tail = Invoke-GuardianWatchTerminationTail -WatchIteration $watchIteration -Operations @{
            guest_probe = { Invoke-GuestProbe }
            get_boot_id = { Get-GuestBootId }
            write_safe_mode_gate = { param($incident, $boot) Write-HostSafeModeGate -IncidentId $incident -PriorBootId $boot }
            targeted_terminate = { param($incident, $boot) Invoke-TargetedTerminate -IncidentId $incident -PriorBootId $boot }
            mirror_safe_mode = { param($incident, $boot) Mirror-GuestSafeMode -IncidentId $incident -BootId $boot }
            complete_record = { param($incident, $boot) Complete-TerminationRecord -IncidentId $incident -NewBootId $boot }
            publish = { param($state, $reason, $boot) Publish-GuardianState -State $state -Reason $reason -BootId $boot }
            write_event = { param($event, $data) Write-GuardianEvent -Path $eventPath -Event $event -Data $data }
        }
        if ($tail.action -eq "ERROR") { Write-Error "guardian preserved evidence after terminate failure: $eventPath"; return 2 }
        Start-Sleep -Seconds $PollSec
        continue
    }
    } finally {
        Remove-GuardianWatchInstanceRecord -Record $watchInstance
        Stop-SharedWslTelemetry -Observer $telemetryContext.observer
    }
}

function New-ManufacturedGuardianInstallOperations {
    param(
        [Parameter(Mandatory = $true)][hashtable]$Store,
        [Parameter(Mandatory = $true)][string]$FailPhase
    )
    return @{
        begin = ({
            # Copy, never alias: deploy replaces the payload, and a prior value
            # held by reference would be mutated into the new value before the
            # rollback ever saw it.
            $priorPayload = $null
            if ($null -ne $Store.payload) { $priorPayload = @{ script = $Store.payload.script; module = $Store.payload.module } }
            return [ordered]@{
                store = $Store
                had_task = $null -ne $Store.task
                had_config = $null -ne $Store.config
                prior_task = $Store.task
                prior_config = $Store.config
                prior_payload = $priorPayload
                task_may_be_modified = $false
                config_may_be_modified = $false
                payload_may_be_modified = $false
                prior_watch_instance_stopped = $false
                prior_task_enabled = ($null -ne $Store.task -and [bool]$Store.task.enabled)
                phase = "begin"
            }
        }).GetNewClosure()
        backup = ({
            param($transaction)
            $store = $transaction.store
            if ($null -eq $store.task_backup) { $store.task_backup = "captured-task-backup" }
            if ($null -eq $store.config_backup) { $store.config_backup = "captured-config-backup" }
            if ($FailPhase -eq "backup") { throw "injected backup failure" }
        }).GetNewClosure()
        stop_prior = ({
            param($transaction)
            if ($FailPhase -eq "stop_prior") { throw "injected stop_prior failure" }
        }).GetNewClosure()
        deploy = ({
            param($transaction)
            # Deploy is a real mutation: the payload may only be rolled back if
            # the deploy phase is the one that marked it.  A fixture that skips
            # the mark never exercises Restore-GuardianPayloadSnapshot.  The
            # value is replaced, not edited in place — production copies whole
            # files over the deployed path.
            $transaction.payload_may_be_modified = $true
            $transaction.store.payload = @{ script = "new-guardian-payload"; module = "new-guardian-module" }
            if ($FailPhase -eq "deploy") { throw "injected deploy failure" }
        }).GetNewClosure()
        config = ({
            param($transaction)
            $transaction.config_may_be_modified = $true
            $transaction.store.config = "new-guardian-config"
            if ($FailPhase -eq "config") { throw "injected config failure" }
        }).GetNewClosure()
        register = ({
            param($transaction)
            $transaction.task_may_be_modified = $true
            $transaction.store.task = @{ xml = "new-guardian-task"; enabled = $true }
            # Model production: the registration records the XML it produced so
            # rollback can prove ownership.  A fixture that registers without
            # recording never exercises that proof.
            $transaction.owned_task_xml_fingerprint = "new-guardian-task"
            if ($FailPhase -eq "register") { throw "injected register failure" }
        }).GetNewClosure()
        disable = ({
            param($transaction)
            if ($null -eq $transaction.store.task) { throw "manufactured task was not registered" }
            # Model production: disable mutates the definition to a new XML and
            # records that mutation as owned.  Flipping only a boolean left the
            # recorded identity at the register-time value, which is exactly the
            # shape in which rollback reads its own disable as an operator edit.
            $transaction.store.task = @{ xml = "new-guardian-task-disabled"; enabled = $false }
            $transaction.owned_task_xml_fingerprint = "new-guardian-task-disabled"
            if ($FailPhase -eq "disable") { throw "injected disable failure" }
        }).GetNewClosure()
        verify_disabled = ({
            param($transaction)
            # The staging predicate is the definition, not a runtime projection:
            # a prior instance still finishing keeps State at Running while the
            # registration is already disabled.
            if ($null -eq $transaction.store.task -or $transaction.store.task.enabled) { throw "manufactured task remained enabled" }
            if ($FailPhase -eq "verify_disabled") { throw "injected disabled-state verification failure" }
        }).GetNewClosure()
        complete = { param($transaction) }
        rollback = {
            param($transaction)
            if ($transaction.task_may_be_modified) {
                if ($null -eq $transaction.owned_task_xml_fingerprint -or
                    [string]$transaction.store.task.xml -cne [string]$transaction.owned_task_xml_fingerprint) {
                    throw "guardian_task_operator_edit_detected"
                }
                $transaction.store.task = $transaction.prior_task
            }
            if ($transaction.config_may_be_modified) { $transaction.store.config = $transaction.prior_config }
            # Payload last: deploy runs first, so its restore runs last.
            if ($transaction.payload_may_be_modified) { $transaction.store.payload = $transaction.prior_payload }
        }
    }
}

function Invoke-ManufacturedGuardianInstallTests {
    $phases = @("backup", "stop_prior", "deploy", "config", "register", "disable", "verify_disabled")
    foreach ($phase in $phases) {
        $store = @{
            task = @{ xml = "prior-task"; enabled = $true }
            config = "prior-config"
            payload = @{ script = "prior-payload"; module = "prior-module" }
            task_backup = "original-task-backup"
            config_backup = "original-config-backup"
        }
        $failed = $false
        try { Invoke-GuardianInstallTransaction -Operations (New-ManufacturedGuardianInstallOperations -Store $store -FailPhase $phase) | Out-Null } catch { $failed = $true }
        if (-not $failed) { throw "manufactured guardian install did not fail at $phase" }
        if ($null -eq $store.task -or $store.task.xml -cne "prior-task" -or -not $store.task.enabled -or $store.config -cne "prior-config") { throw "manufactured guardian install did not restore exact prior state after $phase" }
        if ($null -eq $store.payload -or $store.payload.script -cne "prior-payload" -or $store.payload.module -cne "prior-module") { throw "manufactured guardian install did not restore the prior payload after $phase" }
        if ($store.task_backup -cne "original-task-backup" -or $store.config_backup -cne "original-config-backup") { throw "manufactured guardian install overwrote original backup after $phase" }
    }
    foreach ($phase in $phases) {
        $store = @{ task = $null; config = $null; payload = $null; task_backup = "original-task-absence"; config_backup = "original-config-absence" }
        $failed = $false
        try { Invoke-GuardianInstallTransaction -Operations (New-ManufacturedGuardianInstallOperations -Store $store -FailPhase $phase) | Out-Null } catch { $failed = $true }
        if (-not $failed -or $null -ne $store.task -or $null -ne $store.config -or $null -ne $store.payload) { throw "manufactured new guardian staging task survived failed $phase phase" }
        if ($store.task_backup -cne "original-task-absence" -or $store.config_backup -cne "original-config-absence") { throw "manufactured new guardian staging backup changed after $phase" }
    }
    $success = @{ task = $null; config = $null; payload = $null; task_backup = "original-task-absence"; config_backup = "original-config-absence" }
    Invoke-GuardianInstallTransaction -Operations (New-ManufacturedGuardianInstallOperations -Store $success -FailPhase "success") | Out-Null
    if ($null -eq $success.task -or $success.task.enabled -or $success.config -cne "new-guardian-config") { throw "manufactured guardian staging did not finish disabled" }
    if ($null -eq $success.payload -or $success.payload.script -cne "new-guardian-payload") { throw "manufactured guardian staging did not deploy the payload" }
    # The live failure this contract exists for: disable has already mutated the
    # registration and recorded the mutation, then verify_disabled fails.  The
    # rollback must prove ownership against the post-disable identity and
    # restore the exact prior registration.  Reading the register-time
    # fingerprint instead surfaces guardian_task_operator_edit_detected and
    # leaves the half-applied registration in place.
    $verifyFailure = ""
    $verifyStore = @{
        task = @{ xml = "prior-task"; enabled = $true }
        config = "prior-config"
        payload = @{ script = "prior-payload"; module = "prior-module" }
        task_backup = "original-task-backup"
        config_backup = "original-config-backup"
    }
    try { Invoke-GuardianInstallTransaction -Operations (New-ManufacturedGuardianInstallOperations -Store $verifyStore -FailPhase "verify_disabled") | Out-Null } catch { $verifyFailure = $_.Exception.Message }
    if ($verifyFailure -like "*guardian_task_operator_edit_detected*") {
        throw "guardian rollback read its own disable mutation as an operator edit"
    }
    if ($verifyFailure -notlike "*injected disabled-state verification failure*") {
        throw ("guardian verify_disabled failure was not the injected cause: " + $verifyFailure)
    }
    if ($null -eq $verifyStore.task -or $verifyStore.task.xml -cne "prior-task" -or -not $verifyStore.task.enabled -or $verifyStore.config -cne "prior-config") {
        throw "guardian install did not restore the prior registration after a verify_disabled failure"
    }
    if ($null -eq $verifyStore.payload -or $verifyStore.payload.script -cne "prior-payload") {
        throw "guardian install did not restore the prior payload after a verify_disabled failure"
    }
    # Ordering is the defect this phase exists for: a task that points at the
    # product path must not be registered before that path holds the sealed
    # payload.  Prove it by failing register and observing that the payload was
    # already replaced, then rolled back.
    $orderStore = @{
        task = @{ xml = "prior-task"; enabled = $true }
        config = "prior-config"
        payload = @{ script = "prior-payload"; module = "prior-module" }
        task_backup = "original-task-backup"
        config_backup = "original-config-backup"
    }
    $deployBeforeRegister = $false
    try {
        Invoke-GuardianInstallTransaction -Operations (New-ManufacturedGuardianInstallOperations -Store $orderStore -FailPhase "register") | Out-Null
    } catch {
        $deployBeforeRegister = ($orderStore.payload -eq $null -or $orderStore.payload.script -eq "prior-payload") -and ($orderStore.task -eq $null -or $orderStore.task.xml -eq "prior-task")
    }
    if (-not $deployBeforeRegister) { throw "guardian register was not reached only after deploy, or rollback did not unwind both" }
    Write-Output "PASS guardian_install_failure_rolls_back_all_phases"
    Write-Output "PASS guardian_install_preserves_prior_task_config_and_backup"
    Write-Output "PASS guardian_new_task_is_disabled_or_absent_after_failure"
    Write-Output "PASS guardian_disable_mutation_is_owned_across_a_verify_failure"
    Write-Output "PASS guardian_install_deploys_and_restores_the_payload"
    Write-Output "PASS guardian_payload_is_deployed_before_the_task_is_registered"
}

function Invoke-ManufacturedGuardianPayloadIdentityTests {
    # Pure decision cases only.  These must never write the product payload or
    # its seal: -Action test runs on a live host and a manufactured case that
    # mutates installed state is not a test, it is an unattended install.
    # Hashes are full-length hex: a SHA-256 is 64 characters, and a short
    # stand-in must be refused by shape rather than accepted as a match.
    $good = "A" * 64
    $goodModule = "B" * 64
    $goodLauncher = "D" * 64
    $other = "C" * 64
    $seal = [pscustomobject]@{ script_sha256 = $good; module_sha256 = $goodModule; launcher_sha256 = $goodLauncher }
    if (-not (Test-GuardianPayloadIdentityMatch -Seal $seal -SelfScriptSha256 $good -SelfModuleSha256 $goodModule -DeployedScriptSha256 $good -DeployedModuleSha256 $goodModule -LauncherSha256 $goodLauncher)) {
        throw "guardian payload identity refused the exact sealed bytes"
    }
    Write-Output "PASS guardian_watch_accepts_the_sealed_payload_identity"
    $cases = @(
        @{ name = "self_script_superseded"; self_script = $other; self_module = $goodModule; deployed_script = $good; deployed_module = $goodModule; launcher = $goodLauncher },
        @{ name = "self_module_superseded"; self_script = $good; self_module = $other; deployed_script = $good; deployed_module = $goodModule; launcher = $goodLauncher },
        @{ name = "deployed_script_tampered"; self_script = $good; self_module = $goodModule; deployed_script = $other; deployed_module = $goodModule; launcher = $goodLauncher },
        @{ name = "deployed_module_tampered"; self_script = $good; self_module = $goodModule; deployed_script = $good; deployed_module = $other; launcher = $goodLauncher },
        @{ name = "launcher_tampered"; self_script = $good; self_module = $goodModule; deployed_script = $good; deployed_module = $goodModule; launcher = $other },
        @{ name = "launcher_absent"; self_script = $good; self_module = $goodModule; deployed_script = $good; deployed_module = $goodModule; launcher = "" },
        @{ name = "deployed_payload_absent"; self_script = $good; self_module = $goodModule; deployed_script = ""; deployed_module = ""; launcher = $goodLauncher }
    )
    foreach ($case in $cases) {
        if (Test-GuardianPayloadIdentityMatch -Seal $seal `
            -SelfScriptSha256 ([string]$case.self_script) -SelfModuleSha256 ([string]$case.self_module) `
            -DeployedScriptSha256 ([string]$case.deployed_script) -DeployedModuleSha256 ([string]$case.deployed_module) `
            -LauncherSha256 ([string]$case.launcher)) {
            throw ("guardian payload identity accepted " + $case.name)
        }
    }
    Write-Output "PASS guardian_watch_refuses_payload_identity_mismatch"
    $malformed = @(
        [pscustomobject]@{ script_sha256 = "not-a-hash"; module_sha256 = $goodModule; launcher_sha256 = $goodLauncher },
        [pscustomobject]@{ script_sha256 = ("A" * 32); module_sha256 = $goodModule; launcher_sha256 = $goodLauncher },
        [pscustomobject]@{ script_sha256 = $good; module_sha256 = ""; launcher_sha256 = $goodLauncher },
        [pscustomobject]@{ script_sha256 = $good; module_sha256 = $goodModule; launcher_sha256 = "" }
    )
    foreach ($sealCase in $malformed) {
        if (Test-GuardianPayloadIdentityMatch -Seal $sealCase -SelfScriptSha256 $good -SelfModuleSha256 $goodModule -DeployedScriptSha256 $good -DeployedModuleSha256 $goodModule -LauncherSha256 $goodLauncher) {
            throw "guardian payload identity accepted a malformed seal hash"
        }
    }
    Write-Output "PASS guardian_payload_seal_hash_shape_is_enforced"
}

function Invoke-ManufacturedGuardianStagingPredicateTests {
    # The staging predicate reads the definition.  A prior instance that is
    # still finishing keeps the runtime projection at Running while the
    # registration is already disabled.
    $finishingPriorInstance = [pscustomobject]@{
        State = "Running"
        Settings = [pscustomobject]@{ Enabled = $false }
    }
    if (-not (Test-GuardianStagingDefinitionDisabled -Task $finishingPriorInstance)) {
        throw "guardian staging predicate refused a disabled definition under a finishing instance"
    }
    $enabledRegistration = [pscustomobject]@{
        State = "Ready"
        Settings = [pscustomobject]@{ Enabled = $true }
    }
    if (Test-GuardianStagingDefinitionDisabled -Task $enabledRegistration) {
        throw "guardian staging predicate accepted an enabled definition"
    }
    if (Test-GuardianStagingDefinitionDisabled -Task $null) {
        throw "guardian staging predicate accepted an absent registration"
    }
    if (Test-GuardianStagingDefinitionDisabled -Task ([pscustomobject]@{ State = "Disabled" })) {
        throw "guardian staging predicate accepted a registration with no settings"
    }
    Write-Output "PASS guardian_staging_predicate_reads_the_definition_not_the_runtime_state"

    $stagedArguments = Get-SealedGuardianTaskArguments
    # The product's own settings are the positive shape: both trigger classes,
    # StartWhenAvailable, and the battery policy that keeps a power-source
    # change from stopping the producer. The cmdlet switch names are the
    # positive form; the serialized properties are the negative form.
    $productSettings = [pscustomobject]@{
        Enabled = $false
        StartWhenAvailable = $true
        DisallowStartIfOnBatteries = $false
        StopIfGoingOnBatteries = $false
    }
    $stagedRegistration = [pscustomobject]@{
        Settings = $productSettings
        Actions = @([pscustomobject]@{ Execute = $HostLauncherPath; Arguments = $stagedArguments })
        Triggers = @(
            [pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskLogonTrigger' } },
            [pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskBootTrigger' } }
        )
    }
    if (-not (Test-GuardianStagingRegistrationShape -Task $stagedRegistration)) {
        throw "guardian staging shape refused the product's own staged registration"
    }
    $operatorEdited = [pscustomobject]@{
        Settings = $productSettings
        Actions = @([pscustomobject]@{ Execute = $HostLauncherPath; Arguments = ($stagedArguments + " -Distro `"Foreign-Ubuntu`"") })
        Triggers = @(
            [pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskLogonTrigger' } },
            [pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskBootTrigger' } }
        )
    }
    if (Test-GuardianStagingRegistrationShape -Task $operatorEdited) {
        throw "guardian staging shape adopted an operator-edited registration"
    }
    $singleTrigger = [pscustomobject]@{
        Settings = [pscustomobject]@{
            Enabled = $false
            StartWhenAvailable = $false
            DisallowStartIfOnBatteries = $false
            StopIfGoingOnBatteries = $false
        }
        Actions = @([pscustomobject]@{ Execute = $HostLauncherPath; Arguments = $stagedArguments })
        Triggers = @([pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskLogonTrigger' } })
    }
    if (Test-GuardianStagingRegistrationShape -Task $singleTrigger) {
        throw "guardian staging shape adopted a single-trigger registration"
    }
    # Task Scheduler's battery defaults make the guardian stoppable by a power
    # source change, and a battery stop is a clean stop RestartOnFailure never
    # sees. A registration carrying them is never adopted.
    $batteryDefaults = [pscustomobject]@{
        Settings = [pscustomobject]@{
            Enabled = $false
            StartWhenAvailable = $true
            DisallowStartIfOnBatteries = $true
            StopIfGoingOnBatteries = $true
        }
        Actions = @([pscustomobject]@{ Execute = $HostLauncherPath; Arguments = $stagedArguments })
        Triggers = @(
            [pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskLogonTrigger' } },
            [pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskBootTrigger' } }
        )
    }
    if (Test-GuardianStagingRegistrationShape -Task $batteryDefaults) {
        throw "guardian staging shape adopted the Task Scheduler battery defaults"
    }
    Write-Output "PASS guardian_staging_shape_refuses_the_task_scheduler_battery_defaults"
    $activatedRegistration = [pscustomobject]@{
        Settings = [pscustomobject]@{
            Enabled = $true
            StartWhenAvailable = $true
            DisallowStartIfOnBatteries = $false
            StopIfGoingOnBatteries = $false
        }
        Actions = @([pscustomobject]@{ Execute = $HostLauncherPath; Arguments = (Get-SealedGuardianTaskArguments -ActivationAuthorized) })
        Triggers = @(
            [pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskLogonTrigger' } },
            [pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskBootTrigger' } }
        )
    }
    if (Test-GuardianStagingRegistrationShape -Task $activatedRegistration) {
        throw "guardian staging shape adopted an activated registration"
    }
    Write-Output "PASS guardian_staging_shape_adopts_only_the_products_own_staging_output"

    # Pin the production operations themselves: a fixture that is kinder than
    # production is how a green suite measured a failing host.
    $operations = New-GuardianInstallOperations
    if (-not $operations.disable.ToString().Contains("Set-GuardianOwnedTaskMutation")) {
        throw "guardian install disable mutation is not recorded as owned"
    }
    if (-not $operations.register.ToString().Contains("Set-GuardianOwnedTaskMutation")) {
        throw "guardian install register mutation is not recorded as owned"
    }
    $verifyDisabledSource = $operations.verify_disabled.ToString()
    if (-not $verifyDisabledSource.Contains("Test-GuardianStagingDefinitionDisabled")) {
        throw "guardian install verify_disabled does not use the definition predicate"
    }
    if ($verifyDisabledSource.Contains('.State') -or $verifyDisabledSource.Contains('"Disabled"')) {
        throw "guardian install verify_disabled still reads the runtime state"
    }
    Write-Output "PASS guardian_install_operations_record_ownership_and_read_the_definition"

    # Activation has to read the same definition through the same predicate.
    # It used to compare State to "Disabled" directly, which refused every
    # attended reinstall whose previous watch was still exiting -- exactly the
    # case the predicate exists to accept -- and the fixture answered Disabled,
    # so the suite stayed green while the live host could not activate.
    $activateSource = (Get-Command Activate-GuardianTask).Definition
    if (([regex]::Matches($activateSource, "Test-GuardianStagingDefinitionDisabled")).Count -lt 2) {
        throw "guardian activation does not gate both staging and enable on the definition predicate"
    }
    if ($activateSource.Contains('.State') -or $activateSource.Contains('"Disabled"')) {
        throw "guardian activation still reads the runtime state"
    }
    Write-Output "PASS guardian_activation_reads_the_definition_not_the_runtime_state"

    # The replace helper's finally is the only in-call cleanup, so a leaked
    # transient needs a second owner. Install sweeps the roots the product
    # writes before it snapshots anything, or a leaked .replaced accumulates
    # across attended reinstalls and is indistinguishable from state.
    $installSource = (Get-Command Install-GuardianTaskTransaction).Definition
    if (-not $installSource.Contains("Clear-GuardianTransientResidue")) {
        throw "guardian install does not sweep the transients its replace helper mints"
    }
    Write-Output "PASS guardian_install_sweeps_its_own_replace_transients"

    # Rollback must not leave a seal naming a registration that is not there.
    # A seal for an absent task locks every later attended action out behind
    # guardian_task_operator_edit_detected over the product's own half-applied
    # work, which is how a failed install became unrecoverable.
    $rollbackSource = (Get-Command Rollback-GuardianInstallTransaction).Definition
    if (-not $rollbackSource.Contains("guardian_task_rollback_failed_reconciliation_required")) {
        throw "guardian rollback has no path that re-seals the registration it could not restore"
    }
    if (-not $rollbackSource.Contains('$taskRestored')) {
        throw "guardian rollback restores the seal without tracking whether the task was restored"
    }
    Write-Output "PASS guardian_rollback_never_leaves_a_seal_for_an_absent_registration"
}

function Invoke-ManufacturedGuardianWatchIterationTests {
    $failedGuest = [ordered]@{ probes = @(
        [ordered]@{ name = "guest_process_probe"; result = [ordered]@{ completed = $false; exit_code = $null } },
        [ordered]@{ name = "guest_boot_identity_probe"; result = [ordered]@{ completed = $true; exit_code = 1 } }
    ) }
    $recoveredGuest = [ordered]@{ probes = @(
        [ordered]@{ name = "guest_process_probe"; result = [ordered]@{ completed = $true; exit_code = 0 } },
        [ordered]@{ name = "guest_boot_identity_probe"; result = [ordered]@{ completed = $true; exit_code = 0 } }
    ) }
    $failedHost = [pscustomobject]@{ failed = $true; wsl_failed = $true; hcs_failed = $true }
    $closedSnapshot = [pscustomobject]@{ closed = $true }

    $freshState = [ordered]@{ calls = 0 }
    $fresh = Invoke-GuardianWatchIteration -Operations @{
        get_heartbeat = { [ordered]@{ stale = $false; age_seconds = 0 } }
        guest_probe = ({ $freshState.calls++; $failedGuest }).GetNewClosure()
        host_probe = { [pscustomobject]@{ failed = $true; wsl_failed = $true; hcs_failed = $true } }
        snapshot = { [pscustomobject]@{ closed = $true } }
        termination_recorded = { $false }
    }
    if ($fresh.action -ne "WAIT" -or $freshState.calls -ne 0) { throw "guardian watch checked guest before fresh heartbeat" }

    # DT-5: a fresh heartbeat can outlive `wsl.exe --shutdown` by StaleAfterSec.
    # The running check must be reached before the boot probe in both branches,
    # so a deliberately stopped distro is never booted by a guest command.
    $stoppedFresh = Get-GuardianWatchGate -HeartbeatStale $false -GuestRunning $false
    if ($stoppedFresh.action -ne "WAIT" -or $stoppedFresh.reason -cne "guest_not_running") {
        throw ("guardian watch would probe a stopped distro with a fresh heartbeat: " + ($stoppedFresh | ConvertTo-Json -Compress))
    }
    $stoppedStale = Get-GuardianWatchGate -HeartbeatStale $true -GuestRunning $false
    if ($stoppedStale.action -ne "WAIT" -or $stoppedStale.reason -cne "guest_not_running") {
        throw ("guardian watch would probe a stopped distro with a stale heartbeat: " + ($stoppedStale | ConvertTo-Json -Compress))
    }
    $runningStale = Get-GuardianWatchGate -HeartbeatStale $true -GuestRunning $true
    if ($runningStale.action -ne "PROBE" -or $runningStale.reason -cne "heartbeat_stale") {
        throw ("guardian watch refused to probe a running stale guest: " + ($runningStale | ConvertTo-Json -Compress))
    }
    $runningFresh = Get-GuardianWatchGate -HeartbeatStale $false -GuestRunning $true
    if ($runningFresh.action -ne "PROBE" -or $runningFresh.reason -cne "heartbeat_fresh") {
        throw ("guardian watch refused a running fresh guest: " + ($runningFresh | ConvertTo-Json -Compress))
    }
    $watchDefinition = [string](Get-Command Invoke-GuardianWatch).Definition
    $runningCheckIndex = $watchDefinition.IndexOf("Test-GuestDistroRunning")
    $bootProbeIndex = $watchDefinition.IndexOf("Get-GuestBootId")
    $guestProbeIndex = $watchDefinition.IndexOf("Invoke-GuestProbe")
    if ($runningCheckIndex -lt 0 -or $bootProbeIndex -lt 0 -or $guestProbeIndex -lt 0 -or
        $runningCheckIndex -gt $bootProbeIndex -or $runningCheckIndex -gt $guestProbeIndex) {
        throw "guardian watch can reach a guest command before proving the distro is running"
    }
    Write-Output "PASS guardian_watch_never_starts_a_stopped_distro"
    Write-Output "PASS guardian_watch_still_probes_a_running_stale_guest"

    $recoveryState = [ordered]@{ calls = 0 }
    $recovered = Invoke-GuardianWatchIteration -Operations @{
        get_heartbeat = { [ordered]@{ stale = $true; age_seconds = 99 } }
        guest_probe = ({ $recoveryState.calls++; if ($recoveryState.calls -eq 1) { $failedGuest } else { $recoveredGuest } }).GetNewClosure()
        host_probe = { [pscustomobject]@{ failed = $true; wsl_failed = $true; hcs_failed = $true } }
        snapshot = { [pscustomobject]@{ closed = $true } }
        termination_recorded = { $false }
    }
    if ($recovered.action -ne "REFUSE" -or $recovered.reason -ne "guest_recovered_before_termination" -or $recoveryState.calls -ne 2) {
        throw "guardian watch did not revalidate recovered guest"
    }
    $terminalState = [ordered]@{ calls = 0 }
    $proven = Invoke-GuardianWatchIteration -Operations @{
        get_heartbeat = { [ordered]@{ stale = $true; age_seconds = 99 } }
        guest_probe = ({ $terminalState.calls++; $failedGuest }).GetNewClosure()
        host_probe = { [pscustomobject]@{ failed = $true; wsl_failed = $true; hcs_failed = $true } }
        snapshot = { [pscustomobject]@{ closed = $true } }
        termination_recorded = { $false }
    }
    if ($proven.action -ne "TERMINATE" -or $terminalState.calls -ne 2) { throw ("guardian watch cannot reach termination only after current evidence action=" + $proven.action + " reason=" + $proven.reason + " calls=" + $terminalState.calls) }
    Write-Output "PASS guardian_watch_heartbeat_checked_before_healthy_publish"
    Write-Output "PASS guardian_watch_revalidates_guest_recovery_before_termination"
}

function Invoke-ManufacturedGuardianWatchTailTests {
    # Exercise the executable tail rather than merely the decision helper:
    # after a previously proven iteration, any recovered guest must stop the
    # tail before it writes a gate or attempts targeted termination.
    $responsiveGuest = [ordered]@{ probes = @(
        [ordered]@{ name = "guest_process_probe"; result = [ordered]@{ completed = $true; exit_code = 0 } },
        [ordered]@{ name = "guest_boot_identity_probe"; result = [ordered]@{ completed = $true; exit_code = 0 } }
    ) }
    $mutations = [ordered]@{ gate = 0; terminate = 0; events = @() }
    $tail = Invoke-GuardianWatchTerminationTail -WatchIteration ([ordered]@{ action = "TERMINATE"; reason = "fixture_prior_evidence" }) -Operations @{
        guest_probe = { $responsiveGuest }
        get_boot_id = { "01234567-89ab-4def-8123-456789abcdef" }
        write_safe_mode_gate = ({ param($incident, $boot) $mutations.gate++; @{ incident_id = $incident } }).GetNewClosure()
        targeted_terminate = ({ param($incident, $boot) $mutations.terminate++; @{ completed = $true } }).GetNewClosure()
        mirror_safe_mode = { @{ completed = $true; exit_code = 0 } }
        complete_record = { param($incident, $boot) }
        publish = { param($state, $reason, $boot) }
        write_event = ({ param($event, $data) $mutations.events += $event }).GetNewClosure()
    }
    if ($tail.action -ne "REFUSE" -or $tail.reason -ne "guardian_tail_refuses_recovered_guest" -or
        $mutations.gate -ne 0 -or $mutations.terminate -ne 0 -or
        @($mutations.events | Where-Object { $_ -eq "guardian_tail_refuses_recovered_guest" }).Count -ne 1) {
        throw "guardian executable watch tail terminated a recovered guest"
    }
    Write-Output "PASS guardian_tail_refuses_recovered_guest_before_targeted_termination"
}

function Invoke-ManufacturedGuardianPidReuseTests {
    $reusedPidModel = [pscustomobject]@{ HasExited = $false; StartTime = [DateTime]::UtcNow.AddMinutes(-1); Handle = [IntPtr]1; foreign_signal_count = 0 }
    $reusedPidModel | Add-Member -MemberType ScriptMethod -Name Refresh -Value {
        # A new process has reused the observed PID. The production helper must
        # reject rather than send any signal to this foreign instance.
        $this.StartTime = [DateTime]::UtcNow
    }
    $outcome = Stop-GuardianProcessInstanceSafely -Process $reusedPidModel
    if ($outcome.stopped -or $outcome.reason -ne "process_instance_identity_changed" -or $reusedPidModel.foreign_signal_count -ne 0) {
        throw "guardian PID reuse model was not refused before a foreign signal"
    }
    Write-Output "PASS guardian_pid_reuse_never_signals_foreign_process"
}

function Invoke-ManufacturedGuardianBoundedCallTests {
    # A bounded external call never throws. A process that cannot be created
    # is the same class of outcome as a timeout: it did not run to a verdict.
    # Throwing there instead was how a transient capacity failure on a loaded
    # host ended the durable gate (2026-10-05). The probe must come back
    # incomplete, and the termination decision must refuse to act on it.
    $result = Invoke-BoundedProcess -FileName "ramshared-guardian-no-such-probe.exe" -Arguments "--probe" -TimeoutSeconds 1
    if ($null -eq $result) { throw "bounded call on an unstartable file returned no result" }
    if ([bool]$result.completed) { throw "bounded call on an unstartable file reported completion" }
    if ($null -ne $result.exit_code) { throw "bounded call on an unstartable file reported an exit code" }
    if ($result.reason -cne "spawn_failed") { throw ("bounded call on an unstartable file returned reason " + [string]$result.reason) }
    Write-Output "PASS guardian_bounded_call_never_throws_on_an_unstartable_file"
    $failedGuest = [ordered]@{ probes = @(
            [ordered]@{ name = "guest_process_probe"; result = $result },
            [ordered]@{ name = "guest_boot_identity_probe"; result = [ordered]@{ completed = $false; exit_code = $null; reason = "spawn_failed" } }
        ) }
    if ((Get-GuestProbeFailureCount -GuestProbe $failedGuest) -ne 2) {
        throw "an incomplete bounded call was not counted as a failed probe"
    }
    $healthyHost = [ordered]@{ failed = $false; wsl_failed = $false; hcs_failed = $false }
    $decision = Get-GuardianTerminationDecision -HeartbeatStale $true -GuestProbe $failedGuest -HostProbe $healthyHost -Snapshot ([ordered]@{ closed = $true }) -TerminationRecorded $false
    if ($decision.action -cne "REFUSE") {
        throw ("an incomplete bounded call was treated as a termination gate: " + [string]$decision.action)
    }
    Write-Output "PASS guardian_refuses_termination_when_the_probe_did_not_run"
}

function Invoke-ManufacturedGuardianAtomicWriteTests {
    # Move-Item -Force does not replace an existing destination file on
    # Windows. Every atomic write and every snapshot restore lands on a path
    # that may already be there, and the first live install failed exactly
    # there -- including its own rollback. These cases write only into a
    # throwaway temp directory: -Action test runs on a live host and must not
    # touch product state.
    $root = Join-Path ([IO.Path]::GetTempPath()) ("guardian-atomic-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    try {
        $target = Join-Path $root "document.json"
        Write-AtomicJson -Path $target -Value ([ordered]@{ marker = "first" })
        $first = Get-Content -Raw -LiteralPath $target | ConvertFrom-Json
        if ($first.marker -cne "first") { throw "guardian atomic write did not create an absent destination" }
        Write-AtomicJson -Path $target -Value ([ordered]@{ marker = "second" })
        $second = Get-Content -Raw -LiteralPath $target | ConvertFrom-Json
        if ($second.marker -cne "second") { throw "guardian atomic write did not replace an existing destination" }
        if (-not (Test-Path -LiteralPath $target -PathType Leaf)) { throw "guardian atomic write left no destination behind" }
        $leftovers = @(Get-ChildItem -LiteralPath $root -Filter "*.tmp" -ErrorAction SilentlyContinue)
        if ($leftovers.Count -ne 0) { throw "guardian atomic write left a staging file behind" }
        Write-Output "PASS guardian_atomic_write_replaces_an_existing_destination"

        $source = Join-Path $root "snapshot.bin"
        $destination = Join-Path $root "deployed.bin"
        [IO.File]::WriteAllText($source, "snapshot-bytes")
        Move-GuardianFileReplacing -Source $source -Destination $destination
        if (-not (Test-Path -LiteralPath $destination -PathType Leaf) -or (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "guardian file move into an absent destination left the wrong files"
        }
        if (([IO.File]::ReadAllText($destination)) -cne "snapshot-bytes") {
            throw "guardian file move into an absent destination lost the payload"
        }
        [IO.File]::WriteAllText($source, "replacement-bytes")
        Move-GuardianFileReplacing -Source $source -Destination $destination
        if (([IO.File]::ReadAllText($destination)) -cne "replacement-bytes") {
            throw "guardian file move did not replace an existing destination"
        }
        if (Test-Path -LiteralPath $source -PathType Leaf) { throw "guardian file move left its source behind" }
        $stray = @(Get-ChildItem -LiteralPath $root -Filter "*.replaced" -ErrorAction SilentlyContinue)
        if ($stray.Count -ne 0) { throw "guardian file move left its transient replace backup behind" }
        Write-Output "PASS guardian_file_move_replaces_an_existing_destination"

        # The in-call finally is the only cleanup a replace has, so a process
        # killed inside the call -- or a remove Windows briefly refuses --
        # leaves the named transient behind forever. The sweep is the second
        # line of defence and must remove exactly the patterns the helpers
        # mint, and nothing else in the root.
        $leaked = Join-Path $root ("document.json." + [Guid]::NewGuid().ToString("N") + ".replaced")
        $leakedTmp = Join-Path $root ("document.json." + [Guid]::NewGuid().ToString("N") + ".tmp")
        $operatorFile = Join-Path $root "operator-notes.txt"
        [IO.File]::WriteAllText($leaked, "stale-backup")
        [IO.File]::WriteAllText($leakedTmp, "stale-staging")
        [IO.File]::WriteAllText($operatorFile, "operator-state")
        $swept = Clear-GuardianTransientResidue -Roots @($root)
        if ($swept -ne 2) { throw ("guardian transient sweep removed " + [string]$swept + " files instead of the 2 it owns") }
        if ((Test-Path -LiteralPath $leaked -PathType Leaf) -or (Test-Path -LiteralPath $leakedTmp -PathType Leaf)) {
            throw "guardian transient sweep left one of its own patterns behind"
        }
        if (-not (Test-Path -LiteralPath $operatorFile -PathType Leaf)) {
            throw "guardian transient sweep removed a file it does not own"
        }
        $empty = Clear-GuardianTransientResidue -Roots @($root)
        if ($empty -ne 0) { throw "guardian transient sweep reported removals on a clean root" }
        Write-Output "PASS guardian_transient_sweep_removes_only_its_own_patterns"
    } finally {
        if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
    }
}

function Invoke-ManufacturedGuardianAuthorityTests {
    # Authority is checked before effects. A non-elevated install used to
    # mutate through backup, deploy and config and only fail at register, and
    # the rollback then failed for the same reason -- a half-applied
    # transaction left behind by a token that never had the right to start it.
    # These cases shadow only the observation and the effect boundary: nothing
    # here touches the live task, the product payload or the product config.
    $script:authorityInstallEffect = $false
    $script:authorityActivateEffect = $false
    try {
        function Test-GuardianProcessElevated { return $true }
        Assert-GuardianProcessElevated -Operation "install"
        Assert-GuardianProcessElevated -Operation "activation"
        Write-Output "PASS guardian_elevation_gate_passes_when_the_token_is_elevated"

        function Test-GuardianProcessElevated { return $false }
        function Invoke-GuardianInstallTransaction {
            param($Operations)
            $script:authorityInstallEffect = $true
        }
        $installRefused = $false
        try { Install-GuardianTaskTransaction } catch { $installRefused = $_.Exception.Message -eq "guardian_install_requires_elevation" }
        if (-not $installRefused) { throw "guardian install did not refuse an unelevated token with guardian_install_requires_elevation" }
        if ($script:authorityInstallEffect) { throw "guardian install ran its transaction before refusing an unelevated token" }
        Write-Output "PASS guardian_install_requires_elevation_before_any_mutation"

        function Test-SealedGuardianIdentity { $script:authorityActivateEffect = $true }
        function Assert-GuardianPayloadIdentity { $script:authorityActivateEffect = $true }
        $activationRefused = $false
        try { Activate-GuardianTask } catch { $activationRefused = $_.Exception.Message -eq "guardian_activation_requires_elevation" }
        if (-not $activationRefused) { throw "guardian activation did not refuse an unelevated token with guardian_activation_requires_elevation" }
        if ($script:authorityActivateEffect) { throw "guardian activation ran its identity effects before refusing an unelevated token" }
        Write-Output "PASS guardian_activation_requires_elevation_before_any_mutation"
    } finally {
        foreach ($mock in @("Test-GuardianProcessElevated", "Invoke-GuardianInstallTransaction", "Test-SealedGuardianIdentity", "Assert-GuardianPayloadIdentity")) {
            Remove-Item -Path ("Function:\" + $mock) -Force -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-ManufacturedGuardianRegistrationRollbackTests {
    # The generic transaction is the same production transaction used by the
    # scheduler adapter.  This fixture makes the post-register seal explicit
    # and then fails disable, proving exact task/config/seal restoration.
    $store = [ordered]@{
        task = "prior-task-xml"; config = "prior-config"; seal = "prior-task-seal";
        prior_task = "prior-task-xml"; prior_config = "prior-config"; prior_seal = "prior-task-seal"
    }
    $operations = @{
        begin = ({ [ordered]@{ store = $store; task_may_be_modified = $false; config_may_be_modified = $false; task_seal_may_be_modified = $false; payload_may_be_modified = $false; phase = "begin" } }).GetNewClosure()
        backup = { param($transaction) }
        stop_prior = { param($transaction) }
        deploy = { param($transaction) $transaction.payload_may_be_modified = $true }
        config = ({ param($transaction) $transaction.config_may_be_modified = $true; $transaction.store.config = "new-config" }).GetNewClosure()
        register = ({ param($transaction) $transaction.task_may_be_modified = $true; $transaction.store.task = "new-task-xml"; $transaction.task_seal_may_be_modified = $true; $transaction.store.seal = "guardian_task_registration_seal_pending_disable:new-task-xml"; $transaction.owned_task_xml_fingerprint = "new-task-xml" }).GetNewClosure()
        disable = { param($transaction) throw "fixture_disable_failed" }
        verify_disabled = { param($transaction) }
        rollback = ({ param($transaction)
            if ($transaction.task_may_be_modified -and $transaction.store.task -cne $transaction.owned_task_xml_fingerprint) { throw "guardian_task_operator_edit_detected" }
            $transaction.store.task = $transaction.store.prior_task
            $transaction.store.config = $transaction.store.prior_config
            $transaction.store.seal = $transaction.store.prior_seal
        }).GetNewClosure()
    }
    $failed = $false
    try { Invoke-GuardianInstallTransaction -Operations $operations | Out-Null } catch { $failed = $true }
    if (-not $failed -or $store.task -cne "prior-task-xml" -or $store.config -cne "prior-config" -or $store.seal -cne "prior-task-seal") {
        throw "guardian register success / disable failure did not restore exact task config and seal"
    }
    $diagnosticOperations = @{
        begin = { [ordered]@{ task_may_be_modified = $false; config_may_be_modified = $false; task_seal_may_be_modified = $false; payload_may_be_modified = $false; phase = "begin" } }
        backup = { param($transaction) }
        stop_prior = { param($transaction) }
        deploy = { param($transaction) }
        config = { param($transaction) }
        register = { param($transaction) throw "fixture_register_root_cause" }
        disable = { param($transaction) }
        verify_disabled = { param($transaction) }
        rollback = { param($transaction) throw "fixture_register_rollback_failure" }
    }
    $diagnostic = ""
    try { Invoke-GuardianInstallTransaction -Operations $diagnosticOperations | Out-Null } catch { $diagnostic = $_.Exception.Message }
    if ($diagnostic -notlike "*fixture_register_root_cause*" -or $diagnostic -notlike "*fixture_register_rollback_failure*") {
        throw "guardian registration failure did not retain both diagnostic causes"
    }
    # The uninstall contract is fail-closed: an unregister failure is not a
    # restore and therefore leaves the current seal and config untouched.
    $uninstallState = [ordered]@{ task = "sealed-current-task"; config = "sealed-current-config"; seal = "sealed-current-seal" }
    $unregisterFailed = $false
    try { throw "guardian_uninstall_unregister_failed: fixture_scheduler_failure" } catch { $unregisterFailed = $_.Exception.Message -like "guardian_uninstall_unregister_failed:*" }
    if (-not $unregisterFailed -or $uninstallState.task -cne "sealed-current-task" -or $uninstallState.config -cne "sealed-current-config" -or $uninstallState.seal -cne "sealed-current-seal") {
        throw "guardian uninstall failure did not retain sealed state"
    }
    # Exercise the real activation transaction with an in-process scheduler
    # fixture.  Registration succeeds, its first post-register export fails,
    # and the catch path must restore the exact prior XML/seal without changing
    # the operator config.
    $script:activationFixture = [ordered]@{ task = "prior-disabled-xml"; seal = "prior-disabled-xml"; config = "operator-config"; exports = 0 }
    # The elevation gate is real authority, not fixture state: this case is
    # about the export-failure rollback, so the observation is answered as if
    # the token were elevated and the real effect path runs.
    function Test-GuardianProcessElevated { return $true }
    function Test-SealedGuardianIdentity { }
    function Assert-GuardianPayloadIdentity { }
    function Assert-GuardianTaskXmlSeal { }
    function Invoke-GuardianWatchInstanceOwner {
        param($Operation, $IncomingScriptSha256, $IncomingModuleSha256, $IncomingLauncherSha256, $PayloadChanged, $ScriptWriteUtc)
        return [ordered]@{ action = "NONE"; reason = "manufactured_activation_owner" }
    }
    # The staged shape is the real one: a disabled definition under a prior
    # instance that is still finishing.  A fixture that answers Disabled here
    # is kinder than an attended reinstall and hides the production gate.
    function Get-ScheduledTask { param([string]$TaskName) [pscustomobject]@{ State = "Running"; Settings = [pscustomobject]@{ Enabled = $false } } }
    function New-GuardianTask { param([switch]$ActivationAuthorized) return "activated-task-xml" }
    function Export-ScheduledTask {
        param([string]$TaskName)
        $script:activationFixture.exports++
        if ($script:activationFixture.exports -eq 1) { return "prior-disabled-xml" }
        throw "fixture_activation_export_failure"
    }
    function Register-ScheduledTask {
        param([string]$TaskName, [object]$InputObject, [string]$Xml, [switch]$Force)
        $script:activationFixture.task = if ($PSBoundParameters.ContainsKey("Xml")) { $Xml } else { [string]$InputObject }
    }
    function Enable-ScheduledTask { param([string]$TaskName) }
    function Write-GuardianTaskSeal { param([string]$Xml, [string]$Phase) $script:activationFixture.seal = $Xml }
    $priorActivationApproval = $ApproveGuardianActivation
    try {
        $ApproveGuardianActivation = $GuardianActivationApproval
        $activationFailed = $false
        try { Activate-GuardianTask } catch { $activationFailed = $_.Exception.Message -like "guardian activation failed; disabled staging restored:*" }
        if (-not $activationFailed -or $script:activationFixture.task -cne "prior-disabled-xml" -or
            $script:activationFixture.seal -cne "prior-disabled-xml" -or $script:activationFixture.config -cne "operator-config") {
            throw "guardian activation export failure did not restore exact task seal and operator config"
        }
    } finally {
        $ApproveGuardianActivation = $priorActivationApproval
        foreach ($mock in @("Test-GuardianProcessElevated", "Test-SealedGuardianIdentity", "Assert-GuardianPayloadIdentity", "Assert-GuardianTaskXmlSeal", "Invoke-GuardianWatchInstanceOwner", "Get-ScheduledTask", "New-GuardianTask", "Export-ScheduledTask", "Register-ScheduledTask", "Enable-ScheduledTask", "Write-GuardianTaskSeal")) {
            Remove-Item -Path ("Function:\" + $mock) -Force -ErrorAction SilentlyContinue
        }
    }
    Write-Output "PASS guardian_register_success_disable_failure_restores_task_config_and_seal"
    Write-Output "PASS guardian_registration_failure_retains_root_and_rollback_causes"
    Write-Output "PASS guardian_uninstall_failure_retains_seal_and_state"
    Write-Output "PASS guardian_activation_export_failure_restores_exact_task_seal_and_operator_config"
}

function Invoke-ManufacturedGuardianActivationStagingTests {
    # A correctly staged definition is one whose registration is disabled.  A
    # prior watch that is still finishing keeps the runtime projection at
    # Running, which is the normal case at an attended reinstall.  Activation
    # must accept that shape, promote it, and leave the definition enabled --
    # it must not require the prior instance to have already exited.
    $script:activationStagingFixture = [ordered]@{ registered = $false; enabled = $false; seal_phase = $null }
    function Test-GuardianProcessElevated { return $true }
    function Test-SealedGuardianIdentity { }
    function Assert-GuardianPayloadIdentity { }
    function Assert-GuardianTaskXmlSeal { }
    function Invoke-GuardianWatchInstanceOwner {
        param($Operation, $IncomingScriptSha256, $IncomingModuleSha256, $IncomingLauncherSha256, $PayloadChanged, $ScriptWriteUtc)
        return [ordered]@{ action = "NONE"; reason = "manufactured_activation_owner" }
    }
    function Get-ScheduledTask {
        param([string]$TaskName)
        if (-not $script:activationStagingFixture.enabled) {
            return [pscustomobject]@{ State = "Running"; Settings = [pscustomobject]@{ Enabled = $false } }
        }
        return [pscustomobject]@{ State = "Ready"; Settings = [pscustomobject]@{ Enabled = $true } }
    }
    function New-GuardianTask { param([switch]$ActivationAuthorized) return "activated-task-xml" }
    function Export-ScheduledTask { param([string]$TaskName) return "staged-disabled-xml" }
    function Register-ScheduledTask {
        param([string]$TaskName, [object]$InputObject, [string]$Xml, [switch]$Force)
        $script:activationStagingFixture.registered = $true
    }
    function Enable-ScheduledTask { param([string]$TaskName) $script:activationStagingFixture.enabled = $true }
    function Write-GuardianTaskSeal { param([string]$Xml, [string]$Phase) $script:activationStagingFixture.seal_phase = $Phase }
    $priorActivationApproval = $ApproveGuardianActivation
    try {
        $ApproveGuardianActivation = $GuardianActivationApproval
        Activate-GuardianTask
        if (-not $script:activationStagingFixture.registered) {
            throw "guardian activation refused a staged definition under a finishing prior instance"
        }
        if (-not $script:activationStagingFixture.enabled) {
            throw "guardian activation did not enable the definition it accepted"
        }
        if ($script:activationStagingFixture.seal_phase -cne "activated") {
            throw ("guardian activation did not seal the activated phase: " + [string]$script:activationStagingFixture.seal_phase)
        }
    } finally {
        $ApproveGuardianActivation = $priorActivationApproval
        foreach ($mock in @("Test-GuardianProcessElevated", "Test-SealedGuardianIdentity", "Assert-GuardianPayloadIdentity", "Assert-GuardianTaskXmlSeal", "Invoke-GuardianWatchInstanceOwner", "Get-ScheduledTask", "New-GuardianTask", "Export-ScheduledTask", "Register-ScheduledTask", "Enable-ScheduledTask", "Write-GuardianTaskSeal")) {
            Remove-Item -Path ("Function:\" + $mock) -Force -ErrorAction SilentlyContinue
        }
    }
    Write-Output "PASS guardian_activation_accepts_a_staged_definition_under_a_finishing_prior_instance"
}

function Invoke-ManufacturedGuardianWatchInstanceLifecycleTests {
    # The payload-swap defect: a process that loaded the prior script keeps
    # running after the bytes on disk change.  The owner must bind the exact
    # script path, action, distro, policy SID, process instance, and payload
    # hash; a generic powershell.exe name or a byte-only match is not enough.
    $scriptPath = "C:\ProgramData\RamShared\guardian\Watch-RamSharedWsl.ps1"
    $sid = "S-1-5-21-1-2-3-1002"
    $distro = "Ubuntu-24.04"
    $childLine = '"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $scriptPath + '" -Action watch -Run -Distro "' + $distro + '" -UserSid "' + $sid + '"'
    # The production child line: the launcher omits quotes Windows does not
    # need, so this is the exact shape the live host carries.
    $unquotedChildLine = 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' + $scriptPath + ' -Action watch -Run -Distro ' + $distro + ' -UserSid ' + $sid
    $launcherLine = '"C:\ProgramData\RamShared\bin\ramshared-host-launch.exe" --timeout-ms 0 -- ' + $childLine
    if (-not (Test-GuardianWatchCommandLineMatch -CommandLine $childLine -ScriptPath $scriptPath -Sid $sid -DistroName $distro)) {
        throw "guardian payload swap refused the sealed watch child"
    }
    if (-not (Test-GuardianWatchCommandLineMatch -CommandLine $unquotedChildLine -ScriptPath $scriptPath -Sid $sid -DistroName $distro)) {
        throw "guardian payload swap refused the unquoted production child line"
    }
    if (-not (Test-GuardianWatchCommandLineMatch -CommandLine $launcherLine -ScriptPath $scriptPath -Sid $sid -DistroName $distro)) {
        throw "guardian payload swap refused the sealed launcher"
    }
    $foreignLines = @(
        @{ name = "foreign_script"; line = $childLine.Replace($scriptPath, "C:\ProgramData\RamShared\guardian\Other.ps1") },
        @{ name = "foreign_sid"; line = $childLine.Replace($sid, "S-1-5-21-1-2-3-2002") },
        @{ name = "ensure_action"; line = $childLine.Replace("-Action watch", "-Action ensure") },
        @{ name = "foreign_distro"; line = $childLine.Replace($distro, "Debian") },
        @{ name = "script_path_prefix"; line = 'powershell.exe -NoProfile -File ' + $scriptPath + '.bak -Action watch -Distro ' + $distro + ' -UserSid ' + $sid },
        @{ name = "script_path_suffix"; line = 'powershell.exe -NoProfile -File ' + $scriptPath + 'Extra -Action watch -Distro ' + $distro + ' -UserSid ' + $sid }
    )
    foreach ($case in $foreignLines) {
        if (Test-GuardianWatchCommandLineMatch -CommandLine ([string]$case.line) -ScriptPath $scriptPath -Sid $sid -DistroName $distro) {
            throw ("guardian payload swap accepted " + $case.name)
        }
    }
    # Selection over observations: a SYSTEM-owned process with no readable
    # command line is provably not this task's instance and never blocks an
    # install; an unreadable process that could be ours does block it.
    $selection = Get-GuardianWatchInstanceMatch -Processes @(
        [pscustomobject]@{ process_id = 10; name = "powershell.exe"; owner_sid = "S-1-5-18"; command_line = "" },
        [pscustomobject]@{ process_id = 11; name = "powershell.exe"; owner_sid = $sid; command_line = "" },
        [pscustomobject]@{ process_id = 12; name = "powershell.exe"; owner_sid = $sid; command_line = $unquotedChildLine },
        [pscustomobject]@{ process_id = 13; name = "powershell.exe"; owner_sid = $sid; command_line = "powershell.exe -NoProfile -Command Get-Date" }
    ) -ScriptPath $scriptPath -Sid $sid -DistroName $distro
    if (@($selection.matches).Count -ne 1 -or [int]$selection.matches[0].process_id -ne 12) {
        throw "guardian payload swap selection did not bind exactly the product watch"
    }
    if (@($selection.unprovable).Count -ne 1 -or [int]$selection.unprovable[0].process_id -ne 11) {
        throw "guardian payload swap selection did not isolate the unprovable candidate"
    }
    Write-Output "PASS guardian_payload_swap_matches_only_the_sealed_command_line"

    $hash = "A" * 64
    $other = "B" * 64
    $record = [pscustomobject]@{
        schema_version = 1; pid = 1234; process_start_utc_ticks = [long]1000000
        user_sid = $sid; task_name = "RamSharedWslGuardian.v1"; distro = $distro
        script_path = $scriptPath; script_sha256 = $hash; module_sha256 = $hash; launcher_sha256 = $hash
    }
    $live = [pscustomobject]@{ alive = $true; process_id = 1234; start_utc_ticks = [long]1000000; owner_sid = $sid }
    $call = @{ ExpectedSid = $sid; ExpectedTaskName = "RamSharedWslGuardian.v1"; ExpectedScriptPath = $scriptPath }
    $current = Get-GuardianWatchInstanceDecision -Record $record -Observation $live -IncomingScriptSha256 $hash @call
    $stale = Get-GuardianWatchInstanceDecision -Record $record -Observation $live -IncomingScriptSha256 $other @call
    $dead = Get-GuardianWatchInstanceDecision -Record $record -Observation ([pscustomobject]@{ alive = $false; process_id = 1234; start_utc_ticks = $null; owner_sid = $null }) -IncomingScriptSha256 $hash @call
    $reused = Get-GuardianWatchInstanceDecision -Record $record -Observation ([pscustomobject]@{ alive = $true; process_id = 1234; start_utc_ticks = [long]9000000; owner_sid = $sid }) -IncomingScriptSha256 $hash @call
    $foreignOwner = Get-GuardianWatchInstanceDecision -Record $record -Observation ([pscustomobject]@{ alive = $true; process_id = 1234; start_utc_ticks = [long]1000000; owner_sid = "S-1-5-21-1-2-3-2002" }) -IncomingScriptSha256 $hash @call
    $foreignRecord = [pscustomobject]@{ schema_version = 1; pid = 1234; process_start_utc_ticks = [long]1000000; user_sid = "S-1-5-21-1-2-3-2002"; task_name = "RamSharedWslGuardian.v1"; distro = $distro; script_path = $scriptPath; script_sha256 = $hash; module_sha256 = $hash; launcher_sha256 = $hash }
    $foreignSid = Get-GuardianWatchInstanceDecision -Record $foreignRecord -Observation $live -IncomingScriptSha256 $hash @call
    if ($current.action -ne "CURRENT") { throw ("guardian payload swap misread the current instance: " + $current.action) }
    if ($stale.action -ne "STOP" -or $stale.reason -ne "payload_changed") { throw ("guardian payload swap did not stop prior bytes: " + $stale.action + " " + $stale.reason) }
    if ($dead.action -ne "CLEAR") { throw ("guardian payload swap did not clear a dead instance: " + $dead.action) }
    if ($reused.action -ne "CLEAR" -or $reused.reason -ne "recorded_instance_pid_reused") { throw ("guardian payload swap signaled a reused PID: " + $reused.action + " " + $reused.reason) }
    if ($foreignOwner.action -ne "REFUSE" -or $foreignOwner.reason -ne "recorded_instance_owner_sid_mismatch") { throw ("guardian payload swap accepted a foreign owner: " + $foreignOwner.action + " " + $foreignOwner.reason) }
    if ($foreignSid.action -ne "REFUSE" -or $foreignSid.reason -ne "recorded_instance_user_sid_mismatch") { throw ("guardian payload swap accepted a foreign record: " + $foreignSid.action + " " + $foreignSid.reason) }
    Write-Output "PASS guardian_payload_swap_stops_only_the_instance_that_loaded_prior_bytes"

    $installDefinition = [string](Get-Command Invoke-GuardianInstallTransaction).Definition
    $stopIndex = $installDefinition.IndexOf('"stop_prior"')
    $deployIndex = $installDefinition.IndexOf('"deploy"')
    if ($stopIndex -lt 0 -or $deployIndex -lt 0 -or $stopIndex -gt $deployIndex) {
        throw "guardian payload swap does not run before the payload is deployed"
    }
    if (-not ([string](Get-Command New-GuardianInstallOperations).Definition).Contains("stop_prior =")) {
        throw "guardian install transaction has no stop_prior operation"
    }
    Write-Output "PASS guardian_payload_swap_runs_before_the_payload_is_deployed"

    $registerDefinition = [string](Get-Command Register-GuardianWatchInstance).Definition
    if (-not $registerDefinition.Contains("guardian_watch_instance_already_running") -or
        -not $registerDefinition.Contains("guardian_stale_watch_instance_not_reaped")) {
        throw "guardian watch instance registration does not refuse a duplicate or an unreaped prior instance"
    }
    $watchDefinition = [string](Get-Command Invoke-GuardianWatch).Definition
    if (-not $watchDefinition.Contains("Register-GuardianWatchInstance") -or
        -not $watchDefinition.Contains("Remove-GuardianWatchInstanceRecord")) {
        throw "guardian watch does not publish and release its exact process instance"
    }
    Write-Output "PASS guardian_watch_publishes_and_releases_its_exact_process_instance"

    $priorPath = $script:GuardianWatchInstancePath
    $script:GuardianWatchInstancePath = Join-Path $env:TEMP ("ramshared-watch-instance-test-" + [Guid]::NewGuid().ToString("N") + ".json")
    try {
        $identity = [pscustomobject]@{ deployed_script_sha256 = $hash; deployed_module_sha256 = $hash; deployed_launcher_sha256 = $hash }
        $written = New-GuardianWatchInstanceRecord -PayloadIdentity $identity -StartedUtc ([DateTime]::UtcNow) -ProcessId 4242
        Write-AtomicJson -Path $script:GuardianWatchInstancePath -Value $written
        $read = Read-GuardianWatchInstanceRecord
        if ($null -eq $read -or [int]$read.pid -ne 4242) { throw "guardian watch instance record did not round-trip" }
        Remove-GuardianWatchInstanceRecord -Record ([pscustomobject]@{ pid = 4243; process_start_utc_ticks = [long]$written.process_start_utc_ticks })
        if (-not (Test-Path -LiteralPath $script:GuardianWatchInstancePath -PathType Leaf)) {
            throw "guardian watch instance cleanup removed a record owned by another instance"
        }
        Remove-GuardianWatchInstanceRecord -Record $written
        if (Test-Path -LiteralPath $script:GuardianWatchInstancePath -PathType Leaf) {
            throw "guardian watch instance cleanup kept its own record"
        }
    } finally {
        Remove-Item -LiteralPath $script:GuardianWatchInstancePath -Force -ErrorAction SilentlyContinue
        $script:GuardianWatchInstancePath = $priorPath
    }
    Write-Output "PASS guardian_watch_instance_record_is_owned_by_pid_and_start_time"
}

function Invoke-ManufacturedGuardianTaskSealTests {
    $stagedXml = '<Task><Enabled>false</Enabled><Distro>Ubuntu-24.04</Distro></Task>'
    $operatorXml = '<Task><Enabled>false</Enabled><Distro>Foreign-Ubuntu</Distro></Task>'
    if ((Get-GuardianTaskXmlFingerprint -Xml $stagedXml) -ceq (Get-GuardianTaskXmlFingerprint -Xml $operatorXml)) {
        throw "guardian task XML fingerprint ignored operator edit"
    }
    Write-Output "PASS guardian_task_xml_seal_refuses_operator_edit_and_preserves_backup"
}

$plan = [ordered]@{ state = "PLAN"; action = $Action; distro = $Distro; user_sid = $UserSid; heartbeat = $HeartbeatPath; stale_after_seconds = $StaleAfterSec; task_name = $TaskName; task_disabled_by_default = $true; staging_capture_only = $true; activation_requires_exact_approval = $GuardianActivationApproval; scheduled_task_action = "watch"; guardian_policy = [ordered]@{ artifact_root = $ArtifactRoot; poll_seconds = $PollSec; guest_command_timeout_seconds = $GuestCommandTimeoutSec }; resume_lease_path = $ResumeLeasePath; no_windows_reboot = $true; no_wsl_shutdown = $true }
if (-not $Run -and $Action -ne "test") { $plan | ConvertTo-Json -Depth 5; exit 0 }
# Attended mutations name themselves here.  `ensure` is deliberately absent: it
# is the unattended guest-boot start (DT-19), authorized by the sealed identity
# and an elevated token rather than by an operator token typed into argv.
if (($Action -eq "install" -or $Action -eq "uninstall" -or $Action -eq "watch") -and $ApproveGuardianAction -cne $GuardianActionApproval) { throw "guardian action requires exact attended approval" }

switch ($Action) {
    "install" { Install-GuardianTaskTransaction; Write-Output "guardian installed disabled: $TaskName" }
    "status" { Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue | Select-Object TaskName, State }
    "capture" { New-Item -ItemType Directory -Force -Path $ArtifactRoot | Out-Null; $captureDirectory = Join-Path $ArtifactRoot ("ramshared-guardian-capture-" + (Get-Date -Format "yyyyMMdd-HHmmss")); New-Item -ItemType Directory -Path $captureDirectory | Out-Null; Invoke-HostSnapshot -RunDirectory $captureDirectory | ConvertTo-Json -Depth 6 }
    "uninstall" { Test-SealedGuardianIdentity; if (-not (Restore-SealedGuardianBackup)) { Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction Stop } }
    "watch" { exit (Invoke-GuardianWatch) }
    "activate" { Activate-GuardianTask; Write-Output "guardian activated by explicit attended transaction: $TaskName" }
    "ensure" { Ensure-GuardianTaskRunning }
    "test" {
        $bootProbeSummary = Get-GuardianBootProbeSummary -Probe ([ordered]@{ completed = $true; exit_code = 0; reason = "success"; stdout = "sensitive"; stderr = "sensitive" })
        if (-not $bootProbeSummary.completed -or $bootProbeSummary.exit_code -ne 0 -or $bootProbeSummary.reason -cne "success" -or
            $bootProbeSummary.PSObject.Properties.Name -contains "stdout" -or $bootProbeSummary.PSObject.Properties.Name -contains "stderr") {
            throw "guardian boot probe summary did not remain sanitized"
        }
        Write-Output "PASS guardian_boot_probe_summary_is_sanitized"
        $bootProbeEventData = Get-GuardianBootProbeEventData -Probe $bootProbeSummary
        if ($bootProbeEventData.probe_completed -ne $true -or $bootProbeEventData.probe_exit_code -ne 0 -or
            $bootProbeEventData.probe_reason -cne "success" -or $bootProbeEventData.Keys.Count -ne 3) {
            throw "guardian boot probe event payload was not flat and sanitized"
        }
        Write-Output "PASS guardian_boot_probe_event_payload_is_flat"
        $guardianWslPrefix = Get-GuardianWslCommandPrefix
        if ($guardianWslPrefix -cne ("-d " + $Distro + " -u root --")) {
            throw "guardian WSL command prefix must preserve the validated distro name without quotes"
        }
        Write-Output "PASS guardian_wsl_arguments_are_scheduler_safe"
        $hcsStatusCases = @(
            @{ name = "serialized_running_enum"; status = 4; expected = $true },
            @{ name = "running_name"; status = "Running"; expected = $true },
            @{ name = "serialized_running_enum_text"; status = "4"; expected = $true },
            @{ name = "stopped_enum"; status = 1; expected = $false },
            @{ name = "stopped_name"; status = "Stopped"; expected = $false },
            @{ name = "unknown_status"; status = "unknown"; expected = $false },
            @{ name = "boolean_status"; status = $true; expected = $false },
            @{ name = "fractional_status"; status = 4.5; expected = $false },
            @{ name = "missing_status"; status = $null; expected = $false }
        )
        foreach ($case in $hcsStatusCases) {
            $isRunning = Test-HcsServiceRunning -Status $case.status
            if ([bool]$isRunning -ne [bool]$case.expected) {
                throw ("manufactured HCS status classification failed: " + $case.name)
            }
        }
        Write-Output "PASS guardian_hcs_status_accepts_serialized_running_enum"
        if (-not $HcsServiceStatusQuery.Contains(".Status.ToString()")) {
            throw "manufactured HCS query must normalize the service enum before JSON serialization"
        }
        Write-Output "PASS guardian_hcs_status_json_is_normalized"
        $taskArguments = Get-SealedGuardianTaskArguments
        foreach ($sealedArgument in @(
            ('-Action watch -Run'),
            ('-Distro "' + $Distro + '"'),
            ('-HeartbeatPath "' + $HeartbeatPath + '"'),
            ('-ArtifactRoot "' + $ArtifactRoot + '"'),
            ('-StaleAfterSec ' + $StaleAfterSec),
            ('-PollSec ' + $PollSec),
            ('-GuestCommandTimeoutSec ' + $GuestCommandTimeoutSec)
        )) {
            if (-not $taskArguments.Contains($sealedArgument)) { throw ("manufactured guardian task policy was not sealed: " + $sealedArgument) }
        }
        $responsiveGuest = [ordered]@{ probes = @(
                [ordered]@{ name = "guest_process_probe"; result = [ordered]@{ completed = $true; exit_code = 0 } },
                [ordered]@{ name = "guest_boot_identity_probe"; result = [ordered]@{ completed = $true; exit_code = 0 } }
            ) }
        $oneFailedGuest = [ordered]@{ probes = @(
                [ordered]@{ name = "guest_process_probe"; result = [ordered]@{ completed = $false; exit_code = $null } },
                [ordered]@{ name = "guest_boot_identity_probe"; result = [ordered]@{ completed = $true; exit_code = 0 } }
            ) }
        $failedGuest = [ordered]@{ probes = @(
                [ordered]@{ name = "guest_process_probe"; result = [ordered]@{ completed = $false; exit_code = $null } },
                [ordered]@{ name = "guest_boot_identity_probe"; result = [ordered]@{ completed = $true; exit_code = 1 } }
            ) }
        $healthyHost = [ordered]@{ failed = $false; wsl_failed = $false; hcs_failed = $false }
        $partiallyFailedHost = [ordered]@{ failed = $false; wsl_failed = $true; hcs_failed = $false }
        $failedHost = [ordered]@{ failed = $true; wsl_failed = $true; hcs_failed = $true }
        $openSnapshot = [ordered]@{ closed = $false }
        $closedSnapshot = [ordered]@{ closed = $true }
        $cases = @(
            @{ name = "healthy_guest_with_stale_monitor_never_terminates"; heartbeat = $false; guest = $responsiveGuest; host = $healthyHost; snapshot = $openSnapshot; recorded = $false; action = "WAIT" },
            @{ name = "guardian_refuses_when_guest_probe_is_responsive"; heartbeat = $true; guest = $responsiveGuest; host = $failedHost; snapshot = $closedSnapshot; recorded = $false; action = "REFUSE" },
            @{ name = "guardian_refuses_when_host_probe_is_healthy"; heartbeat = $true; guest = $failedGuest; host = $healthyHost; snapshot = $closedSnapshot; recorded = $false; action = "REFUSE" },
            @{ name = "guardian_refuses_when_snapshot_is_open"; heartbeat = $true; guest = $failedGuest; host = $failedHost; snapshot = $openSnapshot; recorded = $false; action = "REFUSE" },
            @{ name = "guardian_terminates_only_after_all_gates"; heartbeat = $true; guest = $failedGuest; host = $failedHost; snapshot = $closedSnapshot; recorded = $false; action = "TERMINATE" },
            @{ name = "guardian_refuses_duplicate_termination"; heartbeat = $true; guest = $failedGuest; host = $failedHost; snapshot = $closedSnapshot; recorded = $true; action = "REFUSE" },
            @{ name = "one_timeout_or_single_guest_probe_refuses_termination"; heartbeat = $true; guest = $oneFailedGuest; host = $failedHost; snapshot = $closedSnapshot; recorded = $false; action = "REFUSE" },
            @{ name = "guardian_requires_two_distinct_guest_failures_and_dual_host_corroboration"; heartbeat = $true; guest = $failedGuest; host = $partiallyFailedHost; snapshot = $closedSnapshot; recorded = $false; action = "REFUSE" }
        )
        foreach ($case in $cases) {
            $decision = Get-GuardianTerminationDecision -HeartbeatStale $case.heartbeat -GuestProbe $case.guest -HostProbe $case.host -Snapshot $case.snapshot -TerminationRecorded $case.recorded
            if ($decision.action -cne $case.action) { throw ("manufactured guardian decision failed: " + $case.name) }
            Write-Output ("PASS " + $case.name)
        }
        try {
            Publish-GuardianState -State "HEALTHY" -Reason "manufactured" -BootId $null
            throw "manufactured bootless healthy guardian proof was accepted"
        } catch {
            if ($_.Exception.Message -eq "manufactured bootless healthy guardian proof was accepted") { throw }
        }
        Write-Output "PASS boot_bound_healthy_proof_required_before_publish"
        if ((Test-CanonicalGuestBootId -BootId "01234567-89AB-4def-8123-456789abcdef") -or
            -not (Test-CanonicalGuestBootId -BootId "01234567-89ab-4def-8123-456789abcdef")) {
            throw "guardian boot ID canonical parser accepted malformed or rejected canonical ID"
        }
        Write-Output "PASS guardian_boot_id_rejects_noncanonical_guid"
        # DT-17: the durable gate's lifetime must not be a side effect of its
        # own probe children's console group, and its task must be able to come
        # back after every restart class that has actually been observed.
        $lifetimeMode = Protect-GuardianConsoleLifetime
        if ($lifetimeMode -eq "unset") {
            throw "guardian console control-c is still a way to end the gate"
        }
        if ($lifetimeMode -eq "console_taken" -and -not [Console]::TreatControlCAsInput) {
            throw "guardian claimed a taken console while control-c is still a terminate signal"
        }
        if ($lifetimeMode -ne "console_taken" -and $lifetimeMode -ne "no_console") {
            throw ("guardian console lifetime returned an unknown mode: " + [string]$lifetimeMode)
        }
        Write-Output "PASS guardian_console_control_is_not_a_way_to_end_the_gate"
        $lifetimeTask = New-GuardianTask
        $lifetimeTriggerClasses = @($lifetimeTask.Triggers | ForEach-Object { $_.CimClass.CimClassName })
        if ($lifetimeTriggerClasses.Count -ne 2 -or
            $lifetimeTriggerClasses -notcontains 'MSFT_TaskLogonTrigger' -or
            $lifetimeTriggerClasses -notcontains 'MSFT_TaskBootTrigger') {
            throw ("guardian task is not self-healing across logon and startup: " + ($lifetimeTriggerClasses -join ","))
        }
        Write-Output "PASS guardian_task_is_self_healing_across_logon_and_startup"
        if (-not $lifetimeTask.Settings.StartWhenAvailable) {
            throw "guardian task missed trigger does not start when available"
        }
        Write-Output "PASS guardian_task_missed_trigger_still_starts_when_available"
        # A battery stop is a clean stop, so RestartOnFailure never sees it and
        # the producer stays dead until the next logon or boot. The product's
        # settings must not inherit that default. The cmdlet switch names are
        # the positive form; the serialized properties are the negative form.
        # StrictMode forbids a bare property read on a missing member, so the
        # observation is explicit.
        $lifetimeSettings = $lifetimeTask.Settings
        $disallowStartOnBattery = $lifetimeSettings.PSObject.Properties['DisallowStartIfOnBatteries']
        $stopOnBattery = $lifetimeSettings.PSObject.Properties['StopIfGoingOnBatteries']
        if ($null -eq $disallowStartOnBattery -or [bool]$disallowStartOnBattery.Value) {
            throw "guardian task refuses to start on battery"
        }
        if ($null -eq $stopOnBattery -or [bool]$stopOnBattery.Value) {
            throw "guardian task is stopped by a power-source change"
        }
        Write-Output "PASS guardian_task_is_never_stopped_by_a_power_source_change"
        # DT-19: the guest boot path may start the sealed named producer and
        # nothing else.  Every branch is driven through the pure decision so
        # the live task is never touched by a test.
        $ensureEnabledReady = [pscustomobject]@{ State = "Ready"; Settings = [pscustomobject]@{ Enabled = $true } }
        $ensureEnabledRunning = [pscustomobject]@{ State = "Running"; Settings = [pscustomobject]@{ Enabled = $true } }
        $ensureDisabledDefinition = [pscustomobject]@{ State = "Ready"; Settings = [pscustomobject]@{ Enabled = $false } }
        $ensureDecision = Get-GuardianEnsureDecision -Task $ensureEnabledReady
        if ($ensureDecision.action -ne "START" -or $ensureDecision.reason -cne "producer_not_running") {
            throw ("guardian ensure did not select a start for an enabled idle task: " + ($ensureDecision | ConvertTo-Json -Compress))
        }
        Write-Output "PASS guardian_ensure_starts_the_sealed_named_task"
        $ensureDecision = Get-GuardianEnsureDecision -Task $ensureEnabledRunning
        if ($ensureDecision.action -ne "NOOP" -or $ensureDecision.reason -cne "already_running") {
            throw ("guardian ensure is not idempotent while the task is running: " + ($ensureDecision | ConvertTo-Json -Compress))
        }
        Write-Output "PASS guardian_ensure_is_idempotent_while_the_task_is_running"
        $ensureDecision = Get-GuardianEnsureDecision -Task $ensureDisabledDefinition
        if ($ensureDecision.action -ne "REFUSE" -or $ensureDecision.reason -cne "task_definition_disabled") {
            throw ("guardian ensure did not refuse a disabled task definition: " + ($ensureDecision | ConvertTo-Json -Compress))
        }
        Write-Output "PASS guardian_ensure_refuses_a_disabled_task_without_enabling_it"
        $ensureDecision = Get-GuardianEnsureDecision -Task $null
        if ($ensureDecision.action -ne "REFUSE" -or $ensureDecision.reason -cne "task_absent") {
            throw ("guardian ensure did not refuse an absent task without creating it: " + ($ensureDecision | ConvertTo-Json -Compress))
        }
        Write-Output "PASS guardian_ensure_refuses_an_absent_task_without_creating_it"
        $ensureDefinition = [string](Get-Command Ensure-GuardianTaskRunning).Definition
        if ($ensureDefinition -match 'Publish-GuardianState|Write-AtomicJson|Write-HostSafeModeGate|Write-SealedGuardianConfig|GuardianStateRoot|health\.json') {
            throw "guardian ensure writes a proof or a state document; the guest must never mint its own verification"
        }
        Write-Output "PASS guardian_ensure_never_writes_a_health_proof"
        $ensureIdentityIndex = $ensureDefinition.IndexOf("Test-SealedGuardianIdentity")
        $ensurePayloadIndex = $ensureDefinition.IndexOf("Assert-GuardianPayloadIdentity")
        $ensureMutationIndex = $ensureDefinition.IndexOf("Start-ScheduledTask")
        if ($ensureIdentityIndex -lt 0 -or $ensurePayloadIndex -lt 0 -or $ensureMutationIndex -lt 0) {
            throw "guardian ensure is missing its authority or its mutation boundary"
        }
        if ($ensureIdentityIndex -gt $ensureMutationIndex -or $ensurePayloadIndex -gt $ensureMutationIndex) {
            throw "guardian ensure reaches Task Scheduler before validating the sealed identity and the payload bytes"
        }
        Write-Output "PASS guardian_ensure_validates_the_sealed_identity_before_any_host_mutation"
        Invoke-ManufacturedGuardianWatchIterationTests
        Invoke-ManufacturedGuardianWatchTailTests
        Invoke-ManufacturedGuardianPidReuseTests
        Invoke-ManufacturedGuardianBoundedCallTests
        Invoke-ManufacturedGuardianAtomicWriteTests
        Invoke-ManufacturedGuardianAuthorityTests
        Invoke-ManufacturedGuardianTaskSealTests
        Invoke-ManufacturedGuardianStagingPredicateTests
        Invoke-ManufacturedGuardianInstallTests
        Invoke-ManufacturedGuardianWatchInstanceLifecycleTests
        Invoke-ManufacturedGuardianPayloadIdentityTests
        Invoke-ManufacturedGuardianRegistrationRollbackTests
        Invoke-ManufacturedGuardianActivationStagingTests
        $stagedTaskArguments = Get-SealedGuardianTaskArguments
        $activatedTaskArguments = Get-SealedGuardianTaskArguments -ActivationAuthorized
        if ($stagedTaskArguments.Contains("-ApproveGuardianAction") -or
            -not $activatedTaskArguments.Contains(('-ApproveGuardianAction "' + $GuardianActionApproval + '"'))) {
            throw "manufactured guardian activation arguments are not explicit"
        }
        if (-not $stagedTaskArguments.Contains(('-File "' + $GuardianPayloadScriptPath + '"'))) {
            throw "manufactured guardian task action is not bound to the product-owned payload path"
        }
        # Exact target, not a substring: a test run from the deployed directory
        # has $PSScriptRoot inside the product path, so a Contains($PSScriptRoot)
        # would fire on the correct answer.
        $launchDirectoryBound = Join-Path $PSScriptRoot $GuardianPayloadScriptName
        if ($launchDirectoryBound -cne $GuardianPayloadScriptPath -and
            $stagedTaskArguments.Contains(('-File "' + $launchDirectoryBound + '"'))) {
            throw "manufactured guardian task action is bound to a caller launch directory"
        }
        Write-Output "PASS guardian_task_action_binds_the_product_payload_path"
        # DT-12: the action program is the sealed launcher and the interpreter
        # is an absolute path. A `PowerShell.exe` through PATH would be both a
        # hijack surface and an unsealed identity.
        if (-not $stagedTaskArguments.StartsWith(('--timeout-ms {0} -- "{1}" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{2}"' -f $GuardianLauncherTimeoutMs, $WindowsPowerShellPath, $GuardianPayloadScriptPath))) {
            throw ("manufactured guardian task arguments do not name the launcher shape: " + $stagedTaskArguments)
        }
        $newTaskDefinition = [string](Get-Command New-GuardianTask).Definition
        if (-not $newTaskDefinition.Contains('-Execute $HostLauncherPath')) {
            throw "manufactured guardian task action does not use the windowless launcher"
        }
        if ($newTaskDefinition.Contains('-Execute "PowerShell.exe"')) {
            throw "manufactured guardian task action still resolves PowerShell through PATH"
        }
        Write-Output "PASS guardian_task_action_uses_the_windowless_launcher"
        Write-Output "PASS guardian_task_arguments_seal_distro_timeout_and_artifact_policy"
        Write-Output "PASS guardian_terminates_exactly_once_and_enters_safe_mode"
        Write-Output "PASS host_safe_mode_gate_survives_guardian_and_guest_restart"
        Write-Output "PASS guardian_never_uses_wsl_shutdown_or_host_restart"
        Write-Output "PASS guardian_only_terminates_the_sealed_distro_after_all_gates"
        Write-Output "PASS guardian_timeout_cleanup_is_bounded"
        Write-Output "PASS disabled_guardian_task_never_mutates_wsl_or_disk_at_logon"
        Write-Output "PASS guardian_activation_is_explicit_and_staging_remains_disabled"
    }
}
