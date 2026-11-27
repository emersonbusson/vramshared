// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vsock_lease/listener/native.rs
//! Native connection loop. Metadata custody settles before the owned thread exits.
use super::super::lifecycle::ConfiguredPermit;
use super::super::{HostSamples, StopSignal, VsockMode, serve_connection_with_stop};
use super::{ControlPlaneTask, LeasePolicy, LeaseServerError, VsockConfig};
use crate::vm_id_resolve::{parse_guid, resolve_connect_candidates, verify_connected_route_peer};
use crate::vm_identity::VmOsClassification;
use ramshared_ipc::vsock::{
    VsockError, VsockListener, VsockStreamTrait, connect_hyperv, linux_service_guid, listen_hyperv,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SINGLE_ROUTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MULTI_ROUTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

enum Endpoint {
    Listen(VsockListener),
    Connect {
        explicit: Option<[u8; 16]>,
        port: u32,
    },
}
impl Endpoint {
    fn connect_route(
        vm_id: [u8; 16],
        port: u32,
        timeout: Duration,
    ) -> Result<Box<dyn VsockStreamTrait>, LeaseServerError> {
        let stream = connect_hyperv(vm_id, linux_service_guid(port), timeout)
            .map_err(LeaseServerError::Vsock)?;
        let observed = stream.native_peer().map_err(LeaseServerError::Io)?;
        verify_connected_route_peer(vm_id, port, observed)?;
        Ok(stream)
    }

    fn resolve_targets(&self) -> Result<Vec<([u8; 16], &'static str, Duration)>, LeaseServerError> {
        match self {
            Self::Listen(_) => Ok(Vec::new()),
            Self::Connect {
                explicit: Some(id), ..
            } => Ok(vec![(*id, "explicit", SINGLE_ROUTE_CONNECT_TIMEOUT)]),
            Self::Connect { explicit: None, .. } => {
                let candidates = resolve_connect_candidates()?;
                if candidates.len() == 1 {
                    let candidate = candidates[0];
                    let kind = match candidate.os_classification {
                        VmOsClassification::Linux => "native_linux",
                        VmOsClassification::Omitted => "endpoint_verified",
                    };
                    return Ok(vec![(
                        candidate.runtime_id,
                        kind,
                        SINGLE_ROUTE_CONNECT_TIMEOUT,
                    )]);
                }
                Ok(candidates
                    .into_iter()
                    .map(|c| {
                        (
                            c.runtime_id,
                            "multi_vm_authenticated",
                            MULTI_ROUTE_CONNECT_TIMEOUT,
                        )
                    })
                    .collect())
            }
        }
    }

    fn retry_delay(&self) -> Duration {
        match self {
            Self::Listen(_) => Duration::from_millis(250),
            Self::Connect { .. } => Duration::from_secs(2),
        }
    }
}
struct MetadataCustody {
    finished: bool,
}
impl MetadataCustody {
    fn finish(&mut self) -> std::io::Result<()> {
        let gpu = crate::gpu_identity::sampler::stop_and_wait();
        let activity = crate::gpu_activity::sampler::stop_and_wait();
        let vm = crate::vm_identity::sampler::stop_and_wait();
        let storage = crate::host_storage::sampler::stop_and_wait();
        self.finished = true;
        gpu.and(activity)
            .and(vm)
            .and(storage)
            .map_err(std::io::Error::other)
    }
}
impl Drop for MetadataCustody {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self.finish()
        {
            eprintln!("native metadata cleanup failed: {error}");
        }
    }
}
fn start_metadata(product: PathBuf, log: &Path) -> MetadataCustody {
    let guard = MetadataCustody { finished: false };
    crate::host_storage::sampler::configure_product_executable(product.clone());
    if let Err(error) = crate::gpu_identity::sampler::start(product.clone()) {
        append_log(log, &format!("native GPU sampler unavailable: {error}"));
    }
    if let Err(error) = crate::gpu_activity::sampler::start(product.clone()) {
        append_log(
            log,
            &format!("native GPU activity sampler unavailable: {error}"),
        );
    }
    if let Err(error) = crate::vm_identity::sampler::start(product) {
        append_log(log, &format!("native VM sampler unavailable: {error}"));
    }
    guard
}
const INITIAL_GPU_OBSERVATION_SETTLE: Duration = Duration::from_millis(250);
const INITIAL_GPU_OBSERVATION_POLL: Duration = Duration::from_millis(10);

fn await_initial_gpu_observation(stop: &StopSignal) {
    let deadline = std::time::Instant::now() + INITIAL_GPU_OBSERVATION_SETTLE;
    while !stop.is_requested()
        && crate::gpu_identity::sampler::observation().is_none()
        && std::time::Instant::now() < deadline
    {
        stop.wait(INITIAL_GPU_OBSERVATION_POLL);
    }
}

fn serve_active_session(
    stream: &mut Box<dyn VsockStreamTrait>,
    route_kind: &str,
    key: &[u8],
    policy: LeasePolicy,
    log: &Path,
    stop: &StopSignal,
) -> bool {
    // Pause VM discovery while a lease session is active and run GPU telemetry at
    // full cadence; restore idle posture when the session closes.
    crate::vm_identity::sampler::set_paused(true);
    crate::gpu_identity::sampler::set_idle(false);
    crate::gpu_activity::sampler::set_idle(false);
    append_log(log, &format!("session connected (route={route_kind})"));
    append_log(log, &crate::gpu_identity::sampler::diagnostic());
    let established = match serve_connection_with_stop(
        stream,
        key,
        policy,
        unix_time_ms,
        host_samples,
        Some(stop),
    ) {
        Ok(beats) => {
            append_log(log, &format!("session closed after {beats} beats"));
            true
        }
        Err(error) => {
            append_log(log, &format!("session ended: {error}"));
            !matches!(
                error,
                LeaseServerError::Refused(_)
                    | LeaseServerError::Handshake(_)
                    | LeaseServerError::Protocol(_)
                    | LeaseServerError::Frame(_)
            )
        }
    };
    crate::gpu_identity::sampler::set_idle(true);
    crate::gpu_activity::sampler::set_idle(true);
    crate::vm_identity::sampler::set_paused(false);
    established
}

fn log_refusal_dedup(log: &Path, last_refusal: &mut Option<String>, error: &LeaseServerError) {
    let text = error.to_string();
    if last_refusal.as_deref() != Some(text.as_str()) {
        append_log(log, &format!("connection attempt refused: {text}"));
        *last_refusal = Some(text);
    }
}

fn run(
    endpoint: Endpoint,
    key: Vec<u8>,
    policy: LeasePolicy,
    log: PathBuf,
    stop: Arc<StopSignal>,
    product: PathBuf,
) -> std::io::Result<()> {
    let mut metadata = start_metadata(product, &log);
    await_initial_gpu_observation(&stop);
    crate::gpu_identity::sampler::set_idle(true);
    append_log(&log, "control plane started");
    let mut last_refusal: Option<String> = None;
    while !stop.is_requested() {
        match &endpoint {
            Endpoint::Listen(listener) => match listener.accept(Duration::from_secs(1)) {
                Ok(mut stream) => {
                    last_refusal = None;
                    let _ = serve_active_session(&mut stream, "listen", &key, policy, &log, &stop);
                    continue;
                }
                Err(VsockError::AcceptTimeout) => continue,
                Err(error) => {
                    log_refusal_dedup(&log, &mut last_refusal, &LeaseServerError::Vsock(error));
                }
            },
            Endpoint::Connect { port, .. } => match endpoint.resolve_targets() {
                Ok(targets) => {
                    let mut last_error: Option<LeaseServerError> = None;
                    let mut authenticated = false;
                    for (vm_id, route_kind, timeout) in targets {
                        if stop.is_requested() {
                            break;
                        }
                        match Endpoint::connect_route(vm_id, *port, timeout) {
                            Ok(mut stream) => {
                                if serve_active_session(
                                    &mut stream,
                                    route_kind,
                                    &key,
                                    policy,
                                    &log,
                                    &stop,
                                ) {
                                    last_refusal = None;
                                    authenticated = true;
                                    break;
                                }
                            }
                            Err(error) => last_error = Some(error),
                        }
                    }
                    if !authenticated
                        && !stop.is_requested()
                        && let Some(error) = last_error
                    {
                        log_refusal_dedup(&log, &mut last_refusal, &error);
                    }
                }
                Err(error) => log_refusal_dedup(&log, &mut last_refusal, &error),
            },
        }
        stop.wait(endpoint.retry_delay());
    }
    append_log(&log, "control plane session loop ended");
    metadata.finish()
}
pub(super) fn spawn(
    config: VsockConfig,
    key: Vec<u8>,
    policy: LeasePolicy,
    log: PathBuf,
    product: PathBuf,
) -> Result<ControlPlaneTask, LeaseServerError> {
    let permit = ConfiguredPermit::acquire().map_err(LeaseServerError::Io)?;
    let endpoint = match config.mode {
        VsockMode::Listen => Endpoint::Listen(
            listen_hyperv(linux_service_guid(config.port)).map_err(LeaseServerError::Vsock)?,
        ),
        VsockMode::Connect => Endpoint::Connect {
            explicit: match config.vm_id.as_deref() {
                None | Some("auto") => None,
                Some(id) => Some(parse_guid(id)?),
            },
            port: config.port,
        },
    };
    let mut task = ControlPlaneTask::spawn("ramshared-native-control", move |stop| {
        run(endpoint, key, policy, log, stop, product)
    })
    .map_err(LeaseServerError::Io)?;
    task.permit = Some(permit);
    Ok(task)
}
fn host_samples() -> HostSamples {
    HostSamples {
        memory: crate::host_memory::sample(),
        gpu: crate::gpu_identity::sampler::aged_observation(),
        activity: crate::gpu_activity::sampler::observation(),
        storage: Some(std::sync::Arc::new(|disk_id| {
            crate::host_storage::sampler::sample_for_disk(
                disk_id,
                std::time::Duration::from_millis(250),
            )
            .map(|(report, captured_since)| super::super::StorageSample {
                report,
                age_ms: 0,
                captured_since,
                complete: true,
            })
        })),
    }
}
fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}
fn append_log(path: &Path, message: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{} {message}", unix_time_ms());
    }
}
