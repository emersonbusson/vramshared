# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/SharedWslPressureOutcome.psm1
Set-StrictMode -Version Latest

# The status document's current schema. The cleanup and live gates read the
# same document the CLI emits, so a schema bump is one value here and one
# fixture per test, never a silent gate that answers false.
$script:PressureHealthSchemaVersion = 5

function Test-PressureBoolean {
    param([AllowNull()][object]$Value, [bool]$Expected)
    return ($Value -is [bool] -and $Value -eq $Expected)
}

function Test-PressureInteger {
    param([AllowNull()][object]$Value, [long]$Expected)
    return (($Value -is [int] -or $Value -is [long]) -and $Value -eq $Expected)
}

function Test-PressureSupervisorIdentity {
    param([object]$Left, [object]$Right)
    foreach ($field in @('boot_id', 'nonce')) {
        if ($Left.$field -isnot [string] -or $Right.$field -isnot [string] -or
            [string]::IsNullOrWhiteSpace($Left.$field) -or $Left.$field -cne $Right.$field) { return $false }
    }
    foreach ($field in @('pid', 'start_time')) {
        if (($Left.$field -isnot [int] -and $Left.$field -isnot [long]) -or $Left.$field -le 0 -or
            -not (Test-PressureInteger $Right.$field $Left.$field)) { return $false }
    }
    return $true
}

function Test-SharedWslPressureCleanup {
    [CmdletBinding()]
    param(
        [AllowNull()][object]$Health,
        [AllowEmptyString()][string]$SwapSnapshot
    )
    try {
        if (-not (Test-PressureInteger $Health.schema_version $script:PressureHealthSchemaVersion)) { return $false }
        if ($Health.phase -cne 'Off' -or $Health.cache_state -cne 'OFF' -or
            $Health.origin_state -cne 'OFF' -or $Health.protection_state -cne 'OFF') {
            return $false
        }
        if (-not (Test-PressureBoolean $Health.topology_ok $true) -or
            -not (Test-PressureBoolean $Health.ghost $false) -or
            -not (Test-PressureBoolean $Health.activation.active $false) -or
            -not (Test-PressureBoolean $Health.daemon.alive $false)) { return $false }
        if ($null -eq $Health.daemon.PSObject.Properties['pid'] -or
            $null -ne $Health.daemon.pid) { return $false }
        if ($Health.measurement_errors -isnot [array] -or
            $Health.measurement_errors.Count -ne 0) { return $false }
        foreach ($name in @('zram', 'vram')) {
            $tier = $Health.tiers.$name
            if (-not (Test-PressureBoolean $tier.present $false)) { return $false }
            # An absent tier publishes `null` for both figures, never a
            # measured `0`: `0` is the occupancy of a present tier holding
            # nothing. A numeric figure on an absent tier is a retained
            # occupancy and is refused; a missing key is not what the product
            # publishes and is refused too.
            foreach ($field in @('size_kib', 'used_kib')) {
                if ($null -eq $tier.PSObject.Properties[$field] -or
                    $null -ne $tier.$field) { return $false }
            }
        }
        # An inactive product has ok=false. Prove managed absence separately
        # from readiness to operate an active product.
        $rows = @($SwapSnapshot.Trim() -split '\r?\n')
        if ($rows.Count -lt 2 -or
            $rows[0] -notmatch '^Filename\s+Type\s+Size\s+Used\s+Priority$') {
            return $false
        }
        foreach ($row in $rows[1..($rows.Count - 1)]) {
            $fields = @($row.Trim() -split '\s+')
            if ($fields.Count -ne 5 -or $fields[1] -notmatch '^(partition|file)$' -or
                $fields[0] -match '^/dev/(nbd|zram)[0-9]+$' -or
                $fields[2] -notmatch '^[0-9]+$' -or $fields[3] -notmatch '^[0-9]+$' -or
                $fields[4] -notmatch '^-?[0-9]+$') { return $false }
            foreach ($value in $fields[2..4]) {
                $parsed = [long]0
                if (-not [long]::TryParse($value, [ref]$parsed)) { return $false }
            }
        }
        return $true
    } catch {
        return $false
    }
}

function Test-SharedWslPressureLiveHealth {
    param([AllowNull()][object]$Health, [object[]]$ControlWitness = @())
    try {
        $protectedState = $Health.control_state -ceq 'GUARDED' -or $Health.control_state -ceq 'CRITICAL'
        $controlOk = $Health.control_state -ceq 'HEALTHY' -and
            ((Test-PressureBoolean $Health.ok $true) -or
            ($Health.origin_state -ceq 'READY' -and $Health.cache_state -ceq 'OFF'))
        if ($protectedState) {
            $controlOk = Test-SharedWslClosedAdmissionProof -Health $Health -Witnesses $ControlWitness
        }
        return ((Test-PressureInteger $Health.schema_version $script:PressureHealthSchemaVersion) -and
            $controlOk -and
            (Test-PressureBoolean $Health.ghost $false) -and
            (Test-PressureBoolean $Health.topology_ok $true) -and
            (Test-PressureBoolean $Health.activation.active $true) -and
            (Test-PressureBoolean $Health.daemon.alive $true) -and
            ($Health.daemon.pid -is [int] -or $Health.daemon.pid -is [long]) -and $Health.daemon.pid -gt 0 -and
            $Health.origin_state -ceq 'READY' -and
            $Health.guardian_state -ceq 'HEALTHY' -and
            $Health.measurement_errors -is [array] -and
            $Health.measurement_errors.Count -eq 0)
    } catch { return $false }
}

function Test-SharedWslClosedAdmissionProof {
    param([object]$Health, [object[]]$Witnesses)
    foreach ($proof in $Witnesses) {
        try {
            foreach ($time in @($proof.epoch_ms, $Health.epoch_ms,
                $proof.supervisor.written_at_unix_ms, $proof.admission.written_at_unix_ms)) {
                if (($time -isnot [int] -and $time -isnot [long]) -or $time -le 0) { throw 'invalid_evidence_timestamp' }
            }
            if ([Math]::Abs($proof.epoch_ms - $Health.epoch_ms) -gt 2000) { continue }
            $supervisor = $proof.supervisor; $admission = $proof.admission
            if ($proof.errors -isnot [array] -or $proof.errors.Count -ne 0 -or
                -not (Test-PressureInteger $supervisor.schema_version 3) -or
                -not (Test-PressureInteger $admission.schema_version 1) -or $supervisor.control_state -cne $Health.control_state -or
                $admission.control_state -cne $Health.control_state -or
                -not (Test-PressureBoolean $admission.admission_open $false)) { continue }
            if ([Math]::Abs([double]$proof.epoch_ms - [double]$supervisor.written_at_unix_ms) -gt 5000 -or
                [Math]::Abs([double]$proof.epoch_ms - [double]$admission.written_at_unix_ms) -gt 5000) { continue }
            if (-not (Test-PressureSupervisorIdentity $supervisor.supervisor_identity $admission.supervisor_identity)) { continue }
            if ($supervisor.daemon_instance_id -isnot [string] -or
                $supervisor.daemon_instance_id -cnotmatch ('^' + $Health.daemon.pid + '-[1-9][0-9]*$')) { continue }
            if ($supervisor.action_results -isnot [array]) { continue }
            $actions = @($supervisor.action_results)
            $ordinaryActions = @($actions)
            if ($Health.control_state -ceq 'CRITICAL') {
                if (-not (Test-PressureCriticalFreezeEvidence -Proof $proof -Actions $actions)) { continue }
                $ordinaryActions = @($actions | Where-Object { $_.action -cne 'freeze_discardable' })
            }
            if (@($ordinaryActions | Where-Object { $_.status -cne 'succeeded' -or $null -ne $_.error }).Count) { continue }
            $required = @('close_admission')
            if ($Health.control_state -ceq 'CRITICAL') { $required += @('reduce_vram_cache', 'request_reclaim') }
            $complete = $true
            foreach ($action in $required) {
                if (@($actions | Where-Object { $_.action -ceq $action -and $_.status -ceq 'succeeded' }).Count -ne 1) { $complete = $false }
            }
            if ($complete) { return $true }
        } catch { continue }
    }
    return $false
}

function Test-PressureCriticalFreezeEvidence {
    param([object]$Proof, [object[]]$Actions)
    $freeze = @($Actions | Where-Object { $_.action -ceq 'freeze_discardable' })
    if ($freeze.Count -eq 1 -and $freeze[0].status -ceq 'succeeded' -and $null -eq $freeze[0].error) { return $true }
    if ($freeze.Count -eq 1 -and $freeze[0].status -ceq 'failed' -and $freeze[0].error -ceq 'no_discardable_scope') {
        return ((Test-PressureInteger $Proof.reservations.schema_version 3) -and
            ($Proof.reservations.next_ordinal -is [int] -or $Proof.reservations.next_ordinal -is [long]) -and
            $Proof.reservations.next_ordinal -gt 0 -and
            $Proof.reservations.reservations -is [array] -and $Proof.reservations.reservations.Count -eq 0 -and
            $null -eq $Proof.frozen_scope -and $null -eq $Proof.frozen_scope_live)
    }
    if ($freeze.Count -ne 0) { return $false }
    $record = $Proof.frozen_scope; $live = $Proof.frozen_scope_live
    return ((Test-PressureInteger $record.schema_version 3) -and $record.phase -ceq 'applied' -and
        ($record.updated_at_unix_ms -is [int] -or $record.updated_at_unix_ms -is [long]) -and
        $record.updated_at_unix_ms -gt 0 -and $record.updated_at_unix_ms -le $Proof.epoch_ms -and
        $record.unit -cmatch '^ramshared-(batch|build|browser-test)-[A-Za-z0-9_-]+\.scope$' -and
        $record.invocation_id -cmatch '^[0-9a-f]{32}$' -and
        $live.Id -ceq $record.unit -and $live.InvocationID -ceq $record.invocation_id -and
        $live.FreezerState -ceq 'frozen' -and $live.ActiveState -ceq 'active' -and
        $live.ControlGroup -ceq ('/ramshared.slice/ramshared-workloads.slice/' + $record.unit))
}

function Test-SharedWslPressureControlEvidence {
    param(
        [AllowNull()][object[]]$PhaseHealth,
        [AllowNull()][object[]]$MonitorHealth,
        [object[]]$ControlWitness = @(),
        [int]$ExpectedPhases = 4
    )
    try {
        if ($ExpectedPhases -lt 2 -or $PhaseHealth.Count -ne $ExpectedPhases -or
            $MonitorHealth.Count -lt 1) { return $false }
        foreach ($health in $PhaseHealth) {
            if (-not (Test-SharedWslPressureLiveHealth $health -ControlWitness $ControlWitness) -or
                ($health.epoch_ms -isnot [int] -and $health.epoch_ms -isnot [long]) -or
                $health.epoch_ms -le 0) { return $false }
        }
        $start = ($PhaseHealth | Measure-Object epoch_ms -Minimum).Minimum
        $end = ($PhaseHealth | Measure-Object epoch_ms -Maximum).Maximum
        if ($end -le $start) { return $false }
        $previous = $start
        $observed = 0
        foreach ($health in $MonitorHealth) {
            if (-not (Test-PressureInteger $health.schema_version $script:PressureHealthSchemaVersion) -or
                $health.activation.active -isnot [bool] -or
                ($health.epoch_ms -isnot [int] -and $health.epoch_ms -isnot [long]) -or
                $health.epoch_ms -le 0) { return $false }
            if ($health.epoch_ms -lt $start -or $health.epoch_ms -gt $end) { continue }
            if ($health.epoch_ms -lt $previous -or $health.epoch_ms - $previous -gt 15000 -or
                -not (Test-SharedWslPressureLiveHealth $health -ControlWitness $ControlWitness)) { return $false }
            $previous = $health.epoch_ms
            $observed++
        }
        return ($observed -gt 0 -and $end - $previous -le 15000)
    } catch { return $false }
}

function Get-SharedWslPressureControlEvidence {
    param([string]$ArtifactDir, [int]$Rounds)
    try {
        $phaseHealth = @(foreach ($round in 1..$Rounds) {
            foreach ($phase in @('before', 'after')) {
                $path = Join-Path $ArtifactDir "campaign\round-$round\$phase-health.json"
                Get-Content -LiteralPath $path -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
            }
        })
        $monitorHealth = @(Get-Content -LiteralPath (Join-Path $ArtifactDir 'cascade-health.jsonl') -ErrorAction Stop |
            ForEach-Object { $_ | ConvertFrom-Json -ErrorAction Stop })
        $witnesses = @(Get-Content -LiteralPath (Join-Path $ArtifactDir 'control-witness.jsonl') -ErrorAction Stop |
            ForEach-Object { $_ | ConvertFrom-Json -ErrorAction Stop })
        return (Test-SharedWslPressureControlEvidence -PhaseHealth $phaseHealth `
            -MonitorHealth $monitorHealth -ControlWitness $witnesses -ExpectedPhases ($Rounds * 2))
    } catch { return $false }
}

Export-ModuleMember -Function Test-SharedWslPressureCleanup, Test-SharedWslPressureControlEvidence, Get-SharedWslPressureControlEvidence
