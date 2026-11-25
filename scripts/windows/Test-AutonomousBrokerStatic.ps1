#Requires -Version 5.1
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/windows/Test-AutonomousBrokerStatic.ps1
[CmdletBinding()]
param(
    [string]$RepoRoot = ""
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrEmpty($RepoRoot)) {
    $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
}
$results = [Collections.Generic.List[object]]::new()
function Assert-Static([bool]$Condition, [string]$Name, [string]$Detail) {
    if (-not $Condition) { throw "$Name failed: $Detail" }
    $results.Add([pscustomobject]@{ test = $Name; verdict = "PASS"; detail = $Detail })
}

$spec = Get-Content (Join-Path $RepoRoot `
        "docs\specs\no-milestone\windows-autonomous-broker-service\SPEC.md") -Raw
$broker = Get-Content (Join-Path $RepoRoot "crates\ramshared-winbroker\src\pipe.rs") -Raw
$brokerMain = Get-Content (Join-Path $RepoRoot "crates\ramshared-winbroker\src\main.rs") -Raw
$brokerService = Get-Content (Join-Path $RepoRoot "crates\ramshared-winbroker\src\service.rs") -Raw
$consumer = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\main.rs") -Raw
# The installer mutex moved to the installation module during the
# transition extraction; the static contract follows the owner.
$consumerTransition = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\installation\transition.rs") -Raw
$serviceSidProbe = Get-Content (Join-Path $RepoRoot `
        "crates\ramshared-winsvc\src\bin\ramshared-service-sid-probe.rs") -Raw
$cudaProbe = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\cuda_probe.rs") -Raw
$online = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\product_online.rs") -Raw
$installer = Get-Content (Join-Path $RepoRoot "scripts\windows\Install-RamSharedService.ps1") -Raw
$build = Get-Content (Join-Path $RepoRoot "scripts\windows\build-winsvc.bat") -Raw
$guestLifecycle = Get-Content (Join-Path $RepoRoot `
        "scripts\windows\Run-GuestAutonomousLifecycle.ps1") -Raw
$hostLifecycle = Get-Content (Join-Path $RepoRoot `
        "scripts\windows\Run-HostAutonomousLifecycle.ps1") -Raw
$loaderWin = Get-Content (Join-Path $RepoRoot `
        "crates\ramshared-cuda\src\loader_win.rs") -Raw
# Test fixtures must not satisfy assertions about the production adapter.
$loaderWin = ($loaderWin -split '(?m)^#\[cfg\(test\)\]', 2)[0]

Assert-Static ($broker -match [regex]::Escape("\\.\pipe\RamSharedBroker.v1")) `
    "canonical_product_pipe" "fixed named-pipe endpoint is compiled into the broker"
Assert-Static ($broker -notmatch "TcpListener|TcpStream") `
    "no_broker_tcp_surface" "native broker pipe module has no TCP transport"
Assert-Static ($brokerMain -match "fn parse_cli" -and
    $brokerMain -match [regex]::Escape('console --config <absolute>') -and
    $brokerMain -match "cli_has_no_tcp_listen_option" -and
    $brokerMain -notmatch "TcpListener") `
    "broker_cli_contract" "broker entry accepts only the sealed service/console configuration surface"
Assert-Static ($brokerService -match 'SERVICE_NAME: &str = "RamSharedBroker"' -and
    $brokerService -match "report_deterministic_start_failure" -and
    $brokerService -match "verify_active_config") `
    "broker_service_contract" "SCM entry binds deterministic failure and active-manifest verification"
Assert-Static ($serviceSidProbe -match "deny-only" -and
    $serviceSidProbe -match "S-1-5-80-" -and
    $serviceSidProbe -match "service_dispatcher::start") `
    "service_sid_probe_contract" "service-SID probe retains deny-only and SCM boundaries"
Assert-Static ($cudaProbe -match "broker_pipe" -and
    $cudaProbe -match "BrokerPipeV1::NamedPipeV1") `
    "cuda_probe_uses_local_broker_config" "CUDA probe fixture uses the native local broker selector"
Assert-Static ($consumerTransition -match "Global\\RamSharedProductInstall.v1") `
    "protected_installer_mutex_named" "product controller uses the SPEC mutex"
Assert-Static ($consumer -match "ReplaceFileW" -and $consumer -match "REPLACEFILE_WRITE_THROUGH") `
    "active_pointer_replacefile" "active pointer uses write-through ReplaceFileW"
Assert-Static ($consumer -match "RamSharedBroker" -and $consumer -match "ServiceDependency") `
    "two_service_dependency" "consumer SCM definition names the broker dependency"
Assert-Static ($online -notmatch "TcpStream") `
    "consumer_no_daily_tcp" "product Online path has no TCP client"
Assert-Static ($installer -notmatch "New-Service|sc.exe create|CreateService") `
    "single_installer_authority" "PowerShell wrapper does not implement a second SCM transaction"
Assert-Static ($spec -match "Run-GuestProductPackage.ps1" -and
    $spec -match "Run-HostAutonomousLifecycle.ps1") `
    "spec_harness_matrix" "SPEC names package and physical lifecycle harnesses"
Assert-Static ($build -match "ramshared-winbroker.exe" -and
    $build -match "ramshared-winsvc.exe") `
    "BROKER_BINARY_MATCH" "native build stages both independently hashed binaries"
Assert-Static ($consumer -match "ServiceDependency" -and
    $consumer -match "BROKER_SERVICE_NAME") `
    "SCM_DEPENDENCY_MATCH" "consumer definition contains the broker SCM dependency"
Assert-Static ($consumer -match "ServiceSidType::Unrestricted") `
    "SERVICE_SID_MATCH" "controller applies unrestricted service SID to both definitions"
Assert-Static ($broker -notmatch "TcpListener" -and $online -notmatch "TcpStream") `
    "DAILY_TCP_LISTENER_ABSENT" "daily broker/consumer surface is named-pipe only"
Assert-Static ($guestLifecycle -notmatch "Lab-LeaseBroker|TcpListener" -and
    $hostLifecycle -notmatch "Lab-LeaseBroker|TcpListener") `
    "NO_LAB_BROKER_REFERENCE" "autonomous guest/host campaigns consume packaged services"

# RF-4 / DT-5 Windows loader twin of `loader_unix.rs`: Win32 triad only, every
# symbol/close entry point refuses a null handle. The driver path is absolute,
# system-owned in the ordinary OS installation, and NUL-terminated UTF-16.
# These source checks supplement the actual native loader regressions.
Assert-Static ($loaderWin -match "LoadLibraryExW" -and
    $loaderWin -match "GetProcAddress" -and
    $loaderWin -match "FreeLibrary" -and
    $loaderWin -notmatch "\bdlopen\s*\(|\bdlsym\s*\(|\bdlclose\s*\(") `
    "loader_win_uses_only_the_win32_loader_triad" `
    "loader_win binds LoadLibraryExW/GetProcAddress/FreeLibrary and no POSIX dlopen call"
Assert-Static ($loaderWin -match "fn sym\([\s\S]{0,400}handle\.is_null\(\)" -and
    $loaderWin -match "fn close\([\s\S]{0,400}handle\.is_null\(\)") `
    "loader_win_null_handle_is_refused_before_any_api_call" `
    "sym/close short-circuit a null handle before calling the native API"
Assert-Static ($loaderWin -match 'GetSystemDirectoryW\(wide.as_mut_ptr\(\)' -and
    $loaderWin -match 'wide.extend\(string_str.encode_utf16\(\)\)' -and
    $loaderWin -match 'wide.push\(0\)' -and
    $loaderWin -match "CStr::from_ptr") `
    "loader_win_module_path_is_nul_terminated_utf16" `
    "open builds the absolute native system-directory path with a terminating NUL"
Assert-Static ($loaderWin -match '(?s)LoadLibraryExW\(.*?LOAD_LIBRARY_SEARCH_SYSTEM32' -and
    $loaderWin -match 'ERROR_INVALID_NAME' -and
    $loaderWin -match 'ERROR_NO_UNICODE_TRANSLATION' -and
    $loaderWin -notmatch '\bLoadLibraryW\s*\(') `
    "loader_win_refuses_ambient_driver_search" `
    "driver and dependencies use System32; invalid caller names never enable ordinary search"
$loaderError = [regex]::Match($loaderWin, '(?s)pub fn error\(\).*?\n\}').Value
Assert-Static ($loaderError -match "GetLastError" -and
    $loaderError -match "Windows error code: 0x" -and
    $loaderError -notmatch "\{:p\}|as usize|as u64") `
    "loader_win_error_reports_code_only" `
    "error() formats the Win32 code and leaks no pointer or module path"
Assert-Static ($loaderWin -match "fn close\([\s\S]{0,600}FreeLibrary" -and
    $loaderWin -match "matching dlclose behavior") `
    "loader_win_close_matches_dlclose_status" `
    "close maps FreeLibrary BOOL to the dlclose-style 0 success / -1 failure"
# Roll-up reached only when every loader_win assertion above has passed.
Write-Output "PASS loader_win_adapter_contract"

$gpuNative = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\gpu_identity\native.rs") -Raw
$gpuNative = ($gpuNative -split '(?m)^#\[cfg\(test\)\]', 2)[0]
Assert-Static ($gpuNative -match 'D3DKMTOpenAdapterFromLuid' -and
    $gpuNative -match 'D3DKMTQueryAdapterInfo' -and $gpuNative -match 'D3DKMTCloseAdapter' -and
    $gpuNative -match 'INTERFACE_QUERY: u32 = 79' -and
    $gpuNative -match 'INTERFACE_BYTES == 520' -and
    $gpuNative -match 'size_of::<QueryAdapter>\(\) == 24' -and
    $gpuNative -match 'GetProcessTimes' -and $gpuNative -notmatch '\.create_context\(|\.alloc\(') `
    "gpu_identity_native_adapter_contract" `
    "native identity queries preserve SDK layout, process observation and context-free effects"

$gpuProcess = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\gpu_identity\process.rs") -Raw
$gpuSpawn = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\gpu_identity\process\spawn.rs") -Raw
$gpuPipe = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\gpu_identity\process\pipe.rs") -Raw
$gpuSampler = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\gpu_identity\sampler.rs") -Raw
$nativeSampler = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\native_sampler.rs") -Raw
Assert-Static ($gpuSpawn -match 'PROC_THREAD_ATTRIBUTE_JOB_LIST' -and
    $gpuSpawn -match 'PROC_THREAD_ATTRIBUTE_HANDLE_LIST' -and
    $gpuSpawn -match 'JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE' -and
    $gpuSpawn -match 'CreateProcessW' -and
    $gpuProcess -match 'GetProcessTimes' -and $gpuProcess -match 'WaitForSingleObject' -and
    $gpuProcess -match 'QueryInformationJobObject' -and $gpuProcess -match 'TerminateJobObject' -and
    $gpuPipe -match 'FILE_FLAG_OVERLAPPED' -and $gpuPipe -match 'GetOverlappedResult' -and
    $gpuPipe -match 'CancelIoEx' -and $gpuPipe -notmatch 'PeekNamedPipe|CreatePipe\(' -and
    $gpuSampler -match 'static OWNER:' -and $gpuSampler -match 'NativeSampler<HostGpuIdentityReport>' -and
    $nativeSampler -match 'try_lock\(' -and $nativeSampler -match 'stop_and_wait' -and
    $nativeSampler -match 'NativeProcess::spawn_identity' -and $nativeSampler -match 'NativeProcess::spawn_vm_identity') `
    "native_gpu_process_custody_contract" `
    "native sampler owns process/job and asynchronous capture; executable controls remain mandatory"

$vmNative = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\vm_identity\native.rs") -Raw
$vmApi = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\vm_identity\api.rs") -Raw
$vmSampler = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\vm_identity\sampler.rs") -Raw
Assert-Static ($vmApi -match 'GetSystemDirectoryW' -and $vmApi -match 'LOAD_LIBRARY_SEARCH_SYSTEM32' -and
    $vmApi -match 'HcsEnumerateComputeSystems' -and $vmApi -match 'HcsWaitForOperationResult' -and
    $vmNative -match 'LocalFree' -and $vmNative -match 'MAX_VM_REPORT_BYTES' -and
    $vmSampler -match 'static OWNER: NativeSampler<VmIdentityReport>' -and
    $nativeSampler -match 'last_failure.try_lock' -and $nativeSampler -match 'ProbeKind::Vm') `
    "native_vm_query_contract" "VM enumeration uses optional native system exports, bounded child custody and nonblocking diagnosis"

$hyperv = Get-Content (Join-Path $RepoRoot "crates\ramshared-ipc\src\vsock\windows_hyperv.rs") -Raw
$hypervStream = Get-Content (Join-Path $RepoRoot "crates\ramshared-ipc\src\vsock\windows_hyperv\stream.rs") -Raw
Assert-Static ($hyperv -match 'getpeername' -and $hyperv -match 'size_of::<SOCKADDR_HV>\(\) == 36' -and
    $hyperv -match 'expected.is_some_and' -and $hyperv -match 'peer != expected' -and
    $hyperv -match 'canonical_guid\(address.VmId\)' -and
    $hypervStream -match 'Ok\(Some\(self\.peer\)\)') `
    "native_kernel_endpoint_contract" "Windows socket scope comes from the kernel and matches the requested canonical VM/service"

$taskPermit = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\vsock_lease\lifecycle.rs") -Raw
$taskOwner = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\thread_custody.rs") -Raw
$taskNative = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\thread_custody\native.rs") -Raw
$nativeLoop = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\vsock_lease\listener\native.rs") -Raw
$labHost = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\bin\ramshared-vsock-host.rs") -Raw
Assert-Static ($taskOwner -match 'JoinHandle' -and $taskOwner -match 'worker\s*\.join\(' -and
    $taskOwner -match 'ThreadState::Pending' -and $taskPermit -match 'ConfiguredPermit' -and
    $taskNative -match 'WaitForSingleObject\(worker.as_raw_handle\(\), 0\)' -and
    $nativeLoop -match 'MetadataCustody' -and $nativeLoop -match 'serve_connection_with_stop' -and
    $labHost -match 'wait_for_stop' -and $labHost -notmatch 'from_millis\(1_500\)') `
    "native_control_plane_custody_contract" "control-plane custody observes the kernel thread, retains Pending and settles metadata before joined stop"

$watchdogOwner = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\watchdog\wsl_scm_watchdog.rs") -Raw
$watchdogNative = Get-Content (Join-Path $RepoRoot "crates\ramshared-winsvc\src\watchdog\wsl_scm_watchdog\native.rs") -Raw
Assert-Static ($nativeSampler -match 'Mutex<Option<OwnedThread>>' -and $nativeSampler -match 'task.has_joined' -and
    $nativeSampler -match 'settle_processes' -and $nativeSampler -match 'catch_unwind' -and
    $watchdogOwner -match 'io::Result<OwnedThread>' -and $watchdogOwner -match 'stop.wait' -and
    $watchdogNative -match 'ServiceAccess::QUERY_STATUS' -and $watchdogNative -match 'query_status' -and
    $consumer -match '(?s)spawn_watchdog\(\s*BrokerServiceProbe,\s*[^,]+,\s*\)\?' -and
    $consumer -match '(?s)let result =\s*run_product_online.*?scm_watchdog.request_stop\(\).*?scm_watchdog.wait_for_stop.*?current_state: ServiceState::Stopped') `
    "native_sampler_watchdog_join_contract" "sampler and watchdog retain shared actual-thread custody; native status probes are read-only"

$results
