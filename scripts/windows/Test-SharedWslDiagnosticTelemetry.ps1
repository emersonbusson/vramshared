#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-SharedWslDiagnosticTelemetry.ps1
param()
$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'SharedWslHostMemoryGate.psm1') -Force

$observer = New-SharedWslTelemetryObserver -Probe {
    Start-Sleep -Milliseconds 1500
    return @{ schema_version = 3; marker = 'actual_runspace_result' }
}
try {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    for ($i = 0; $i -lt 10; $i++) {
        $poll = [Diagnostics.Stopwatch]::StartNew()
        $result = Receive-SharedWslTelemetry -Observer $observer
        if ($poll.ElapsedMilliseconds -gt 250) { throw 'slow_diagnostics_blocked_guardian_poll' }
        if ($result.state -ne 'PENDING') { throw 'slow_collector_reported_a_premature_result' }
        Start-Sleep -Milliseconds 50
    }
    if ($clock.ElapsedMilliseconds -ge 1400) { throw 'pending_collector_waited_in_the_guardian_thread' }
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    do {
        $result = Receive-SharedWslTelemetry -Observer $observer
        if ($result.state -ne 'PENDING') { break }
        Start-Sleep -Milliseconds 50
    } while ($deadline.ElapsedMilliseconds -lt 5000)
    if ($result.state -ne 'COMPLETED' -or $result.data.marker -ne 'actual_runspace_result') {
        throw 'completed_collector_result_not_delivered'
    }
    if (-not $observer.disposed) { throw 'completed_collector_resources_not_disposed' }
} finally { Stop-SharedWslTelemetry -Observer $observer }

$failed = New-SharedWslTelemetryObserver -Probe { throw 'fixture_read_failure' }
try {
    $deadline.Restart()
    do {
        $result = Receive-SharedWslTelemetry -Observer $failed
        if ($result.state -ne 'PENDING') { break }
        Start-Sleep -Milliseconds 25
    } while ($deadline.ElapsedMilliseconds -lt 5000)
    if ($result.state -ne 'FAILED' -or $null -ne $result.data) { throw 'failed_diagnostics_fabricated_a_result' }
} finally { Stop-SharedWslTelemetry -Observer $failed }
$cancelled = New-SharedWslTelemetryObserver -Probe { Start-Sleep -Seconds 30; return @{ marker = 'should_not_publish' } }
try {
    $stopClock = [Diagnostics.Stopwatch]::StartNew()
    Stop-SharedWslTelemetry -Observer $cancelled
    Stop-SharedWslTelemetry -Observer $cancelled
    if ($stopClock.ElapsedMilliseconds -gt 250 -or -not $cancelled.stop_requested) {
        throw 'diagnostic_stop_blocks_guardian_or_is_not_idempotent'
    }
    $deadline.Restart()
    do {
        $result = Receive-SharedWslTelemetry -Observer $cancelled
        if ($result.state -ne 'PENDING') { break }
        Start-Sleep -Milliseconds 25
    } while ($deadline.ElapsedMilliseconds -lt 5000)
    if ($result.state -ne 'FAILED' -or -not $cancelled.disposed) { throw 'cancelled_diagnostic_resources_or_result_unproven' }
} finally { Stop-SharedWslTelemetry -Observer $cancelled }
Write-Host 'SHARED_WSL_DIAGNOSTIC_TELEMETRY=PASS'

$watchPath = Join-Path $PSScriptRoot 'Watch-RamSharedWsl.ps1'
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($watchPath, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'guardian_source_parser_error' }
$step = $ast.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -ceq 'Invoke-GuardianTelemetryStep' }, $true)
Invoke-Expression $step.Extent.Text
$script:starts = 0
function New-SharedWslTelemetryObserver { param([string]$TelemetryPath) $script:starts++; return @{ disposed = $false } }
function Receive-SharedWslTelemetry { param($Observer) return @{ state = 'PENDING'; data = $null } }
$context = @{ observer = $null }
1..10 | ForEach-Object { Invoke-GuardianTelemetryStep -Context $context -Path 'fixture-only' }
if ($script:starts -ne 1) { throw 'guardian_created_duplicate_pending_collectors' }
function Receive-SharedWslTelemetry { param($Observer) throw 'fixture_diagnostic_failure' }
Invoke-GuardianTelemetryStep -Context $context -Path 'fixture-only'
if ($context.last_error -ne 'diagnostic_collection_or_publication_failed') {
    throw 'diagnostic_failure_not_contained_in_observer'
}
$watch = $ast.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -ceq 'Invoke-GuardianWatch' }, $true).Extent.Text
if ($watch.Contains('Write-HostTelemetryRing -Path') -or $watch.Contains('Get-HostTelemetry')) {
    throw 'synchronous_diagnostics_remain_in_guardian_heartbeat_loop'
}
Write-Host 'GUARDIAN_DIAGNOSTICS_SINGLE_OBSERVER=PASS'
