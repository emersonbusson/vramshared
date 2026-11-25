#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-NativeGpuIdentity.ps1
# Verification adapter for the compiled Rust test; product behavior stays in Rust.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$TestExecutable,
    [Parameter(Mandatory = $true)][string]$ExpectedTestSha256,
    [Parameter(Mandatory = $true)][string]$ExpectedProductSha256,
    [Parameter(Mandatory = $true)][string]$ArtifactDirectory,
    [ValidateSet("public_native_identity_command_matches_its_owned_windows_process", "public_supervised_identity_matches_native_child_and_repeats", "native_vm_probe_binds_actual_child_and_repeated_sampler_observations", "native_kernel_peer_matches_current_guest_endpoint", "native_vm_permission_failure_is_observable_without_authority_or_abandoned_custody", "native_owned_control_plane_stops_active_guest", "native_sampler::tests::native_sampler_panic_recovers_retained_child_custody", "native_scm_watchdog_observes_probe_and_joins")]
    [string]$TestCase = "public_native_identity_command_matches_its_owned_windows_process",
    [string]$ExpectedVmId = "",
    [uint32]$ExpectedPort = 0
)
$ErrorActionPreference = "Stop"
$isVm = $TestCase -eq "native_vm_probe_binds_actual_child_and_repeated_sampler_observations"
$isEndpoint = $TestCase -eq "native_kernel_peer_matches_current_guest_endpoint"
$isOwned = $TestCase -eq "native_owned_control_plane_stops_active_guest"
$isSamplerFailure = $TestCase -eq "native_sampler::tests::native_sampler_panic_recovers_retained_child_custody"
$isWatchdog = $TestCase -eq "native_scm_watchdog_observes_probe_and_joins"
$isVmRefusal = $TestCase -eq "native_vm_permission_failure_is_observable_without_authority_or_abandoned_custody"
if ($isVm -or $isEndpoint -or $isOwned) {
    $parsedVm = [Guid]::Empty
    if (-not [Guid]::TryParse($ExpectedVmId, [ref]$parsedVm) -or $parsedVm -eq [Guid]::Empty) { throw "Native VM control requires the exact current guest runtime GUID" }
}
if (($isEndpoint -or $isOwned) -and ($ExpectedPort -eq 0 -or $ExpectedPort -gt [int]::MaxValue)) { throw "Native endpoint requires a bounded current guest port" }
function Pass([string]$Name) { Write-Output "PASS $Name" }
function Assert-Hash([string]$Path, [string]$Expected) {
    if ($Expected -notmatch '^[a-fA-F0-9]{64}$' -or
        (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -ine $Expected) {
        throw "Native test artifact hash mismatch: $Path"
    }
}
if (-not [IO.Path]::IsPathRooted($TestExecutable) -or
    -not [IO.Path]::IsPathRooted($ArtifactDirectory) -or
    (Test-Path -LiteralPath $ArtifactDirectory)) {
    throw "Require an absolute compiled test and a new absolute artifact directory"
}
$TestExecutable = (Resolve-Path -LiteralPath $TestExecutable).ProviderPath
$product = Join-Path (Split-Path (Split-Path $TestExecutable -Parent) -Parent) "ramshared-winsvc.exe"
Assert-Hash $TestExecutable $ExpectedTestSha256
Assert-Hash $product $ExpectedProductSha256
New-Item -Path $ArtifactDirectory -ItemType Directory -ErrorAction Stop | Out-Null
$stdout = Join-Path $ArtifactDirectory "stdout.log"
$stderr = Join-Path $ArtifactDirectory "stderr.log"
$receipt = Join-Path $ArtifactDirectory "custody.json"
$process = New-Object System.Diagnostics.Process
$process.StartInfo.FileName = $TestExecutable
$process.StartInfo.WorkingDirectory = Split-Path $TestExecutable -Parent
$process.StartInfo.Arguments = "$TestCase --ignored --exact --nocapture --test-threads=1"
$process.StartInfo.UseShellExecute = $false
if ($isVm -or $isEndpoint -or $isOwned) { $process.StartInfo.EnvironmentVariables["RAMSHARED_NATIVE_EXPECTED_VM_ID"] = $ExpectedVmId }
if ($isEndpoint -or $isOwned) { $process.StartInfo.EnvironmentVariables["RAMSHARED_NATIVE_EXPECTED_PORT"] = [string]$ExpectedPort }
if ($isOwned) { $process.StartInfo.EnvironmentVariables["RAMSHARED_NATIVE_GUEST_READY"] = Join-Path $ArtifactDirectory "guest-ready" }
$process.StartInfo.RedirectStandardOutput = $true
$process.StartInfo.RedirectStandardError = $true
$stdoutFile = [IO.File]::Open($stdout, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
$stderrFile = [IO.File]::Open($stderr, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
if (-not $process.Start()) { throw "Native test process did not start" }
$stdoutCopy = $process.StandardOutput.BaseStream.CopyToAsync($stdoutFile)
$stderrCopy = $process.StandardError.BaseStream.CopyToAsync($stderrFile)
# Retain the actual native process object. An observation timeout does not kill
# the launcher, abandon the test's child handle, or authorize a replacement.
$state = [ordered]@{
    test_pid = $process.Id
    creation_filetime_utc = $process.StartTime.ToUniversalTime().ToFileTimeUtc()
    test_sha256 = $ExpectedTestSha256
    product_sha256 = $ExpectedProductSha256
    state = "running"
}
$state | ConvertTo-Json | Set-Content -LiteralPath $receipt
if (-not $process.WaitForExit(15000)) {
    $state.state = "observation_timeout_retaining_same_process"
    $state | ConvertTo-Json | Set-Content -LiteralPath $receipt
    Write-Warning "Native identity test is still owned; waiting for actual exit"
    while (-not $process.WaitForExit(1000)) { }
}
$process.WaitForExit()
[void]($stdoutCopy.GetAwaiter().GetResult())
[void]($stderrCopy.GetAwaiter().GetResult())
$stdoutFile.Dispose()
$stderrFile.Dispose()
$state.state = "exited"
$state.exit_code = $process.ExitCode
$state | ConvertTo-Json | Set-Content -LiteralPath $receipt
$process.Dispose()
if ($state.exit_code -ne 0) { throw "Native identity test failed: exit=$($state.exit_code)" }
if ((Get-Item -LiteralPath $stdout).Length -gt 262144 -or
    (Get-Item -LiteralPath $stderr).Length -gt 262144) {
    throw "Native test evidence exceeds its accepted output bound"
}
$text = Get-Content -LiteralPath $stdout -Raw
$detail = Get-Content -LiteralPath $stderr -Raw
if ($text -notmatch 'test result: ok\. 1 passed; 0 failed;') {
    throw "Native test did not execute exactly one successful case"
}
if ($isSamplerFailure) {
    if ($detail -notmatch 'NATIVE_SAMPLER_PANIC_CLEANUP_OK thread_joined=true child_custody_empty=true failure_preserved=true') { throw "Native sampler actor failure or child custody was not settled" }
} elseif ($isWatchdog) {
    if ($detail -notmatch 'NATIVE_WATCHDOG_JOIN_OK native_probe_observed=true kernel_joined=true' -or $detail -notmatch 'NATIVE_SCM_PROBE_OUTCOME broker_running=(true|false)') { throw "Native SCM probe outcome and joined watchdog were not observed" }
} elseif ($isOwned) {
    if ($detail -notmatch "NATIVE_CONTROL_STOP_OK joined=true samplers_stopped=true duplicate_refused=true") { throw "Native control-plane exit and sampler custody were not proved" }
} elseif ($isVmRefusal) {
    if ($detail -notmatch 'NATIVE_VM_REFUSAL_OK access_denied=true authority=false stopped=true') { throw "Native VM permission refusal was not proved" }
} elseif ($isEndpoint) {
    if ($detail -notmatch 'NATIVE_KERNEL_PEER_OK observed_vm=true observed_service=true roundtrip=true') { throw "Native kernel endpoint and connected roundtrip were not observed" }
} elseif ($isVm) {
    if ($detail -notmatch 'NATIVE_VM_PROBE_OK systems=[1-9][0-9]* process_verified=true exited=true job_empty=true' -or
        $detail -notmatch 'NATIVE_VM_SAMPLER_OK distinct_children=2 duplicate_start_refused=true stopped=true') {
        throw "Native VM process, current runtime and sampler controls were not completed"
    }
} else {
    if ($detail -notmatch '(?m)^NATIVE_IDENTITY_REPORT (.+)$') { throw "Native GPU report missing" }
    $report = $Matches[1] | ConvertFrom-Json
    if ($report.schema_version -ne 1 -or @($report.adapters).Count -eq 0) { throw "Native test did not return an adapter observation" }
}
Assert-Hash $TestExecutable $ExpectedTestSha256
Assert-Hash $product $ExpectedProductSha256
if ($isSamplerFailure) {
    Pass "native_sampler_actor_failure_custody"
} elseif ($isWatchdog) {
    Pass "native_watchdog_thread_join"
} elseif ($isOwned) {
    Pass "native_owned_control_shutdown"
} elseif ($isVmRefusal) {
    Pass "native_vm_permission_refusal"
} elseif ($isEndpoint) {
    Pass "native_kernel_endpoint_scope"
} elseif ($isVm) {
    Pass "native_vm_identity_supervision"
} elseif ($TestCase -eq "public_supervised_identity_matches_native_child_and_repeats") {
    if ($detail -notmatch 'NATIVE_SUPERVISOR_CHILD_OK' -or $detail -notmatch 'NATIVE_SAMPLER_OK distinct_children=2 duplicate_start_refused=true stopped=true' -or $detail -notmatch 'NATIVE_SAMPLER_AGE_OK original_parent_clock=true stopped=true') {
        throw "Native sampler custody controls were not completed"
    }
    Pass "native_gpu_identity_supervision"
} else {
    Pass "native_gpu_identity_command"
}
