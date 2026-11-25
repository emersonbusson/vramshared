#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-SharedWslPressureOutcome.ps1
param()
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'SharedWslPressureOutcome.psm1') -Force

function New-CleanHealth {
    return (@'
{"schema_version":5,"phase":"Off","ok":false,"control_state":"GUARDED","protection_state":"OFF","cache_state":"OFF","origin_state":"OFF","topology_ok":true,"ghost":false,"measurement_errors":[],"activation":{"active":false},"daemon":{"alive":false,"pid":null},"tiers":{"zram":{"present":false,"size_kib":null,"used_kib":null},"vram":{"present":false,"size_kib":null,"used_kib":null}}}
'@ | ConvertFrom-Json)
}
$swaps = "Filename`tType`tSize`tUsed`tPriority`n/dev/sdc`tpartition`t8388608`t0`t-2`n"
if (-not (Test-SharedWslPressureCleanup -Health (New-CleanHealth) -SwapSnapshot $swaps)) {
    throw 'current_schema_off_is_clean_even_when_active_readiness_is_false'
}
$cases = @(
    @{ name = 'unknown_schema'; change = { param($h) $h.schema_version = 6 } },
    @{ name = 'string_schema'; change = { param($h) $h.schema_version = '4' } },
    @{ name = 'active_phase'; change = { param($h) $h.phase = 'Armed' } },
    @{ name = 'active_binding'; change = { param($h) $h.activation.active = $true } },
    @{ name = 'ghost'; change = { param($h) $h.ghost = $true } },
    @{ name = 'missing_ghost'; change = { param($h) $h.PSObject.Properties.Remove('ghost') } },
    @{ name = 'string_boolean'; change = { param($h) $h.ghost = 'false' } },
    @{ name = 'bad_topology'; change = { param($h) $h.topology_ok = $false } },
    @{ name = 'live_daemon'; change = { param($h) $h.daemon.alive = $true } },
    @{ name = 'retained_pid'; change = { param($h) $h.daemon.pid = 42 } },
    @{ name = 'missing_pid'; change = { param($h) $h.daemon.PSObject.Properties.Remove('pid') } },
    @{ name = 'cache_active'; change = { param($h) $h.cache_state = 'ACTIVE' } },
    @{ name = 'origin_active'; change = { param($h) $h.origin_state = 'READY' } },
    @{ name = 'protection_active'; change = { param($h) $h.protection_state = 'READY' } },
    @{ name = 'missing_errors'; change = { param($h) $h.PSObject.Properties.Remove('measurement_errors') } },
    @{ name = 'measurement_error'; change = { param($h) $h.measurement_errors = @('unavailable') } },
    @{ name = 'present_zram'; change = { param($h) $h.tiers.zram.present = $true } },
    @{ name = 'present_nbd'; change = { param($h) $h.tiers.vram.present = $true } },
    @{ name = 'retained_nbd_size'; change = { param($h) $h.tiers.vram.size_kib = 4096 } },
    @{ name = 'retained_zram_used'; change = { param($h) $h.tiers.zram.used_kib = 1 } },
    @{ name = 'string_size'; change = { param($h) $h.tiers.zram.size_kib = '0' } },
    @{ name = 'missing_size'; change = { param($h) $h.tiers.zram.PSObject.Properties.Remove('size_kib') } }
)
foreach ($case in $cases) {
    $health = New-CleanHealth
    & $case.change $health
    if (Test-SharedWslPressureCleanup -Health $health -SwapSnapshot $swaps) {
        throw "cleanup_counterexample_accepted:$($case.name)"
    }
}
foreach ($bad in @(
    '', 'garbage', "Filename`tType`tSize`tUsed`tPriority`n",
    ($swaps + "/dev/nbd0`tpartition`t4096`t0`t100`n"),
    ($swaps + "/dev/zram0`tpartition`t4096`t0`t200`n"),
    ($swaps + "/tmp/swap (deleted)`tfile`t4096`t0`t-1`n"),
    ($swaps + "/tmp/swap`tfile`twrong`t0`t-1`n"),
    ($swaps + "/tmp/swap`tfile`t4096`t0`n")
)) {
    if (Test-SharedWslPressureCleanup -Health (New-CleanHealth) -SwapSnapshot $bad) {
        throw 'invalid_or_managed_swap_snapshot_accepted'
    }
}
if (Test-SharedWslPressureCleanup -Health $null -SwapSnapshot $swaps) {
    throw 'missing_health_accepted'
}
Write-Host 'SHARED_WSL_PRESSURE_OUTCOME=PASS cases=32'

function New-LiveHealth {
    param([long]$Epoch = 2500)
    $health = New-CleanHealth
    $health.ok = $true
    $health.activation.active = $true
    $health.daemon.alive = $true
    $health.daemon.pid = 42
    $health.control_state = 'HEALTHY'
    $health.origin_state = 'READY'
    $health.cache_state = 'ACTIVE'
    $health | Add-Member NoteProperty guardian_state 'HEALTHY'
    $health | Add-Member NoteProperty overall_state 'HEALTHY'
    $health | Add-Member NoteProperty epoch_ms $Epoch
    return $health
}
$phases = @(New-LiveHealth 1000; New-LiveHealth 2000; New-LiveHealth 3000; New-LiveHealth 4000)
if (-not (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @((New-LiveHealth)))) {
    throw 'complete_current_control_evidence_rejected'
}
foreach ($case in @('guarded', 'stale_guardian', 'measurement_error', 'missing_schema', 'inactive_phase')) {
    $health = New-LiveHealth
    switch ($case) {
        guarded { $health.control_state = 'GUARDED' }
        stale_guardian { $health.guardian_state = 'STALE' }
        measurement_error { $health.measurement_errors = @('supervisor_status_not_current') }
        missing_schema { $health.PSObject.Properties.Remove('schema_version') }
        inactive_phase { $health.activation.active = $false }
    }
    if (Test-SharedWslPressureControlEvidence -PhaseHealth @($health, $phases[1], $phases[2], $phases[3]) -MonitorHealth $phases) {
        throw "incomplete_control_phase_accepted:$case"
    }
    if (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($health)) {
        throw "invalid_continuous_control_sample_accepted:$case"
    }
}
if (Test-SharedWslPressureControlEvidence -PhaseHealth @($phases[0]) -MonitorHealth $phases) {
    throw 'missing_control_phases_accepted'
}
if (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @()) {
    throw 'missing_continuous_control_capture_accepted'
}
$outside = New-CleanHealth
$outside | Add-Member NoteProperty epoch_ms ([long]500)
if (-not (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($outside, $phases[1], $phases[2]))) {
    throw 'pre_action_off_sample_falsely_blocks_actual_control_window'
}
$longPhases = @($phases[0], $phases[1], (New-LiveHealth 20000), (New-LiveHealth 30000))
if (Test-SharedWslPressureControlEvidence -PhaseHealth $longPhases -MonitorHealth @($phases[1])) {
    throw 'missing_control_interval_exceeds_existing_guardian_freshness_window'
}
Write-Host 'SHARED_WSL_PRESSURE_CONTROL_EVIDENCE=PASS'

$guarded = New-LiveHealth 2500
$guarded.ok = $false; $guarded.control_state = 'GUARDED'; $guarded.overall_state = 'BLOCKED'
$identity = @{ boot_id = 'fixture-boot'; nonce = 'fixture-nonce'; pid = 42; start_time = 100 }
$proof = [pscustomobject]@{
    epoch_ms = 2500; errors = @()
    supervisor = @{ schema_version = 3; control_state = 'GUARDED'; daemon_instance_id = '42-123'; written_at_unix_ms = 2500; supervisor_identity = $identity; action_results = @(@{ action = 'close_admission'; status = 'succeeded'; error = $null }) }
    admission = @{ schema_version = 1; control_state = 'GUARDED'; admission_open = $false; written_at_unix_ms = 2500; supervisor_identity = $identity }
}
if (-not (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($guarded) -ControlWitness @($proof))) {
    throw 'correct_protective_transition_with_closed_admission_rejected'
}
foreach ($mutant in @('open', 'failed', 'stale', 'foreign', 'string-schema', 'null-time', 'string-time', 'null-errors', 'missing-actions', 'string-identity', 'foreign-daemon')) {
    $bad = $proof | ConvertTo-Json -Depth 8 | ConvertFrom-Json
    switch ($mutant) {
        open { $bad.admission.admission_open = $true }
        failed { $bad.supervisor.action_results[0].status = 'failed' }
        stale { $bad.supervisor.written_at_unix_ms = -99999 }
        foreign { $bad.admission.supervisor_identity.nonce = 'foreign' }
        string-schema { $bad.supervisor.schema_version = '3' }
        null-time { $bad.epoch_ms = $null }
        string-time { $bad.epoch_ms = '2500' }
        null-errors { $bad.errors = $null }
        missing-actions { $bad.supervisor.action_results = $null }
        string-identity { $bad.admission.supervisor_identity.pid = '42' }
        foreign-daemon { $bad.supervisor.daemon_instance_id = '43-123' }
    }
    if (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($guarded) -ControlWitness @($bad)) {
        throw "unsafe_protective_transition_accepted:$mutant"
    }
}
Write-Host 'SHARED_WSL_PROTECTIVE_CONTROL=PASS'

$reordered = $proof | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$reordered.admission.supervisor_identity = [pscustomobject][ordered]@{ start_time = 100; pid = 42; nonce = 'fixture-nonce'; boot_id = 'fixture-boot' }
if (-not (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($guarded) -ControlWitness @($reordered))) {
    throw 'identity_comparison_depends_on_json_key_order'
}

$fallback = New-LiveHealth 2500
$fallback.ok = $false
$fallback.overall_state = 'BLOCKED'
$fallback.origin_state = 'READY'
$fallback.cache_state = 'OFF'
if (-not (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($fallback))) {
    throw 'verified_authoritative_origin_fallback_falsely_requires_cache_allocation'
}
$fallback.origin_state = 'FAILED'
if (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($fallback)) {
    throw 'failed_origin_fallback_accepted'
}
$critical = $guarded | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$critical.control_state = 'CRITICAL'
$criticalProof = $proof | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$criticalProof.supervisor.control_state = 'CRITICAL'
$criticalProof.admission.control_state = 'CRITICAL'
if (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($critical) -ControlWitness @($criticalProof)) {
    throw 'critical_without_required_protective_actions_accepted'
}
foreach ($action in @('reduce_vram_cache', 'freeze_discardable', 'request_reclaim')) {
    $criticalProof.supervisor.action_results += [pscustomobject]@{ action = $action; status = 'succeeded'; error = $null }
}
if (-not (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($critical) -ControlWitness @($criticalProof))) {
    throw 'verified_critical_protection_with_live_origin_rejected'
}
Write-Host 'SHARED_WSL_ORIGIN_FALLBACK_AND_CRITICAL_CONTROL=PASS'

$held = $criticalProof | ConvertTo-Json -Depth 10 | ConvertFrom-Json
$held.supervisor.action_results = @($held.supervisor.action_results | Where-Object { $_.action -cne 'freeze_discardable' })
$unit = 'ramshared-batch-fixture.scope'
$held | Add-Member NoteProperty frozen_scope ([pscustomobject]@{ schema_version = 3; phase = 'applied'; unit = $unit; invocation_id = ('a' * 32); updated_at_unix_ms = 2000 })
$held | Add-Member NoteProperty frozen_scope_live ([pscustomobject]@{ Id = $unit; InvocationID = ('a' * 32); FreezerState = 'frozen'; ActiveState = 'active'; ControlGroup = ('/ramshared.slice/ramshared-workloads.slice/' + $unit) })
if (-not (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($critical) -ControlWitness @($held))) {
    throw 'verified_live_applied_freeze_requires_duplicate_effect'
}
foreach ($case in @('thawed', 'foreign', 'pending', 'future', 'missing')) {
    $bad = $held | ConvertTo-Json -Depth 10 | ConvertFrom-Json
    switch ($case) {
        thawed { $bad.frozen_scope_live.FreezerState = 'running' }
        foreign { $bad.frozen_scope_live.InvocationID = ('b' * 32) }
        pending { $bad.frozen_scope.phase = 'pending' }
        future { $bad.frozen_scope.updated_at_unix_ms = 99999 }
        missing { $bad.frozen_scope_live = $null }
    }
    if (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($critical) -ControlWitness @($bad)) { throw "invalid_persistent_freeze_accepted:$case" }
}
$empty = $criticalProof | ConvertTo-Json -Depth 10 | ConvertFrom-Json
$freeze = $empty.supervisor.action_results | Where-Object { $_.action -ceq 'freeze_discardable' }
$freeze.status = 'failed'; $freeze.error = 'no_discardable_scope'
if (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($critical) -ControlWitness @($empty)) { throw 'no_victim_refusal_without_authoritative_absence_proof_accepted' }
$empty | Add-Member NoteProperty reservations ([pscustomobject]@{ schema_version = 3; next_ordinal = 2; reservations = @() })
$empty | Add-Member NoteProperty frozen_scope $null
$empty | Add-Member NoteProperty frozen_scope_live $null
if (-not (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($critical) -ControlWitness @($empty))) { throw 'proved_no_victim_safe_refusal_rejected' }
$empty.reservations.reservations = @([pscustomobject]@{ unit = $unit })
if (Test-SharedWslPressureControlEvidence -PhaseHealth $phases -MonitorHealth @($critical) -ControlWitness @($empty)) { throw 'nonempty_ledger_treated_as_no_victim' }
Write-Host 'SHARED_WSL_PERSISTENT_FREEZE_AND_NO_VICTIM_EVIDENCE=PASS'
