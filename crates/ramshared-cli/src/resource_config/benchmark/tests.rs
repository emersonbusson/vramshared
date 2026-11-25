// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark/tests.rs
pub(super) use std::cell::RefCell;

pub(super) use std::collections::HashSet;

pub(super) use std::fs::{self, File};

pub(super) use std::io::{self, Cursor};

pub(super) use std::os::unix::fs::{PermissionsExt, symlink};

pub(super) use std::path::{Path, PathBuf};

pub(super) use std::process::{Child, ExitStatus};

pub(super) use std::rc::Rc;

pub(super) use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) use std::time::Duration;

pub(super) use ramshared_exec::retained_process::{NativeProcessControl, ProcessControl};

pub(super) use super::custody::{
    AuditEvent, AuditSink, FileAuditSink, ProcessIdentity, WorkerLease, WorkerLeaseStatus,
    acquire_launching_lease, clear_verified_lease, open_verified_dir_chain, prove_process_exited,
    read_existing_lease, read_linux_proc_start_time, recover_lease, replace_lease,
    statx_descriptor,
};

pub(super) use super::model::{
    BENCHMARK_REQUIRED_FREE_BYTES, BenchmarkVolume, MAX_BENCHMARK_VOLUMES,
    PER_VOLUME_WRITE_CAP_BYTES, ROUNDS_PER_VOLUME, RUN_WRITE_CAP_BYTES, RuntimeHealthSnapshot,
    SAMPLE_PAYLOAD_BYTES, SEQ_WRITE_BYTES, SMALL_WRITE_BYTES, SMALL_WRITES_PER_SAMPLE,
    WorkerSampleOutcome, build_benchmark_plan, recommend_leader, revalidate_volume,
    rotate_schedule, small_write_slot, summarize_volume,
};

pub(super) use super::worker::{
    SubprocessWorkerRunner, WorkerReply, WorkerRequest, WorkerRunContext, WorkerRunOutcome,
    WorkerRunner, execute_worker_sample, relative_temp_path, run_worker_from_io,
    verify_or_cleanup_volume_temp,
};

pub(super) use super::{
    BenchmarkAction, BenchmarkEnvironment, BenchmarkRequest, BenchmarkUseCase, render_plan_text,
    run_with_env,
};

pub(super) use crate::resource_config::tests::{fixture_block_device, fixture_snapshot};

pub(super) use crate::resource_config::{
    InventoryError, ResourceSnapshot, RuntimePlatform, SwapDevice,
};

pub(super) static TEST_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

pub(super) fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub(super) fn current_boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "test-boot-id".into())
}

pub(super) fn make_temp_root(label: &str) -> PathBuf {
    let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ramshared-bench-test-{}-{}-{seq}",
        std::process::id(),
        label
    ));
    fs::create_dir_all(&root).expect("create temp test root");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("chmod temp test root");
    root
}

pub(super) fn make_volume_dir(parent: &Path, name: &str) -> (PathBuf, u64, String) {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).expect("create temp volume dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod temp volume dir");
    let handle = File::open(&dir).expect("open temp volume dir");
    let stat = statx_descriptor(&handle).expect("statx temp volume dir");
    let major_minor = format!("{}:{}", stat.stx_dev_major, stat.stx_dev_minor);
    (dir, stat.stx_mnt_id, major_minor)
}

pub(super) fn make_native_device_at(
    idx: usize,
    mount_dir: &Path,
    mount_id: u64,
    major_minor: &str,
    free_bytes: u64,
    observed_unix_ms: u64,
) -> ramshared_block::storage_inventory::BlockDevice {
    let mut dev = fixture_block_device();
    dev.name = format!("nvme{idx}n1p1");
    dev.path = format!("/dev/nvme{idx}n1p1");
    dev.parent = Some(format!("nvme{idx}n1"));
    dev.uuid = Some(format!("fs-uuid-{idx}"));
    dev.partition_uuid = Some(format!("part-uuid-{idx}"));
    dev.parent_hardware_identity = Some(format!("wwn:wwn-{idx}"));
    dev.major_minor = Some(major_minor.to_owned());
    let mp = mount_dir.to_string_lossy().into_owned();
    dev.mountpoints = vec![mp.clone()];
    dev.mounts[0].mount_id = mount_id;
    dev.mounts[0].major_minor = major_minor.to_owned();
    dev.mounts[0].mountpoint = mp;
    dev.mounts[0].source = dev.path.clone();
    dev.mounts[0].available_bytes = Some(free_bytes);
    dev.mounts[0].capacity_observed_unix_ms = Some(observed_unix_ms);
    dev
}

pub(super) fn make_native_snapshot(
    devices: Vec<ramshared_block::storage_inventory::BlockDevice>,
    now: u64,
) -> ResourceSnapshot {
    let mut snapshot = fixture_snapshot();
    snapshot.platform = RuntimePlatform::NativeLinux;
    snapshot.observed_unix_ms = now;
    snapshot.guest_observed_unix_ms = now;
    snapshot.windows = None;
    snapshot.block_devices = devices;
    snapshot
}

pub(super) fn healthy_runtime() -> RuntimeHealthSnapshot {
    RuntimeHealthSnapshot {
        ramshared_off: true,
        psi_full_avg10: Some(0.15),
        telemetry_healthy: true,
        telemetry_detail: None,
    }
}

pub(super) struct StubWorkerRunner {
    pub(super) outcomes: Vec<Result<WorkerRunOutcome, InventoryError>>,
    pub(super) seen_requests: Vec<WorkerRequest>,
}

impl StubWorkerRunner {
    fn from_outcomes(outcomes: Vec<Result<WorkerRunOutcome, InventoryError>>) -> Self {
        Self {
            outcomes,
            seen_requests: Vec::new(),
        }
    }
}

impl WorkerRunner for StubWorkerRunner {
    fn run_sample(
        &mut self,
        request: &WorkerRequest,
        run_ctx: WorkerRunContext<'_>,
        lease: &mut WorkerLease,
        audit: &mut dyn AuditSink,
    ) -> Result<WorkerRunOutcome, InventoryError> {
        let _ = (run_ctx.current_boot_id, run_ctx.use_case, audit);
        self.seen_requests.push(request.clone());
        lease.pid = 999_999;
        lease.process_start_time = 1;
        lease.status = WorkerLeaseStatus::Active;
        replace_lease(run_ctx.state_dir, run_ctx.owner_uid, lease)?;
        if self.outcomes.is_empty() {
            Ok(WorkerRunOutcome::Completed(sample_outcome(
                31_000_000,
                vec![50_000; SMALL_WRITES_PER_SAMPLE],
                "aa",
            )))
        } else {
            self.outcomes.remove(0)
        }
    }
}

pub(super) struct RealSampleWorkerRunner {
    pub(super) seen_rounds: Rc<RefCell<Vec<(usize, String, u64)>>>,
}

impl WorkerRunner for RealSampleWorkerRunner {
    fn run_sample(
        &mut self,
        request: &WorkerRequest,
        run_ctx: WorkerRunContext<'_>,
        lease: &mut WorkerLease,
        _audit: &mut dyn AuditSink,
    ) -> Result<WorkerRunOutcome, InventoryError> {
        self.seen_rounds.borrow_mut().push((
            request.round,
            request.volume.stable_id(),
            request.byte_ceiling,
        ));
        lease.status = WorkerLeaseStatus::Active;
        replace_lease(run_ctx.state_dir, run_ctx.owner_uid, lease)?;
        match execute_worker_sample(request) {
            WorkerReply::Completed(outcome) => Ok(WorkerRunOutcome::Completed(outcome)),
            WorkerReply::Failed { detail, .. } => Ok(WorkerRunOutcome::Failed { detail }),
        }
    }
}

pub(super) struct FailingAuditSink {
    pub(super) fail_on_action: &'static str,
    pub(super) inner: FileAuditSink,
}

impl AuditSink for FailingAuditSink {
    fn record_event(&mut self, event: &AuditEvent) -> Result<(), String> {
        if event.action == self.fail_on_action {
            return Err(format!("injected audit failure on {}", event.action));
        }
        self.inner.record_event(event)
    }
}

pub(super) fn sample_outcome(
    seq_elapsed_ns: u64,
    small_write_latencies_ns: Vec<u64>,
    hex_byte: &str,
) -> WorkerSampleOutcome {
    let digest = hex_byte.repeat(32);
    WorkerSampleOutcome {
        bytes_written: SAMPLE_PAYLOAD_BYTES,
        seq_bytes_written: SEQ_WRITE_BYTES,
        small_write_count: SMALL_WRITES_PER_SAMPLE,
        seq_elapsed_ns,
        small_write_latencies_ns,
        expected_sha256: digest.clone(),
        observed_sha256: digest,
        file_dev: 1,
        file_ino: 2,
        cleanup_verified: true,
    }
}

pub(super) fn synthetic_latencies_us(volume_index: usize, round: usize) -> WorkerSampleOutcome {
    // Give volume 1 significantly faster seq and faster 4KiB p99 by default when desired.
    let seq_elapsed_ns = match volume_index {
        0 => 310_000_000 + (round as u64) * 5_000_000, // ~100 MiB/s
        _ => 155_000_000 + (round as u64) * 2_000_000, // ~200 MiB/s
    };
    let mut small_write_latencies_ns = Vec::with_capacity(SMALL_WRITES_PER_SAMPLE);
    for i in 0..SMALL_WRITES_PER_SAMPLE {
        let base_ns = if volume_index == 0 { 400_000 } else { 150_000 };
        small_write_latencies_ns.push(base_ns + (i as u64) * 500);
    }
    sample_outcome(seq_elapsed_ns, small_write_latencies_ns, "de")
}

pub(super) fn make_test_env<'a>(
    state_dir: PathBuf,
    snapshot: ResourceSnapshot,
    health: RuntimeHealthSnapshot,
    now_unix_ms: u64,
    audit: Box<dyn AuditSink + 'a>,
    worker_runner: Box<dyn WorkerRunner + 'a>,
) -> BenchmarkEnvironment<'a> {
    BenchmarkEnvironment {
        owner_uid: current_uid(),
        state_dir,
        collect_snapshot: Box::new(move || Ok(snapshot.clone())),
        probe_health: Box::new(move || health.clone()),
        load_profile: Box::new(|_| Err(InventoryError("unexpected profile load".into()))),
        now_unix_ms: Box::new(move || now_unix_ms),
        read_boot_id: Box::new(|| Ok(current_boot_id())),
        read_proc_start_time: Box::new(read_linux_proc_start_time),
        audit,
        worker_runner,
        is_tty: false,
        consent_reader: Box::new(Cursor::new(Vec::<u8>::new())),
    }
}

pub(super) struct StuckProcessControl;

impl ProcessControl for StuckProcessControl {
    fn observe_exit(&self, _child: &mut Child) -> io::Result<bool> {
        Ok(false)
    }

    fn stop_group(&self, child: &mut Child) -> io::Result<()> {
        let _ = NativeProcessControl.stop_group(child);
        let _ = NativeProcessControl.reap(child);
        // Simulate failure to stop the worker group so lease persists as worker_stuck.
        Err(io::Error::other("simulated stuck worker process group"))
    }

    fn reap(&self, _child: &mut Child) -> io::Result<Option<ExitStatus>> {
        Ok(None)
    }

    fn group_absent(&self, _pgid: u32) -> io::Result<bool> {
        Ok(false)
    }
}

pub(super) struct ObserveErrorProcessControl;

impl ProcessControl for ObserveErrorProcessControl {
    fn observe_exit(&self, _child: &mut Child) -> io::Result<bool> {
        Err(io::Error::other("simulated waitpid failure"))
    }

    fn stop_group(&self, child: &mut Child) -> io::Result<()> {
        NativeProcessControl.stop_group(child)
    }

    fn reap(&self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        NativeProcessControl.reap(child)
    }

    fn group_absent(&self, pgid: u32) -> io::Result<bool> {
        NativeProcessControl.group_absent(pgid)
    }
}

#[path = "tests/orchestration_and_custody.rs"]
mod orchestration_and_custody;
#[path = "tests/suite_extra.rs"]
mod suite_extra;
#[path = "tests/worker_and_reporting.rs"]
mod worker_and_reporting;
