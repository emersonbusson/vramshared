// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/product_online/teardown.rs
//! Gate-A / VolumeLock / Gate-B / teardown helpers for `product_online`.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ramshared_block::VramBackend;

use crate::broker_tenant::BrokerTenant;
use crate::config::WinDriveConfig;
use crate::driver_link::{DriverLink, QueueAccess};
use crate::evidence::{EvidenceWriter, IoCounters, RuntimeEvidence, summarize_latencies, utc_ms};
use crate::host_safety::{LockWaitDecision, lock_wait_decision};
use crate::ipc::NamedPipeBrokerStream as BrokStream;
use crate::runtime::{
    PostTeardownReleaseDisposition, RunMode, RuntimeError, RuntimeErrorClass, RuntimePhase,
    RuntimeState, RuntimeSummary, post_teardown_release_disposition,
};
use crate::service::{
    DiskControl, PagefileGates, ServiceState, WipeVram, pagefile_refusal_to_runtime,
};
use crate::windows_driver::{WindowsDriverLink, WindowsMappedQueue};
use crate::windows_host::WindowsHostState;

use super::gates::{BackendWipe, HostGates, LinkDisk};

pub(super) struct StopContext<'a> {
    pub(super) state: &'a mut RuntimeState,
    pub(super) row: &'a mut RuntimeEvidence,
    pub(super) evidence: &'a mut EvidenceWriter,
    pub(super) stop: &'a AtomicBool,
    pub(super) resume_notifier: Option<&'a AtomicBool>,
}

pub(super) struct OnlineIo<'a, M> {
    pub(super) link: &'a mut WindowsDriverLink,
    pub(super) dlink: &'a mut DriverLink<WindowsMappedQueue>,
    pub(super) backend: &'a mut VramBackend<M>,
}

pub(super) struct BrokerSession<'a> {
    pub(super) tenant: &'a mut BrokerTenant,
    pub(super) stream: &'a mut BrokStream,
}

pub(super) fn prepare_teardown_gates<M: ramshared_vram::VramMemory>(
    cfg: &WinDriveConfig,
    teardown_letter: char,
    serial_str: &str,
    io: &mut OnlineIo<'_, M>,
    stop_ctx: &mut StopContext<'_>,
) -> Option<HostGates> {
    let mut gates = match HostGates::new(
        teardown_letter,
        cfg.volume_mount_path.clone(),
        serial_str,
        cfg.size_bytes,
    ) {
        Ok(g) => g,
        Err(e) => {
            teardown_diag(&format!("HostGates::new identity error: {e}"));
            stop_ctx.state.healthy = false;
            stop_ctx.state.phase = RuntimePhase::FailedSafe;
            sync_runtime_evidence(stop_ctx.row, stop_ctx.state);
            let _ = stop_ctx.evidence.append(stop_ctx.row);
            preserve_failed_safe("teardown identity construction failed");
        }
    };

    // Re-observe the exact letter-to-disk identity while the I/O pump stays
    // available. CREATE-time values are expectations, never OS identity.
    let identity_serial = serial_str.to_owned();
    let identity_size = cfg.size_bytes;
    let identity_mount_path = cfg.volume_mount_path.clone();
    let (observed, observed_disk_number, observed_volume_path) =
        match readonly_host_call_with_io_pump(
            io.link,
            io.dlink,
            io.backend,
            Duration::from_secs(15),
            move || {
                WindowsHostState::observe_product_volume(
                    teardown_letter,
                    identity_mount_path.as_deref(),
                    &identity_serial,
                    identity_size,
                )
            },
        ) {
            Ok(observed) => observed,
            Err(e) => {
                refuse_stop_online(stop_ctx, &format!("volume_identity_observation: {e}"));
                return None;
            }
        };
    if let Err(e) =
        gates.accept_observed_identity(observed, observed_disk_number, observed_volume_path)
    {
        refuse_stop_online(stop_ctx, &format!("volume_identity: {e}"));
        return None;
    }

    // Identity + Gate A (read-only, safe to refuse back to Online).
    if let Err(e) = gates.verify_volume_identity(teardown_letter) {
        refuse_stop_online(stop_ctx, &format!("volume_identity: {e}"));
        return None;
    }
    let gate_a = readonly_host_call_with_io_pump(
        io.link,
        io.dlink,
        io.backend,
        Duration::from_secs(5),
        WindowsHostState::active_pagefiles,
    )
    .and_then(|rows| gates.filter_pagefiles(rows));
    match gate_a {
        Ok(pf) if pf.is_empty() => {
            teardown_diag("teardown phase=GateA pagefiles_on_volume=0");
            Some(gates)
        }
        Ok(pf) => {
            refuse_stop_online(stop_ctx, &format!("gate_a_active: {}", pf.join(",")));
            None
        }
        Err(e) => {
            refuse_stop_online(stop_ctx, &format!("gate_a_query: {e}"));
            None
        }
    }
}

pub(super) fn lock_mounted_volume_with_io<M: ramshared_vram::VramMemory>(
    teardown_letter: char,
    gates: &mut HostGates,
    teardown_started: Instant,
    stop_ctx: &mut StopContext<'_>,
    io: &mut OnlineIo<'_, M>,
    broker: &mut BrokerSession<'_>,
) -> bool {
    let disk_for_lock = gates.expected_disk_number();
    let volume_path_for_lock = gates.volume_device_path().map(str::to_owned);
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let result = if let Some(path) = volume_path_for_lock {
            WindowsHostState::lock_product_volume_path(&path, teardown_letter, Some(disk_for_lock))
        } else {
            WindowsHostState::lock_product_volume(teardown_letter, Some(disk_for_lock))
        };
        let _ = tx.send(result);
    });
    let lock_res = loop {
        match rx.try_recv() {
            Ok(r) => break r,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                break Err(crate::windows_host::HostError::Volume(
                    "lock worker disconnected".into(),
                ));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        if lock_wait_decision(teardown_started.elapsed(), Duration::from_secs(30), false)
            == LockWaitDecision::EnterFailedSafe
        {
            stop_ctx.state.healthy = false;
            stop_ctx.state.phase = RuntimePhase::FailedSafe;
            stop_ctx.row.error_class = Some("teardown_timeout".into());
            stop_ctx.row.error_code = Some("30s".into());
            sync_runtime_evidence(stop_ctx.row, stop_ctx.state);
            let _ = stop_ctx.evidence.append(stop_ctx.row);
            teardown_diag("FailedSafe: volume lock exceeded 30,000 ms; retaining I/O pump");
            preserve_failed_safe_with_io("volume lock worker remains in flight", io, broker, rx);
        }
        match io.link.commit_and_fetch(Duration::from_millis(50)) {
            Ok(()) => {
                let _ = io.dlink.commit_and_fetch(io.backend);
            }
            Err(crate::windows_driver::IoctlError::Timeout) => {}
            Err(e) => {
                teardown_diag(&format!("I/O pump during lock: {e}"));
            }
        }
    };
    if teardown_started.elapsed() >= Duration::from_secs(30) {
        drop(lock_res);
        refuse_stop_online(stop_ctx, "volume_lock_completed_after_30s_budget");
        return false;
    }
    match lock_res {
        Ok(vol) => {
            gates.take_locked(vol);
            teardown_diag("teardown phase=VolumeLock OK");
            true
        }
        Err(e) => {
            refuse_stop_online(stop_ctx, &format!("volume_lock: {e}"));
            false
        }
    }
}

pub(super) fn run_gate_b_and_teardown<M: ramshared_vram::VramMemory>(
    cfg: &WinDriveConfig,
    teardown_letter: char,
    teardown_started: Instant,
    lease_snapshot: (u32, u64),
    gates: &mut HostGates,
    stop_ctx: &mut StopContext<'_>,
    io: &mut OnlineIo<'_, M>,
) -> bool {
    // Gate B is observed while the exact volume lock is held and while any
    // outstanding miniport request can still be drained. Cache the snapshot
    // so the destructive helper cannot silently re-query a weaker source.
    let gate_b_timeout = Duration::from_secs(30)
        .saturating_sub(teardown_started.elapsed())
        .min(Duration::from_secs(5));
    if gate_b_timeout.is_zero() {
        let _ = gates.unlock_volume();
        refuse_stop_online(stop_ctx, "gate_b_budget_exhausted");
        return false;
    }
    let gate_b = readonly_host_call_with_io_pump(
        io.link,
        io.dlink,
        io.backend,
        gate_b_timeout,
        WindowsHostState::active_pagefiles,
    )
    .and_then(|rows| gates.filter_pagefiles(rows));
    match gate_b {
        Ok(rows) if rows.is_empty() => gates.cache_pagefiles(rows),
        Ok(rows) => {
            let _ = gates.unlock_volume();
            refuse_stop_online(stop_ctx, &format!("gate_b_active: {}", rows.join(",")));
            return false;
        }
        Err(e) => {
            let _ = gates.unlock_volume();
            refuse_stop_online(stop_ctx, &format!("gate_b_query: {e}"));
            return false;
        }
    }

    // Locked: stop serving, then Gate B → dismount → unregister → destroy → wipe.
    let _ = io.link.cancel_fetch();
    let mut svc = ServiceState {
        lease: Some(crate::broker_tenant::LeaseState {
            lease: lease_snapshot.0,
            bytes: lease_snapshot.1,
        }),
        disk_created: true,
        registered_queue: true,
        online: true,
    };
    let mut phases = Vec::new();
    let mut cfg_teardown = cfg.clone();
    cfg_teardown.volume_letter = teardown_letter;

    // Gate B + rest of destructive path (volume already locked).
    let teardown_result = {
        let mut disk_ctl = LinkDisk {
            link: io.link,
            unregistered: false,
            destroyed: false,
        };
        let mut wipe = BackendWipe {
            backend: io.backend,
        };
        teardown_after_lock(
            &cfg_teardown,
            &mut svc,
            &mut disk_ctl,
            &mut wipe,
            gates,
            &mut phases,
        )
    };
    match teardown_result {
        Ok(()) => true,
        Err(e) => {
            if pagefile_refusal_to_runtime(&e).is_some() {
                let reason = e.to_string();
                refuse_stop_online(stop_ctx, &format!("{reason}; phases={phases:?}"));
                return false;
            }
            teardown_diag(&format!("teardown non-code7 failure → FailedSafe: {e}"));
            stop_ctx.state.healthy = false;
            stop_ctx.state.phase = RuntimePhase::FailedSafe;
            sync_runtime_evidence(stop_ctx.row, stop_ctx.state);
            let _ = stop_ctx.evidence.append(stop_ctx.row);
            preserve_failed_safe("partial teardown failure");
        }
    }
}

pub(super) fn finalize_released_teardown<M: ramshared_vram::VramMemory>(
    backend: VramBackend<M>,
    ctx: &ramshared_cuda::Context<'_>,
    free_before: usize,
    broker_lost_online: bool,
    teardown_started: Instant,
    stop_ctx: &mut StopContext<'_>,
    broker: &mut BrokerSession<'_>,
) -> Result<RuntimeSummary, RuntimeError> {
    // The disk and queue no longer reference the backend. Release
    // DeviceMem and verify restoration before LeaseRelease (DT-8).
    let device_mem = backend.into_inner();
    drop(device_mem);
    let free_after = match ctx.mem_info() {
        Ok((free_after, _)) => free_after,
        Err(e) => {
            stop_ctx.state.healthy = false;
            stop_ctx.state.phase = RuntimePhase::FailedSafe;
            stop_ctx.state.allocated_bytes = 0;
            stop_ctx.row.error_class = Some("cuda_restore_query".into());
            stop_ctx.row.error_code = Some(e.to_string());
            sync_runtime_evidence(stop_ctx.row, stop_ctx.state);
            let _ = stop_ctx.evidence.append(stop_ctx.row);
            preserve_failed_safe_lease(
                "CUDA free restoration query failed after destroy",
                broker.tenant,
                broker.stream,
            );
        }
    };
    stop_ctx.row.free_bytes = free_after as u64;
    if free_after.saturating_add(64 * 1024 * 1024) < free_before {
        stop_ctx.state.healthy = false;
        stop_ctx.state.phase = RuntimePhase::FailedSafe;
        stop_ctx.state.allocated_bytes = 0;
        stop_ctx.row.error_class = Some("cuda_restore_miss".into());
        stop_ctx.row.error_code = Some(format!("before={free_before} after={free_after}"));
        sync_runtime_evidence(stop_ctx.row, stop_ctx.state);
        let _ = stop_ctx.evidence.append(stop_ctx.row);
        preserve_failed_safe_lease(
            "CUDA free bytes not restored within 64 MiB after destroy",
            broker.tenant,
            broker.stream,
        );
    }
    if let Err(e) = broker.tenant.release(broker.stream) {
        stop_ctx.state.healthy = false;
        stop_ctx.state.phase = RuntimePhase::FailedSafe;
        stop_ctx.state.allocated_bytes = 0;
        stop_ctx.row.error_class = Some("lease_release".into());
        stop_ctx.row.error_code = Some(e.to_string());
        sync_runtime_evidence(stop_ctx.row, stop_ctx.state);
        let _ = stop_ctx.evidence.append(stop_ctx.row);
        if post_teardown_release_disposition(broker_lost_online, false)
            == PostTeardownReleaseDisposition::StopWithBrokerFailure
        {
            teardown_diag(
                "FailedSafe: safe teardown completed after broker loss; \
                 lease release is unconfirmed",
            );
            return Err(RuntimeError::new(
                RuntimeErrorClass::Broker,
                3,
                format!(
                    "broker lost Online; safe teardown completed; \
                     lease release unconfirmed: {e}"
                ),
            ));
        }
        preserve_failed_safe_lease(
            "destructive teardown completed but lease release failed",
            broker.tenant,
            broker.stream,
        );
    }
    stop_ctx.state.phase = RuntimePhase::Stopped;
    stop_ctx.state.lease_id = None;
    stop_ctx.state.allocated_bytes = 0;
    stop_ctx.row.duration_ms = teardown_started.elapsed().as_millis() as u64;
    stop_ctx.row.begin_event("Stopped", utc_ms());
    sync_runtime_evidence(stop_ctx.row, stop_ctx.state);
    let _ = stop_ctx.evidence.append(stop_ctx.row);
    teardown_diag("Stopped: teardown_after_lock + lease release OK");
    Ok(RuntimeSummary {
        phase: stop_ctx.state.phase,
        lease_id: stop_ctx.state.lease_id,
        allocated_bytes: stop_ctx.state.allocated_bytes,
        idempotent_stop: false,
        exit_code: 0,
    })
}

/// Gate B → flush/dismount → unregister → destroy → unlock → wipe.
/// Caller must already hold exclusive volume lock on `gates`.
fn teardown_after_lock(
    cfg: &WinDriveConfig,
    state: &mut ServiceState,
    disk: &mut dyn DiskControl,
    wipe: &mut dyn WipeVram,
    gates: &mut dyn PagefileGates,
    phases: &mut Vec<crate::service::TeardownPhase>,
) -> Result<(), crate::service::ProvisionError> {
    use crate::service::{ProvisionError, TeardownPhase};

    phases.push(TeardownPhase::GateB);
    match gates.active_pagefiles() {
        Ok(pf) if pf.is_empty() => {}
        Ok(pf) => {
            let _ = gates.unlock_volume();
            phases.push(TeardownPhase::ResumeOnline);
            return Err(ProvisionError::PagefileSafety(format!(
                "gate_b_active: {}",
                pf.join(",")
            )));
        }
        Err(e) => {
            let _ = gates.unlock_volume();
            phases.push(TeardownPhase::ResumeOnline);
            return Err(ProvisionError::PagefileSafety(format!("gate_b_query: {e}")));
        }
    }

    if gates.has_mounted_volume() {
        phases.push(TeardownPhase::FlushDismount);
        if let Err(e) = gates.flush_and_dismount() {
            let _ = gates.unlock_volume();
            phases.push(TeardownPhase::ResumeOnline);
            return Err(ProvisionError::PagefileSafety(format!(
                "flush_dismount: {e}"
            )));
        }
    }

    if state.registered_queue {
        phases.push(TeardownPhase::Unregister);
        disk.unregister_queue().map_err(ProvisionError::Disk)?;
        state.registered_queue = false;
    }
    if state.disk_created {
        phases.push(TeardownPhase::Destroy);
        disk.destroy_disk().map_err(ProvisionError::Disk)?;
        state.disk_created = false;
    }

    if gates.has_mounted_volume() {
        phases.push(TeardownPhase::Unlock);
        gates
            .unlock_volume()
            .map_err(|e| ProvisionError::Disk(format!("unlock: {e}")))?;
    }

    phases.push(TeardownPhase::Wipe);
    wipe.zero().map_err(ProvisionError::Disk)?;

    phases.push(TeardownPhase::Release);
    state.lease = None;
    state.online = false;
    let _ = cfg; // letter already applied by caller
    Ok(())
}

/// Unbuffered ProgramData line for stop/teardown classification (lab + force-kill safe).
pub(super) fn teardown_diag(msg: &str) {
    let path = std::path::Path::new(r"C:\ProgramData\RamShared\teardown-diag.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let ts = utc_ms();
        let _ = writeln!(f, "{ts} {msg}");
        let _ = f.flush();
    }
    eprintln!("{msg}");
    let _ = std::io::stderr().flush();
}

pub(super) fn preserve_failed_safe(reason: &str) -> ! {
    teardown_diag(&format!(
        "FailedSafe: {reason}; preserving disk, allocation, and lease until supervised reboot"
    ));
    loop {
        thread::park_timeout(Duration::from_secs(1));
    }
}

fn refuse_stop_online(stop_ctx: &mut StopContext<'_>, reason: &str) {
    teardown_diag(&format!(
        "teardown refused (code 7) resume Online: {reason}"
    ));
    stop_ctx.state.phase = RuntimePhase::Online;
    stop_ctx.row.begin_event("Online", utc_ms());
    sync_runtime_evidence(stop_ctx.row, stop_ctx.state);
    stop_ctx.row.error_class = Some("pagefile_safety".into());
    stop_ctx.row.error_code = Some("7".into());
    let _ = stop_ctx.evidence.append(stop_ctx.row);
    resume_online_stop(stop_ctx.stop, stop_ctx.resume_notifier);
}

pub(super) fn resume_online_stop(stop: &AtomicBool, resume_notifier: Option<&AtomicBool>) {
    if let Some(notifier) = resume_notifier {
        teardown_diag("SCM stop remains pending; safety retry scheduled");
        notifier.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        while notifier.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if notifier.load(Ordering::Acquire) {
            teardown_diag("FailedSafe: SCM retry monitor did not acknowledge stop refusal");
            return;
        }
        return;
    }
    stop.store(false, Ordering::Release);
}

pub(super) fn readonly_host_call_with_io_pump<T, M, F>(
    link: &mut WindowsDriverLink,
    dlink: &mut DriverLink<WindowsMappedQueue>,
    backend: &mut VramBackend<M>,
    timeout: Duration,
    operation: F,
) -> Result<T, String>
where
    T: Send + 'static,
    M: ramshared_vram::VramMemory,
    F: FnOnce() -> Result<T, crate::windows_host::HostError> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(operation());
    });
    let started = Instant::now();
    loop {
        match rx.try_recv() {
            Ok(result) => return result.map_err(|e| e.to_string()),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err("read-only host worker disconnected".into());
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "read-only host observation timed out after {} ms",
                timeout.as_millis()
            ));
        }
        match link.commit_and_fetch(Duration::from_millis(50)) {
            Ok(()) => dlink
                .commit_and_fetch(backend)
                .map(|_| ())
                .map_err(|e| format!("I/O pump during host observation: {e}"))?,
            Err(crate::windows_driver::IoctlError::Timeout) => {}
            Err(e) => return Err(format!("I/O pump during host observation: {e}")),
        }
    }
}

fn preserve_failed_safe_with_io<M>(
    reason: &str,
    io: &mut OnlineIo<'_, M>,
    broker: &mut BrokerSession<'_>,
    receiver: std::sync::mpsc::Receiver<
        Result<crate::windows_host::LockedVolume, crate::windows_host::HostError>,
    >,
) -> !
where
    M: ramshared_vram::VramMemory,
{
    teardown_diag(&format!(
        "FailedSafe: {reason}; preserving I/O, disk, allocation, and lease until supervised reboot"
    ));
    let mut lock_receiver = Some(receiver);
    let mut last_heartbeat = Instant::now();
    loop {
        if let Some(rx) = lock_receiver.as_ref() {
            match rx.try_recv() {
                Ok(result) => {
                    drop(result);
                    teardown_diag("FailedSafe: late volume-lock result drained and released");
                    lock_receiver = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    lock_receiver = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        match io.link.commit_and_fetch(Duration::from_millis(50)) {
            Ok(()) => {
                if let Err(e) = io.dlink.commit_and_fetch(io.backend) {
                    teardown_diag(&format!("FailedSafe I/O pump error: {e}"));
                }
            }
            Err(crate::windows_driver::IoctlError::Timeout) => {}
            Err(e) => teardown_diag(&format!("FailedSafe COMMIT error: {e}")),
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(1) {
            if let Err(e) = broker.tenant.heartbeat_psi(broker.stream) {
                teardown_diag(&format!("FailedSafe broker heartbeat error: {e}"));
            } else if let Err(e) = broker.stream.flush() {
                teardown_diag(&format!("FailedSafe broker flush error: {e}"));
            }
            last_heartbeat = Instant::now();
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn preserve_failed_safe_lease(
    reason: &str,
    tenant: &mut BrokerTenant,
    stream: &mut BrokStream,
) -> ! {
    teardown_diag(&format!(
        "FailedSafe: {reason}; allocation is free but lease ownership is retained"
    ));
    loop {
        if let Err(e) = tenant.heartbeat_psi(stream) {
            teardown_diag(&format!("FailedSafe broker heartbeat error: {e}"));
        } else if let Err(e) = stream.flush() {
            teardown_diag(&format!("FailedSafe broker flush error: {e}"));
        }
        thread::sleep(Duration::from_secs(1));
    }
}

pub(super) fn sync_runtime_evidence(row: &mut RuntimeEvidence, state: &RuntimeState) {
    row.mode = match state.mode {
        RunMode::Console => "console",
        RunMode::Scm => "scm",
        RunMode::ProbeCuda => "probe-cuda",
    }
    .into();
    row.phase = match state.phase {
        RuntimePhase::Stopped => "Stopped",
        RuntimePhase::WaitingForBroker => "WaitingForBroker",
        RuntimePhase::Leased => "Leased",
        RuntimePhase::CudaReady => "CudaReady",
        RuntimePhase::DiskCreated => "DiskCreated",
        RuntimePhase::QueueRegistered => "QueueRegistered",
        RuntimePhase::Online => "Online",
        RuntimePhase::Degraded => "Degraded",
        RuntimePhase::Stopping => "Stopping",
        RuntimePhase::FailedSafe => "FailedSafe",
    }
    .into();
    // `0` is not a lease id: `LeaseBook::next_id` starts at 1, so the row
    // encodes "no lease" with a value outside the id space rather than
    // inventing an identifier the tenant could be holding.
    row.lease_id = state.lease_id.unwrap_or(0);
    row.lease_bytes = state.lease_bytes;
    row.allocated_bytes = state.allocated_bytes;
    if !state.healthy {
        row.error_class = Some("failed_safe".into());
    } else {
        row.error_class = None;
        row.error_code = None;
    }
}

pub(super) fn sync_io_evidence(row: &mut RuntimeEvidence, link: &DriverLink<WindowsMappedQueue>) {
    let stats = link.stats();
    row.counters = IoCounters {
        reads: stats.reads,
        writes: stats.writes,
        flushes: stats.flushes,
        bytes_read: stats.bytes_read,
        bytes_written: stats.bytes_written,
        errors: stats.errors,
        outstanding: u64::from(link.q.sq_pending()),
    };
    row.latency = Some(summarize_latencies(&stats.latencies_us));
}

pub(super) struct CudaWatchdog {
    active_since: Arc<Mutex<Option<Instant>>>,
    fired: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

impl CudaWatchdog {
    pub(super) fn start(limit: Duration) -> Self {
        let active_since = Arc::new(Mutex::new(None::<Instant>));
        let fired = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let active_for_thread = Arc::clone(&active_since);
        let fired_for_thread = Arc::clone(&fired);
        let shutdown_for_thread = Arc::clone(&shutdown);
        thread::spawn(move || {
            while !shutdown_for_thread.load(Ordering::Acquire) {
                let expired = active_for_thread
                    .lock()
                    .ok()
                    .and_then(|active| *active)
                    .is_some_and(|started| started.elapsed() > limit);
                if expired && !fired_for_thread.swap(true, Ordering::AcqRel) {
                    eprintln!(
                        "CUDA watchdog: synchronous operation exceeded {} ms; preserving owners",
                        limit.as_millis()
                    );
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        Self {
            active_since,
            fired,
            shutdown,
        }
    }

    pub(super) fn begin(&self) {
        if let Ok(mut active) = self.active_since.lock() {
            *active = Some(Instant::now());
        } else {
            self.fired.store(true, Ordering::Release);
        }
    }

    pub(super) fn end(&self) {
        if let Ok(mut active) = self.active_since.lock() {
            *active = None;
        } else {
            self.fired.store(true, Ordering::Release);
        }
    }

    pub(super) fn fired(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }
}

impl Drop for CudaWatchdog {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

pub(super) fn error_after_release(
    tenant: &mut BrokerTenant,
    stream: &mut BrokStream,
    class: RuntimeErrorClass,
    code: i32,
    mut message: String,
) -> RuntimeError {
    if let Err(release_error) = tenant.release(stream) {
        message.push_str(&format!("; lease release failed: {release_error}"));
    }
    RuntimeError::new(class, code, message)
}
