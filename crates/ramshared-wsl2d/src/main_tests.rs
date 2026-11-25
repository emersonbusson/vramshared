// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests.rs
// Runtime integration tests for the daemon's NBD, broker and ublk adapters.
// Manifest-policy tests live in origin_identity/tests.rs and retain their
// function names; the remaining runtime suite uses the tests::* module path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

pub(super) use super::command_runtime::command_stdout_with_timeout;
pub(super) use super::gpu_worker_request::parse_isolated_gpu_worker_args;
pub(super) use super::gpu_worker_runtime::run_gpu_worker_with_selected_wddm;
pub(super) use super::gpu_worker_selection::{
    candidate_from_budget, gpu_candidate, revalidate_selected_adapter,
};
pub(super) use super::nbd_origin::run_nbd;
pub(super) use super::origin_runtime::OpenedOrigin;
pub(super) use super::worker_recovery::*;
pub(super) use super::*;
pub(super) use ramshared_block::BlockBackend;
pub(super) use ramshared_wsl2d::gpu_budget::{GpuBackendKind, worker_config_for_candidate};
pub(super) use ramshared_wsl2d::host_gate::{
    ControlPlaneAuthority, ControlPlaneState, GateError, LeaseToken, SealedOrigin,
    lease_after_connect,
};
pub(super) use std::fs::File;
pub(super) use std::io::Read;
pub(super) use std::os::fd::AsRawFd;
pub(super) use std::os::unix::fs::FileTypeExt;
pub(super) use std::os::unix::net::UnixListener;
pub(super) use std::os::unix::process::CommandExt;
pub(super) use std::path::{Path, PathBuf};
pub(super) use std::process::{Command as ProcessCommand, Stdio};
pub(super) use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
pub(super) use std::sync::{Arc, Mutex};
pub(super) use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(super) fn fixture_credit() -> ramshared_wsl2d::reply_budget::ReplyCredit {
    ramshared_wsl2d::reply_budget::ReplyBudget::new(1, 0)
        .admit(0)
        .unwrap()
}

#[path = "main_tests/bootstrap_coverage.rs"]
mod bootstrap_coverage;
#[path = "main_tests/cache_publication.rs"]
mod cache_publication;
#[path = "main_tests/command_process.rs"]
pub(super) mod command_process;
#[path = "main_tests/native_data.rs"]
mod native_data;
#[path = "main_tests/origin_boot.rs"]
mod origin_boot;
#[path = "main_tests/origin_members.rs"]
mod origin_members;
#[path = "main_tests/origin_release.rs"]
mod origin_release;
#[path = "main_tests/origin_shutdown.rs"]
mod origin_shutdown;
#[path = "main_tests/origin_startup.rs"]
mod origin_startup;
#[path = "main_tests/platform_admission.rs"]
mod platform_admission;
#[path = "main_tests/worker_observation.rs"]
mod worker_observation;
pub(super) use std::cell::{Cell, RefCell};
pub(super) use std::ffi::OsString;
pub(super) use std::os::unix::ffi::OsStringExt;
pub(super) use std::os::unix::fs::PermissionsExt;

/// The tier is released when the peer can no longer serve it, so the swap
/// table is what decides — not the device's mere presence.
#[test]
pub(super) fn the_swap_table_decides_whether_the_tier_is_still_armed() {
    let table = "Filename Type Size Used Priority\n/dev/nbd0 partition 2097148 64 100\n";
    for device in ["/dev/nbd0", "nbd0"] {
        assert_eq!(
            strict_exact_swap_state_from_text(table, device, canonical_nbd_identity).unwrap(),
            ExactSwapState::Active { used_kb: 64 }
        );
    }
    assert_eq!(
        strict_exact_swap_state_from_text(table, "/dev/nbd1", canonical_nbd_identity).unwrap(),
        ExactSwapState::Absent
    );
    assert!(
        strict_exact_swap_state_from_text(
            "unreadable or malformed",
            "/dev/nbd0",
            canonical_nbd_identity
        )
        .is_err()
    );
}

/// Builds the broker control-plane configuration before binding any listener.
/// This preserves the exact telemetry and endpoint wiring while keeping the
/// decision boundary testable without a GPU, swap device, or NBD client.
#[cfg(test)]
pub(super) fn build_broker_config(
    slices: u16,
    sock: &str,
    advertise_tcp: Option<(String, u16)>,
    arbiter_addr: std::net::SocketAddr,
    telemetry_jsonl: Option<std::path::PathBuf>,
) -> BrokerConfig {
    build_broker_config_with_tick(
        slices,
        sock,
        advertise_tcp,
        arbiter_addr,
        telemetry_jsonl,
        Duration::from_secs(2),
    )
}

#[cfg(test)]
pub(super) fn serve_broker_jobs_with_poll_and_heartbeat<B: BlockBackend>(
    backend: B,
    rt: BrokerWorkerRuntime,
    residency: impl FnMut(u64) -> Option<DemoteReason>,
    heartbeat: impl FnMut() -> Option<DemoteReason>,
    poll_interval: Duration,
) -> B {
    serve_broker_jobs_with_poll_heartbeat_and_reply_hook(
        backend,
        rt,
        residency,
        heartbeat,
        poll_interval,
        Duration::from_secs(1),
        || {},
    )
}

/// Generic VRAM broker lifecycle after driver construction. The production
/// wrapper supplies the real memory lock and broker setup; tests supply a
/// heap-backed provider and pre-stopped bounded runtime. This preserves the
/// same zero-before-release ordering while making it observable without CUDA.
#[allow(clippy::too_many_arguments)] // explicit daemon boundary keeps lock/setup test seams injectable
pub(super) fn run_broker_with_setup<P, L, S>(
    provider: P,
    slice_bytes: u64,
    slices: u16,
    _sock: String,
    force: bool,
    lock: L,
    setup: S,
    worker_poll: Duration,
) -> Result<(), Box<dyn std::error::Error>>
where
    P: VramProvider,
    L: FnOnce(bool, bool) -> Result<(), Box<dyn std::error::Error>>,
    S: FnOnce() -> Result<BrokerRuntime, Box<dyn std::error::Error>>,
{
    let total = (slices as u64)
        .checked_mul(slice_bytes)
        .ok_or("--slices * --slice-mb: overflow")?;
    let total_len = usize::try_from(total)
        .map_err(|_| "--slices * --slice-mb exceeds addressable allocation size")?;

    // The provider has already been initialized by the production shell. The
    // lifecycle below remains generic over VramProvider/VramMemory (RF-G1).
    let (free, total_vram) = provider.mem_info()?;
    eprintln!(
        "[ramsharedd] VRAM free={} MiB total={} MiB",
        free >> 20,
        total_vram >> 20
    );
    let mut mem = provider.alloc(total_len)?;
    mem.zero()?;
    // Lock only mappings that already exist. The canary and any later GPU/DXG
    // mappings must never inherit a process-wide MCL_FUTURE obligation.
    if let Err(error) = lock(force, false) {
        let _ = mem.zero();
        return Err(error);
    }
    let mut backend = VramBackend::new(mem, BLOCK_SIZE);
    eprintln!(
        "[ramsharedd] broker VRAM: {slices} slices x {} MiB = {} MiB, block_size={BLOCK_SIZE}",
        slice_bytes >> 20,
        total >> 20
    );

    // Residency canary (§9.4): separated region, not addressable by NBD.
    let canary_region = match provider.alloc(CANARY_BYTES) {
        Ok(memory) => memory,
        Err(error) => {
            backend.zero()?;
            return Err(error.into());
        }
    };
    let mut probe = CanaryProbe::new(canary_region);
    let mut cadence = Cadence::new(CANARY_EVERY);
    let mut sampler = ResidencySampler::new(ResidencyConfig::default());
    let mut canary: Option<Canary> = None;
    let mut baseline: Vec<u64> = Vec::new();

    let rt = match setup() {
        Ok(runtime) => runtime,
        Err(error) => {
            let backend_zeroed = backend.zero();
            let probe_zeroed = probe.zero();
            backend_zeroed?;
            probe_zeroed?;
            return Err(error);
        }
    };
    let (worker, broker, shutdown, socket, _shutdown_bridge) = rt.into_parts();
    let broker_monitor = BrokerJoinMonitor::start(broker, shutdown);
    let vram = std::sync::Arc::clone(&worker.vram);
    backend = serve_broker_jobs_with_poll_and_heartbeat(
        backend,
        worker,
        |lat_us| {
            let mut residency_state = ResidencyCheckState {
                canary: &mut canary,
                baseline: &mut baseline,
                sampler: &mut sampler,
                cadence: &mut cadence,
                probe: &mut probe,
                free_floor_bytes: ResidencyConfig::default().free_floor_bytes,
            };
            residency_check(lat_us, &mut residency_state, || match provider.mem_info() {
                Ok((f, t)) => {
                    vram.free.store(f, Ordering::Relaxed);
                    vram.total.store(t, Ordering::Relaxed);
                    Some(f)
                }
                Err(_) => {
                    vram.free.store(0, Ordering::Relaxed);
                    vram.total.store(0, Ordering::Relaxed);
                    None
                }
            })
        },
        || match provider.mem_info() {
            Ok((f, t)) => {
                vram.free.store(f, Ordering::Relaxed);
                vram.total.store(t, Ordering::Relaxed);
                None
            }
            Err(error) => {
                eprintln!("[ramsharedd] GPU probe failed during idle heartbeat: {error}");
                vram.free.store(0, Ordering::Relaxed);
                vram.total.store(0, Ordering::Relaxed);
                Some(DemoteReason::GpuLost)
            }
        },
        worker_poll,
    );

    let broker_result = broker_monitor.finish();
    let zeroed = backend.zero();
    let _ = probe.zero(); // DT-12/DT-17: zeroes the canary-region as well
    drop(socket);
    zeroed?;
    broker_result?;
    eprintln!("[ramsharedd] broker VRAM stopped (VRAM zeroed)");
    Ok(())
}

pub(super) struct TestMemory {
    bytes: RefCell<Vec<u8>>,
}

impl TestMemory {
    fn new(bytes: usize) -> Self {
        Self {
            bytes: RefCell::new(vec![0; bytes]),
        }
    }

    fn range(&self, off: u64, len: usize) -> Result<(usize, usize), ramshared_vram::VramError> {
        let start = usize::try_from(off).map_err(|_| ramshared_vram::VramError::OutOfRange {
            off,
            len: len as u64,
            size: self.bytes.borrow().len() as u64,
        })?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| ramshared_vram::VramError::OutOfRange {
                off,
                len: len as u64,
                size: self.bytes.borrow().len() as u64,
            })?;
        if end > self.bytes.borrow().len() {
            return Err(ramshared_vram::VramError::OutOfRange {
                off,
                len: len as u64,
                size: self.bytes.borrow().len() as u64,
            });
        }
        Ok((start, end))
    }
}

impl VramMemory for TestMemory {
    fn len(&self) -> usize {
        self.bytes.borrow().len()
    }

    fn zero(&mut self) -> Result<(), ramshared_vram::VramError> {
        self.bytes.borrow_mut().fill(0);
        Ok(())
    }

    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), ramshared_vram::VramError> {
        let (start, end) = self.range(off, dst.len())?;
        dst.copy_from_slice(&self.bytes.borrow()[start..end]);
        Ok(())
    }

    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), ramshared_vram::VramError> {
        let (start, end) = self.range(off, src.len())?;
        self.bytes.borrow_mut()[start..end].copy_from_slice(src);
        Ok(())
    }
}

/// In-memory VRAM that records each secure-wipe request. It lets the
/// lifecycle refusal tests prove wipe ordering without a GPU context.
pub(super) struct ZeroRecordingMemory {
    bytes: usize,
    zeroed: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
}

impl VramMemory for ZeroRecordingMemory {
    fn len(&self) -> usize {
        self.bytes
    }

    fn zero(&mut self) -> Result<(), ramshared_vram::VramError> {
        self.zeroed
            .lock()
            .expect("test zero record lock")
            .push(self.bytes);
        Ok(())
    }

    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), ramshared_vram::VramError> {
        Err(ramshared_vram::VramError::OutOfRange {
            off,
            len: dst.len() as u64,
            size: self.bytes as u64,
        })
    }

    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), ramshared_vram::VramError> {
        Err(ramshared_vram::VramError::OutOfRange {
            off,
            len: src.len() as u64,
            size: self.bytes as u64,
        })
    }
}

pub(super) struct ZeroRecordingProvider {
    zeroed: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
}

impl VramProvider for ZeroRecordingProvider {
    type Mem<'a>
        = ZeroRecordingMemory
    where
        Self: 'a;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
        Ok(ZeroRecordingMemory {
            bytes,
            zeroed: std::sync::Arc::clone(&self.zeroed),
        })
    }

    fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
        Ok((8 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024))
    }
}

pub(super) struct BudgetSnapshotProvider {
    snapshot: GpuBudgetSnapshot,
}

impl VramProvider for BudgetSnapshotProvider {
    type Mem<'a>
        = TestMemory
    where
        Self: 'a;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
        Ok(TestMemory::new(bytes))
    }

    fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
        Ok((
            self.snapshot.available_bytes(),
            self.snapshot
                .total_bytes
                .unwrap_or(self.snapshot.budget_bytes),
        ))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, ramshared_vram::VramError> {
        Ok(self.snapshot.clone())
    }
}

pub(super) fn adapter_budget_snapshot(
    adapter: Option<GpuAdapterIdentity>,
    sampled_at: Instant,
) -> GpuBudgetSnapshot {
    GpuBudgetSnapshot {
        adapter,
        total_bytes: Some(8 * GIB),
        budget_bytes: 8 * GIB,
        used_bytes: GIB,
        source: ramshared_vram::GpuBudgetSource::DriverReported,
        sampled_at,
    }
}

pub(super) fn test_gpu_identity() -> GpuAdapterIdentity {
    GpuAdapterIdentity {
        node_mask: None,
        backend: "vulkan".into(),
        key: "12000000000000000000000000000000".into(),
        luid: None,
    }
}

/// Provider that advertises a GPU cache codec (DT-2 / RF-7). Used to prove
/// codec capability never bypasses adapter revalidation.
pub(super) struct CodecCapableProvider {
    snapshot: GpuBudgetSnapshot,
    codec: ramshared_vram::FakeCodec,
}

impl VramProvider for CodecCapableProvider {
    type Mem<'a>
        = TestMemory
    where
        Self: 'a;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
        Ok(TestMemory::new(bytes))
    }

    fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
        Ok((
            self.snapshot.available_bytes(),
            self.snapshot
                .total_bytes
                .unwrap_or(self.snapshot.budget_bytes),
        ))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, ramshared_vram::VramError> {
        Ok(self.snapshot.clone())
    }

    fn cache_codec(&self) -> Option<&dyn ramshared_vram::GpuCacheCodec<TestMemory>> {
        Some(&self.codec)
    }
}

pub(super) fn daemon_argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

pub(super) struct TestOrigin {
    bytes: std::cell::RefCell<Vec<u8>>,
    fail: std::rc::Rc<std::cell::Cell<bool>>,
}

impl ramshared_block::OriginStorage for TestOrigin {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, ramshared_block::IoError> {
        if self.fail.get() {
            return Err(ramshared_block::IoError("injected origin failure".into()));
        }
        let start = off as usize;
        let count = buf
            .len()
            .min(self.bytes.borrow().len().saturating_sub(start));
        buf[..count].copy_from_slice(&self.bytes.borrow()[start..start + count]);
        Ok(count)
    }

    fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, ramshared_block::IoError> {
        if self.fail.get() {
            return Err(ramshared_block::IoError("injected origin failure".into()));
        }
        let start = off as usize;
        let count = data
            .len()
            .min(self.bytes.borrow().len().saturating_sub(start));
        self.bytes.borrow_mut()[start..start + count].copy_from_slice(&data[..count]);
        Ok(count)
    }

    fn sync_data(&self) -> Result<(), ramshared_block::IoError> {
        if self.fail.get() {
            Err(ramshared_block::IoError("injected origin failure".into()))
        } else {
            Ok(())
        }
    }
}

#[path = "main_tests/worker_fixture.rs"]
pub(super) mod worker_fixture;
use worker_fixture::*;

/// Attach a worker handle whose thread has already finished.
///
/// `MSG_DISABLE_REQ` breaks the worker frame loop, so a released cache in
/// production is left with an exited child. The mock thread has no such
/// exit, so the test drives the same end state directly.
pub(super) fn attach_exited_worker(runtime: &mut IsolatedWorkerRuntime) {
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let handle = std::thread::spawn(|| {});
    while !handle.is_finished() {
        std::thread::yield_now();
    }
    runtime.supervisor = Some(IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Thread {
            stop,
            handle: Some(handle),
        }),
    });
}

pub(super) struct BrokerWorkerShutdownGuard<'a>(&'a BrokerShutdown);

impl Drop for BrokerWorkerShutdownGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.request();
    }
}

pub(super) struct TestFailingBackend {
    fail_io: std::sync::Arc<AtomicBool>,
    size: u64,
}

impl TestFailingBackend {
    fn new(size: u64, fail_io: bool) -> (Self, std::sync::Arc<AtomicBool>) {
        let flag = std::sync::Arc::new(AtomicBool::new(fail_io));
        (
            Self {
                fail_io: std::sync::Arc::clone(&flag),
                size,
            },
            flag,
        )
    }
}

impl BlockBackend for TestFailingBackend {
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn block_size(&self) -> u32 {
        512
    }
    fn read_at(&mut self, _off: u64, _buf: &mut [u8]) -> Result<(), ramshared_block::IoError> {
        if self.fail_io.load(Ordering::SeqCst) {
            Err(ramshared_block::IoError("simulated I/O failure".into()))
        } else {
            Ok(())
        }
    }
    fn write_at(&mut self, _off: u64, _data: &[u8]) -> Result<(), ramshared_block::IoError> {
        if self.fail_io.load(Ordering::SeqCst) {
            Err(ramshared_block::IoError("simulated I/O failure".into()))
        } else {
            Ok(())
        }
    }
    fn flush(&mut self) -> Result<(), ramshared_block::IoError> {
        Ok(())
    }
}

#[path = "main_tests/broker_circuit_and_wddm.rs"]
mod broker_circuit_and_wddm;
#[path = "main_tests/broker_setup_and_nbd_basic.rs"]
mod broker_setup_and_nbd_basic;
#[path = "main_tests/daemon_action_and_socket.rs"]
mod daemon_action_and_socket;
#[path = "main_tests/lease_authority_and_probe.rs"]
mod lease_authority_and_probe;
#[path = "main_tests/nbd_budget_and_recovery.rs"]
mod nbd_budget_and_recovery;
#[path = "main_tests/nbd_residency_and_version.rs"]
mod nbd_residency_and_version;
#[path = "main_tests/nbd_teardown_and_drain.rs"]
mod nbd_teardown_and_drain;
#[path = "main_tests/origin_and_cli_flags.rs"]
mod origin_and_cli_flags;
#[path = "main_tests/origin_cache_modes.rs"]
mod origin_cache_modes;
#[path = "main_tests/ublk_lifecycle.rs"]
mod ublk_lifecycle;
#[path = "main_tests/ublk_shutdown_and_worker.rs"]
mod ublk_shutdown_and_worker;
#[path = "main_tests/unix_socket_and_worker_ctrl.rs"]
mod unix_socket_and_worker_ctrl;
#[path = "main_tests/worker_telemetry_and_publish.rs"]
mod worker_telemetry_and_publish;

#[path = "main_tests/startup_recovery.rs"]
mod startup_recovery;

#[path = "main_tests/worker_set.rs"]
mod worker_set;

#[path = "main_tests/candidate_recovery.rs"]
mod candidate_recovery;
