#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-RamSharedWslWatchdogStatic.ps1
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$target = Join-Path $PSScriptRoot "Watch-RamSharedWsl.ps1"
if (-not (Test-Path -LiteralPath $target -PathType Leaf)) {
    throw "ramshared_wsl_guardian: target script is missing"
}
$source = Get-Content -Raw -LiteralPath $target
$telemetrySource = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot "SharedWslHostMemoryGate.psm1")
$telemetryContracts = @("Get-SharedWslHostMemorySample", "physical_memory_total_kib", "physical_memory_available_kib", "commit_used_kib", "commit_available_kib", "pagefile_used_mib", "vmmem_wsl", "origin_volume", "ramshared-origin-manifest.json", "Get-Volume -FilePath", "volume_unique_id")

function Require-GuardianContract {
    param([Parameter(Mandatory = $true)][string]$Text)
    if ($Text -in $telemetryContracts) {
        if (-not $telemetrySource.Contains($Text)) { throw "guardian telemetry module: missing contract $Text" }
        return
    }
    if (-not $source.Contains($Text)) {
        throw "ramshared_wsl_guardian: missing contract $Text"
    }
}

foreach ($required in @(
    'ValidateSet("install", "status", "capture", "uninstall", "watch", "activate", "ensure", "test")',
    'Register-ScheduledTask',
    'Unregister-ScheduledTask',
    'Get-ScheduledTask',
    'PT0S',
    'ramshared-heartbeat.json',
    'ValidateRange(15, 60)',
    'Invoke-GuestProbe',
    'Invoke-IndependentHostProbe',
    'Invoke-HostSnapshot',
    'Test-HcsServiceRunning',
    '$HcsServiceStatusQuery',
    'safe-mode',
    'host-resume-lease.json',
    'guardian-config.json',
    'Publish-GuardianState',
    '.health.json',
    'Test-SealedGuardianIdentity',
    'Get-GuardianBootProbeSummary',
    'Get-GuardianBootProbeEventData',
    'Restore-SealedGuardianBackup',
    'New-GuardianInstallTransaction',
    'Invoke-GuardianInstallTransaction',
    'Rollback-GuardianInstallTransaction',
    'Install-GuardianPayload',
    'Assert-GuardianPayloadIdentity',
    'Test-GuardianPayloadIdentityMatch',
    'Restore-GuardianPayloadSnapshot',
    # One owner for the payload swap: bind the exact script path, action,
    # distro, policy SID, process instance, and payload hash before stopping
    # a prior watch; an unprovable running instance is a refusal.
    '$GuardianWatchInstancePath',
    'Test-GuardianWatchCommandLineMatch',
    'Get-GuardianWatchInstanceDecision',
    'Register-GuardianWatchInstance',
    'Remove-GuardianWatchInstanceRecord',
    'Invoke-GuardianWatchInstanceOwner',
    'guardian_watch_instance_already_running',
    'guardian_stale_watch_instance_not_reaped',
    'guardian_prior_watch_instance_unproven',
    'stop_prior',
    # DT-11/DT-12: one sealed windowless launcher and one absolute
    # interpreter; the action and the payload seal must name both.
    '$HostLauncherPath',
    'ramshared-host-launch.exe',
    '$WindowsPowerShellPath',
    'New-ScheduledTaskAction -Execute $HostLauncherPath',
    'launcher_sha256',
    'launcher_snapshot_path',
    'guardian_launcher_source_missing',
    '--timeout-ms',
    'Move-GuardianFileReplacing',
    'Test-GuardianProcessElevated',
    'Assert-GuardianProcessElevated',
    'guardian_install_requires_elevation',
    'guardian_payload_identity_mismatch',
    'payload.seal.json',
    'Register-ScheduledTask -TaskName $TaskName -Xml',
    'function Get-GuardianWslCommandPrefix',
    '$guardianWslPrefix = Get-GuardianWslCommandPrefix',
    'function Resolve-GuardianTaskUserName',
    '[System.Security.Principal.SecurityIdentifier]::new($Sid)',
    '$TaskUserName = Resolve-GuardianTaskUserName -Sid $UserSid',
    'New-ScheduledTaskTrigger -AtLogOn -User $TaskUserName',
    'New-ScheduledTaskTrigger -AtStartup',
    '-StartWhenAvailable',
    '-AllowStartIfOnBatteries',
    '-DontStopIfGoingOnBatteries',
    'function Protect-GuardianConsoleLifetime',
    'TreatControlCAsInput',
    'New-ScheduledTaskPrincipal -UserId $TaskUserName -LogonType Interactive -RunLevel Highest',
    '--terminate',
    'Ubuntu-24.04',
    'WaitForExit',
    'guardian-events.jsonl',
    'windows-telemetry.jsonl',
    'Get-SharedWslHostMemorySample',
    'physical_memory_total_kib',
    'physical_memory_available_kib',
    'commit_used_kib',
    'commit_available_kib',
    'pagefile_used_mib',
    'vmmem_wsl',
    'origin_volume',
    'ramshared-origin-manifest.json',
    'Get-Volume -FilePath',
    'volume_unique_id',
    '[Convert]::ToBase64String',
    'termination_already_recorded',
    'Complete-TerminationRecord',
    'new_boot_id',
    'Get-GuardianTerminationDecision',
    'guardian_policy',
    '-HeartbeatPath',
    '-ArtifactRoot',
    '-StaleAfterSec',
    '-PollSec',
    '-GuestCommandTimeoutSec',
    '-Action watch -Run'
)) {
    Require-GuardianContract -Text $required
}
if (-not $telemetrySource.Contains('Get-SharedWslHostMemorySample') -or
    -not $telemetrySource.Contains('commit_available_kib') -or
    $telemetrySource.Contains('FreeVirtualMemory')) {
    throw 'ramshared_wsl_guardian: host commit telemetry must use exact performance counters'
}
if ($telemetrySource.Contains('Get-Volume -DriveLetter I') -or $telemetrySource.Contains('drive_letter = "I"')) {
    throw 'ramshared_wsl_guardian: origin telemetry must discover the manifest physical volume'
}
if ($source.Contains('New-ScheduledTaskPrincipal -UserId $UserSid')) {
    throw 'ramshared_wsl_guardian: task scheduler principal must use the Windows account name, not the policy SID'
}
if ($source.Contains('[Security.Principal.WindowsIdentity]::GetCurrent().Name')) {
    throw 'ramshared_wsl_guardian: task scheduler account must resolve from the sealed policy SID'
}
if ($source.Contains('-d `"$Distro`"')) {
    throw 'ramshared_wsl_guardian: WSL distro arguments must not preserve literal quotes through ProcessStartInfo'
}

# The forbidden-action scan reads code, not comments: the guardian documents
# the `wsl.exe --shutdown` incident it closes, and a documentation mention is
# not the broad action.  The same stripping is used by the Move-Item check
# below, so both scans share one definition of "code".
$sourceWithoutBlockComments = $source -replace '(?s)<#.*?#>', ''
$sourceCodeOnly = (@($sourceWithoutBlockComments -split "`n" | ForEach-Object {
    $hashIndex = $_.IndexOf('#')
    if ($hashIndex -ge 0) { $_.Substring(0, $hashIndex) } else { $_ }
}) -join "`n")
foreach ($forbidden in @(
    'Ensure-OriginAttached',
    '--mount --vhd',
    'Get-DiskImage',
    '--shutdown',
    'Restart-Computer',
    'Stop-Computer',
    'shutdown.exe',
    'wsl.exe --terminate',
    'wsl.exe --shutdown',
    # A direct PowerShell action is the visible-console regression DT-11
    # closes; the launcher is the only program the task may name.
    'New-ScheduledTaskAction -Execute "PowerShell.exe"',
    'New-ScheduledTaskAction -Execute "powershell.exe"'
)) {
    if ($sourceCodeOnly.Contains($forbidden)) {
        throw "ramshared_wsl_guardian: forbidden host or broad WSL action $forbidden"
    }
}
# The payload-swap owner may stop only a process it bound by the sealed
# command line and owner SID. A name-wide stop would kill an unrelated
# PowerShell and would also miss the real guardian under a renamed image.
if ($source.Contains('Stop-Process -Name') -or $source -match 'Get-Process\s+-Name\s+[''"]?powershell') {
    throw 'ramshared_wsl_guardian: payload swap must never match a generic powershell.exe name'
}

# Move-Item cannot replace an existing destination file on Windows: it throws
# ERROR_ALREADY_EXISTS. Every write that lands on a path that may already
# exist must go through Move-GuardianFileReplacing. Comment mentions of the
# cmdlet are the one legitimate appearance. The token is delimited so that
# Remove-Item -- which contains the same letters -- is not mistaken for it.
foreach ($line in ($sourceCodeOnly -split "`n")) {
    $codePart = $line
    $hashIndex = $line.IndexOf('#')
    if ($hashIndex -ge 0) { $codePart = $line.Substring(0, $hashIndex) }
    if ($codePart -match '(?i)(?<![\w-])Move-Item(?![\w-])') {
        throw "ramshared_wsl_guardian: Move-Item cannot replace an existing destination; use Move-GuardianFileReplacing"
    }
}
if ($source -notmatch '\[IO\.File\]::Replace\(' -or $source -notmatch '\[IO\.File\]::Move\(') {
    throw "ramshared_wsl_guardian: file replacement must use the atomic File.Replace / File.Move pair"
}
# File.Replace's third argument is the backup path. Windows PowerShell
# coerces $null to "" for a string parameter, Path.GetFullPathInternal("")
# then throws ArgumentException "The path has an invalid format", and the
# failure reads like a bad path instead of a bad argument. The backup must
# be named, not absent.
if ($source -match '\[IO\.File\]::Replace\([^\)]*\$(?:null\b)') {
    throw "ramshared_wsl_guardian: File.Replace must not receive a null backup path; PowerShell coerces it to an empty string"
}
if ($source -notmatch '\[IO\.File\]::Replace\(\$Source, \$Destination, \$backup\)') {
    throw "ramshared_wsl_guardian: File.Replace must name an explicit transient backup path"
}

# Authority before effects. The install transaction mutates through backup,
# deploy and config before it reaches the register phase that needs the
# elevated token; if the check is not first, a non-elevated attempt leaves a
# half-applied transaction for a rollback that lacks the same authority.
if ($source -notmatch 'function Install-GuardianTaskTransaction \{(?:\s*#[^\n]*)*\s*Assert-GuardianProcessElevated -Operation "install"') {
    throw "ramshared_wsl_guardian: the install transaction must assert elevation before any effect"
}
if ($source -notmatch 'function Activate-GuardianTask \{(?:\s*#[^\n]*)*\s*Assert-GuardianProcessElevated -Operation "activation"') {
    throw "ramshared_wsl_guardian: activation must assert elevation before any effect"
}

foreach ($required in @(
    'RAMSHARED_ATTENDED_GUARDIAN_ACTION',
    'guardian action requires exact attended approval',
    'Disable-ScheduledTask -TaskName $TaskName',
    '-Action watch -Run'
)) {
    Require-GuardianContract -Text $required
}

# A timed child-process call must not turn into an unbounded cleanup wait after
# its deadline. The guardian has to return to its independent control loop even
# when a WSL client does not exit promptly after termination is requested.
if ($source -match '\$process\.WaitForExit\(\)') {
    throw "ramshared_wsl_guardian: timeout cleanup must not wait without a bound"
}
if ($source -notmatch '\$process\.Dispose\(\)') {
    throw "ramshared_wsl_guardian: timeout cleanup must release its process handle"
}
if ($source -notmatch '\.StandardOutput\.ReadToEndAsync\(\)' -or
    $source -notmatch '\.StandardError\.ReadToEndAsync\(\)' -or
    $source -notmatch 'Stop-GuardianProcessInstanceSafely\s+-Process\s+\$process' -or
    $source -notmatch 'process_instance_handle_terminated' -or
    $source -notmatch 'process_tree_terminated') {
    throw "ramshared_wsl_guardian: bounded clients must drain pipes and fail closed without numeric PID termination"
}
if ($source -match '(?m)^\s*(?:Stop-Process|taskkill\.exe)') {
    throw 'ramshared_wsl_guardian: watchdog may not race a numeric PID force-kill'
}

$powershell = Join-Path $PSHOME "powershell.exe"
if (-not (Test-Path -LiteralPath $powershell -PathType Leaf)) { $powershell = "powershell.exe" }
$currentUserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$manufactured = @(& $powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $target -Action test -Run -Distro "Manufactured-Ubuntu" -UserSid $currentUserSid -HeartbeatPath "C:\manufactured\heartbeat.json" -ArtifactRoot "C:\manufactured\artifacts" -StaleAfterSec 17 -PollSec 3 -GuestCommandTimeoutSec 7 2>&1)
if ($LASTEXITCODE -ne 0) {
    throw ("ramshared_wsl_guardian: manufactured decision cases failed: " + ($manufactured -join "`n"))
}
foreach ($required in @(
    'PASS healthy_guest_with_stale_monitor_never_terminates',
    'PASS guardian_refuses_when_guest_probe_is_responsive',
    'PASS guardian_refuses_when_host_probe_is_healthy',
    'PASS guardian_refuses_when_snapshot_is_open',
    'PASS guardian_terminates_only_after_all_gates',
    'PASS guardian_refuses_duplicate_termination',
    'PASS guardian_task_arguments_seal_distro_timeout_and_artifact_policy',
    'PASS guardian_terminates_exactly_once_and_enters_safe_mode',
    'PASS host_safe_mode_gate_survives_guardian_and_guest_restart',
    'PASS guardian_never_uses_wsl_shutdown_or_host_restart',
    'PASS guardian_only_terminates_the_sealed_distro_after_all_gates',
    'PASS guardian_timeout_cleanup_is_bounded',
    'PASS disabled_guardian_task_never_mutates_wsl_or_disk_at_logon',
    'PASS guardian_install_failure_rolls_back_all_phases',
    'PASS guardian_install_preserves_prior_task_config_and_backup',
    'PASS guardian_new_task_is_disabled_or_absent_after_failure',
    'PASS guardian_install_deploys_and_restores_the_payload',
    'PASS guardian_payload_is_deployed_before_the_task_is_registered',
    'PASS guardian_watch_accepts_the_sealed_payload_identity',
    'PASS guardian_watch_refuses_payload_identity_mismatch',
    'PASS guardian_payload_seal_hash_shape_is_enforced',
    'PASS guardian_task_action_binds_the_product_payload_path',
    'PASS guardian_task_action_uses_the_windowless_launcher',
    'PASS guardian_atomic_write_replaces_an_existing_destination'
    'PASS guardian_file_move_replaces_an_existing_destination'
    'PASS guardian_elevation_gate_passes_when_the_token_is_elevated'
    'PASS guardian_install_requires_elevation_before_any_mutation'
    'PASS guardian_activation_requires_elevation_before_any_mutation'
    'PASS one_timeout_or_single_guest_probe_refuses_termination'
    'PASS guardian_requires_two_distinct_guest_failures_and_dual_host_corroboration'
    'PASS boot_bound_healthy_proof_required_before_publish'
    'PASS guardian_console_control_is_not_a_way_to_end_the_gate'
    'PASS guardian_task_is_self_healing_across_logon_and_startup'
    'PASS guardian_task_missed_trigger_still_starts_when_available'
    'PASS guardian_task_is_never_stopped_by_a_power_source_change'
    'PASS guardian_ensure_starts_the_sealed_named_task'
    'PASS guardian_ensure_is_idempotent_while_the_task_is_running'
    'PASS guardian_ensure_refuses_a_disabled_task_without_enabling_it'
    'PASS guardian_ensure_refuses_an_absent_task_without_creating_it'
    'PASS guardian_ensure_never_writes_a_health_proof'
    'PASS guardian_ensure_validates_the_sealed_identity_before_any_host_mutation'
    'PASS guardian_staging_shape_refuses_the_task_scheduler_battery_defaults'
    'PASS guardian_activation_is_explicit_and_staging_remains_disabled'
    'PASS guardian_wsl_arguments_are_scheduler_safe'
    'PASS guardian_hcs_status_accepts_serialized_running_enum'
    'PASS guardian_hcs_status_json_is_normalized'
    'PASS guardian_boot_probe_summary_is_sanitized'
    'PASS guardian_boot_probe_event_payload_is_flat'
)) {
    if (-not ($manufactured -join "`n").Contains($required)) {
        throw "ramshared_wsl_guardian: manufactured output missing $required"
    }
}

foreach ($required in @(
    'Get-GuestProbeFailureCount',
    'Test-CanonicalGuestBootId',
    'Get-GuardianEnsureDecision',
    'guardian task is not registered; ensure starts the producer and never creates it',
    'guardian task definition is disabled; ensure never enables a task',
    'Invoke-GuardianWatchIteration',
    'guardian_watch_heartbeat_checked_before_healthy_publish',
    'guardian_watch_revalidates_guest_recovery_before_termination',
    'guardian_task_xml_fingerprint',
    'guardian_task_operator_edit_detected',
    'Invoke-GuardianWatchTerminationTail',
    'guardian_tail_refuses_recovered_guest',
    'guardian_task_registration_seal_pending_disable',
    'guardian_uninstall_unregister_failed',
    'Invoke-BoundedJsonQuery',
    'two_distinct_guest_probe_failures_required',
    'dual_wsl_hcs_corroboration_required',
    'schema_version = 3',
    'boot_id = $BootId'
    'guardian activation requires exact attended approval'
    'staging_capture_only = $true'
    'Stop-GuardianProcessInstanceSafely'
)) {
    Require-GuardianContract -Text $required
}
if ($source.Contains('-GuestFailed $true')) {
    throw 'ramshared_wsl_guardian: a precomputed guest-failure Boolean may not bypass probe aggregation'
}

# R4-HOST-01/02/09/10 RED: the source action test must drive the production
# watch iteration with injected observations, not merely manufacture a final
# decision Boolean.  These named outcomes are emitted only by that path.
foreach ($required in @(
    'PASS guardian_watch_heartbeat_checked_before_healthy_publish',
    'PASS guardian_watch_revalidates_guest_recovery_before_termination',
    'PASS guardian_task_xml_seal_refuses_operator_edit_and_preserves_backup',
    'PASS guardian_boot_id_rejects_noncanonical_guid'
    'PASS guardian_tail_refuses_recovered_guest_before_targeted_termination'
    'PASS guardian_register_success_disable_failure_restores_task_config_and_seal'
    'PASS guardian_uninstall_failure_retains_seal_and_state'
    'PASS guardian_activation_export_failure_restores_exact_task_seal_and_operator_config'
    'PASS guardian_pid_reuse_never_signals_foreign_process'
)) {
    if (-not ($manufactured -join "`n").Contains($required)) {
        throw "ramshared_wsl_guardian: production-flow output missing $required"
    }
}

Write-Output "PASS guardian_task_action_uses_the_windowless_launcher"
Write-Output "PASS guardian_static_contract"
