// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/product_online.rs
//! Windows product Online composition (SPEC DT-3/DT-4/DT-7):
//! broker lease → CUDA DeviceMem → CREATE/REGISTER → I/O loop on one thread.
//!
//! Cover: N/A for unsafe Windows bits; pure sequencing tested via RuntimeOps.

#![cfg(windows)]

mod gates;
mod teardown;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ramshared_block::VramBackend;
use ramshared_cuda::Cuda;

use crate::broker_tenant::BrokerTenant;
use crate::config::WinDriveConfig;
use crate::driver_link::DriverLink;
use crate::evidence::{EvidenceWriter, RuntimeEvidence, new_process_run_id, utc_ms};
use crate::ipc::NamedPipeBrokerStream as BrokStream;
use crate::proto::DiskParams;
use crate::runtime::{
    RunMode, RuntimeError, RuntimeErrorClass, RuntimePhase, RuntimeState, RuntimeSummary,
};
use crate::service::PagefileGates;
use crate::windows_driver::{WindowsDriverLink, WindowsMappedQueue};
use crate::windows_host::WindowsHostState;

use teardown::{
    BrokerSession, CudaWatchdog, OnlineIo, StopContext, error_after_release,
    finalize_released_teardown, lock_mounted_volume_with_io, prepare_teardown_gates,
    preserve_failed_safe, readonly_host_call_with_io_pump, run_gate_b_and_teardown,
    sync_io_evidence, sync_runtime_evidence, teardown_diag,
};

const STARTUP_LUN_PROVIDER_TIMEOUT: Duration = Duration::from_secs(12);
const STARTUP_LUN_PUMP_TIMEOUT: Duration = Duration::from_secs(15);

/// Serial: 16 uppercase hex digits derived from run id hash (DT-11).
pub fn serial_from_run_id(run_id: &str) -> [u8; 16] {
    let mut out = [b'0'; 16];
    let h = simple_hash(run_id.as_bytes());
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for (i, digit) in out.iter_mut().enumerate() {
        let nibble = ((h >> (i * 4)) & 0xF) as usize;
        *digit = HEX[nibble];
    }
    out
}

fn simple_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Run product Online until `stop` is set or fatal error.
pub fn run_product_online(
    cfg: &WinDriveConfig,
    mode: RunMode,
    stop: Arc<AtomicBool>,
    resume_notifier: Option<Arc<AtomicBool>>,
) -> Result<RuntimeSummary, RuntimeError> {
    let run_id = new_process_run_id();
    let mut state = RuntimeState::new(mode);
    let serial = serial_from_run_id(&run_id);
    let serial_str = String::from_utf8_lossy(&serial).into_owned();

    // Evidence (diagnostic only).
    let mut evidence = EvidenceWriter::open(cfg.evidence_path().join(format!("{run_id}.jsonl")))
        .map_err(|e| RuntimeError::new(RuntimeErrorClass::Internal, 1, e.to_string()))?;
    let mut row = RuntimeEvidence::base(&run_id, "Stopped");
    row.lun_serial = serial_str.clone();
    row.lun_size_bytes = cfg.size_bytes;
    row.requested_bytes = cfg.size_bytes;
    row.queue_depth = cfg.queue_depth;
    row.max_io_bytes = cfg.max_io_bytes;
    row.cuda_ordinal = cfg.cuda_device;
    sync_runtime_evidence(&mut row, &state);
    let _ = evidence.append(&row);

    // --- Broker lease ---
    let mut stream = BrokStream::connect_product_pipe(
        Instant::now() + Duration::from_secs(cfg.broker_ready_timeout_secs),
    )
    .map_err(|error| {
        RuntimeError::new(
            RuntimeErrorClass::Broker,
            3,
            format!("named-pipe connect: {error:?}"),
        )
    })?;
    let mut tenant = BrokerTenant::new(cfg.tenant.clone(), Duration::from_secs(cfg.heartbeat_secs));
    tenant
        .register(&mut stream)
        .map_err(|e| RuntimeError::new(RuntimeErrorClass::Broker, 3, e.to_string()))?;
    let lease = tenant
        .acquire(&mut stream, cfg.size_bytes)
        .map_err(|e| RuntimeError::new(RuntimeErrorClass::Broker, 3, e.to_string()))?;
    macro_rules! lease_try {
        ($expr:expr, $class:expr, $code:expr) => {
            match $expr {
                Ok(value) => value,
                Err(error) => {
                    return Err(error_after_release(
                        &mut tenant,
                        &mut stream,
                        $class,
                        $code,
                        error.to_string(),
                    ));
                }
            }
        };
    }
    state.lease_id = Some(lease.lease);
    state.lease_bytes = lease.bytes;
    state.phase = RuntimePhase::Leased;
    row.begin_event("Leased", utc_ms());
    sync_runtime_evidence(&mut row, &state);
    let _ = evidence.append(&row);

    // --- CUDA on this thread (affinity) ---
    let cuda = lease_try!(Cuda::load(), RuntimeErrorClass::Cuda, 4);
    let count = lease_try!(cuda.device_count(), RuntimeErrorClass::Cuda, 4);
    if cfg.cuda_device as i32 >= count {
        return Err(error_after_release(
            &mut tenant,
            &mut stream,
            RuntimeErrorClass::Cuda,
            4,
            format!("cuda_device {} >= {count}", cfg.cuda_device),
        ));
    }
    let dev = lease_try!(
        cuda.device(cfg.cuda_device as i32),
        RuntimeErrorClass::Cuda,
        4
    );
    let ctx = lease_try!(cuda.create_context(&dev), RuntimeErrorClass::Cuda, 4);
    let (free, total) = lease_try!(ctx.mem_info(), RuntimeErrorClass::Cuda, 4);
    let reserve = cfg.effective_reserve_bytes(total as u64) as usize;
    let Some(need) = (cfg.size_bytes as usize).checked_add(reserve) else {
        return Err(error_after_release(
            &mut tenant,
            &mut stream,
            RuntimeErrorClass::Config,
            2,
            "size+reserve overflow".into(),
        ));
    };
    if free < need {
        return Err(error_after_release(
            &mut tenant,
            &mut stream,
            RuntimeErrorClass::Cuda,
            4,
            format!("free {free} < need {need}"),
        ));
    }
    if let Err(e) = tenant.coresidence_gate(free as u64, cfg.size_bytes) {
        return Err(error_after_release(
            &mut tenant,
            &mut stream,
            RuntimeErrorClass::Cuda,
            4,
            e.to_string(),
        ));
    }
    let size = cfg.size_bytes as usize;
    let mut mem = lease_try!(ctx.alloc(size), RuntimeErrorClass::Cuda, 4);
    if let Err(error) = mem.zero() {
        return Err(error_after_release(
            &mut tenant,
            &mut stream,
            RuntimeErrorClass::Cuda,
            4,
            error.to_string(),
        ));
    }
    state.allocated_bytes = cfg.size_bytes;
    state.phase = RuntimePhase::CudaReady;
    row.begin_event("CudaReady", utc_ms());
    sync_runtime_evidence(&mut row, &state);
    row.free_bytes = free as u64;
    row.reserve_bytes = reserve as u64;
    row.cuda_name = dev.name().to_string();
    let _ = evidence.append(&row);

    let mut backend = VramBackend::new(mem, cfg.block_size);

    // --- Driver CREATE + REGISTER ---
    let mut link = match WindowsDriverLink::open() {
        Ok(link) => link,
        Err(error) => {
            let mut message = error.to_string();
            if let Err(wipe_error) = backend.zero() {
                message.push_str(&format!("; VRAM wipe failed: {wipe_error}"));
            }
            return Err(error_after_release(
                &mut tenant,
                &mut stream,
                RuntimeErrorClass::Abi,
                5,
                message,
            ));
        }
    };
    let params = DiskParams {
        size_bytes: cfg.size_bytes,
        block_size: cfg.block_size,
        reserved: 0,
        serial,
    };
    if let Err(e) = link.create_disk(&params) {
        let mut message = e.to_string();
        if let Err(wipe_error) = backend.zero() {
            message.push_str(&format!("; VRAM wipe failed: {wipe_error}"));
        }
        return Err(error_after_release(
            &mut tenant,
            &mut stream,
            RuntimeErrorClass::Abi,
            5,
            message,
        ));
    }
    state.phase = RuntimePhase::DiskCreated;
    row.begin_event("DiskCreated", utc_ms());
    sync_runtime_evidence(&mut row, &state);
    let _ = evidence.append(&row);

    let q = match WindowsMappedQueue::try_new(cfg.queue_depth, cfg.max_io_bytes, cfg.block_size) {
        Ok(queue) => queue,
        Err(error) => {
            if let Err(destroy_error) = link.destroy_disk() {
                eprintln!("queue allocation failed and DESTROY failed: {error}; {destroy_error}");
                preserve_failed_safe("startup unwind could not destroy disk");
            }
            let mut message = error.to_string();
            if let Err(wipe_error) = backend.zero() {
                message.push_str(&format!("; VRAM wipe failed: {wipe_error}"));
            }
            return Err(error_after_release(
                &mut tenant,
                &mut stream,
                RuntimeErrorClass::Abi,
                5,
                message,
            ));
        }
    };
    let reg = q.registration(0);
    if let Err(e) = link.register_queue(&reg) {
        if let Err(destroy_error) = link.destroy_disk() {
            eprintln!("REGISTER failed and DESTROY failed: {e}; {destroy_error}");
            preserve_failed_safe("startup unwind could not destroy disk");
        }
        let mut message = e.to_string();
        if let Err(wipe_error) = backend.zero() {
            message.push_str(&format!("; VRAM wipe failed: {wipe_error}"));
        }
        return Err(error_after_release(
            &mut tenant,
            &mut stream,
            RuntimeErrorClass::Abi,
            5,
            message,
        ));
    }

    let mut dlink = DriverLink::from_queue(q);
    let startup_lun_deadline = Duration::from_secs(60);
    let startup_lun_started = Instant::now();
    let serial_arc = Arc::<str>::from(serial_str.as_str());
    loop {
        // The startup LUN identity wait must pump I/O: Windows disk
        // enumeration may issue READs before Get-Disk exposes the device.
        let want_serial = Arc::clone(&serial_arc);
        let want_size = cfg.size_bytes;
        let startup_lun = readonly_host_call_with_io_pump(
            &mut link,
            &mut dlink,
            &mut backend,
            STARTUP_LUN_PUMP_TIMEOUT,
            move || {
                WindowsHostState::find_lun(&want_serial, want_size, STARTUP_LUN_PROVIDER_TIMEOUT)
            },
        );
        let startup_lun_error = match startup_lun {
            Ok(Some(_)) => break,
            Ok(None) => "not enumerated".to_string(),
            Err(e) => e,
        };
        if startup_lun_started.elapsed() >= startup_lun_deadline {
            let mut message = format!(
                "startup LUN identity did not appear after {} ms: {}",
                startup_lun_deadline.as_millis(),
                startup_lun_error
            );
            if let Err(unreg_error) = link.unregister_queue() {
                message.push_str(&format!("; UNREGISTER failed: {unreg_error}"));
            }
            if let Err(destroy_error) = link.destroy_disk() {
                message.push_str(&format!("; DESTROY failed: {destroy_error}"));
                preserve_failed_safe("startup missing LUN could not destroy disk");
            }
            if let Err(wipe_error) = backend.zero() {
                message.push_str(&format!("; VRAM wipe failed: {wipe_error}"));
            }
            return Err(error_after_release(
                &mut tenant,
                &mut stream,
                RuntimeErrorClass::Abi,
                5,
                message,
            ));
        }
        thread::sleep(Duration::from_millis(500));
    }

    state.phase = RuntimePhase::QueueRegistered;
    state.phase = RuntimePhase::Online;
    row.begin_event("Online", utc_ms());
    sync_runtime_evidence(&mut row, &state);
    row.backend = "cuda".into();
    let _ = evidence.append(&row);

    eprintln!(
        "product Online: run_id={run_id} lease={} size={} serial={} cuda={}",
        lease.lease,
        cfg.size_bytes,
        serial_str,
        dev.name()
    );

    // --- I/O loop: one pending COMMIT at a time ---
    let mut last_progress = Instant::now();
    let mut broker_lost_online = false;
    let cuda_watchdog = CudaWatchdog::start(Duration::from_millis(5_000));
    let commit_watchdog = Duration::from_millis(5_000);

    loop {
        while !stop.load(Ordering::Acquire) {
            match link.commit_and_fetch(Duration::from_millis(500)) {
                Ok(()) => {
                    cuda_watchdog.begin();
                    let served = dlink.commit_and_fetch(&mut backend);
                    cuda_watchdog.end();
                    sync_io_evidence(&mut row, &dlink);
                    if cuda_watchdog.fired() {
                        state.healthy = false;
                        state.phase = RuntimePhase::FailedSafe;
                        sync_runtime_evidence(&mut row, &state);
                        let _ = evidence.append(&row);
                        preserve_failed_safe("CUDA operation exceeded 5,000 ms");
                    }
                    match served {
                        Ok(n) if n > 0 => last_progress = Instant::now(),
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("serve error: {e}");
                            state.healthy = false;
                            state.phase = RuntimePhase::FailedSafe;
                            sync_runtime_evidence(&mut row, &state);
                            let _ = evidence.append(&row);
                            preserve_failed_safe("I/O service failure");
                        }
                    }
                }
                Err(crate::windows_driver::IoctlError::Timeout) => {}
                Err(e) => {
                    eprintln!("commit_and_fetch: {e}");
                    if last_progress.elapsed() > commit_watchdog {
                        state.healthy = false;
                        state.phase = RuntimePhase::FailedSafe;
                        sync_runtime_evidence(&mut row, &state);
                        let _ = evidence.append(&row);
                        preserve_failed_safe("COMMIT watchdog expired");
                    }
                }
            }
            let broker_loss = match tenant.heartbeat_psi(&mut stream) {
                Err(error) => Some(error.to_string()),
                Ok(()) => stream.flush().err().map(|error| error.to_string()),
            };
            if let Some(e) = broker_loss {
                broker_lost_online = true;
                teardown_diag(&format!("broker_lost_online: {e}"));
                state.healthy = false;
                state.phase = RuntimePhase::FailedSafe;
                row.begin_event("broker_lost_online", utc_ms());
                row.error_class = Some("broker".into());
                row.error_code = Some("3".into());
                sync_runtime_evidence(&mut row, &state);
                let _ = evidence.append(&row);
                stop.store(true, Ordering::Release);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        state.phase = RuntimePhase::Stopping;
        let teardown_started = Instant::now();
        row.begin_event("Stopping", utc_ms());
        sync_runtime_evidence(&mut row, &state);
        let _ = evidence.append(&row);
        teardown_diag(&format!(
            "Stopping: letter={} serial={} size={}",
            cfg.volume_letter.to_ascii_uppercase(),
            serial_str,
            cfg.size_bytes
        ));
        // Do NOT cancel_fetch / stop serving yet. CreateFile(\\.\S:) / FSCTL_LOCK
        // deadlocks if the product I/O loop is idle: NTFS waits on the miniport
        // and the miniport waits on this process's COMMIT loop.

        let teardown_letter = cfg.volume_letter.to_ascii_uppercase();
        let mut gates = {
            let mut io = OnlineIo {
                link: &mut link,
                dlink: &mut dlink,
                backend: &mut backend,
            };
            let mut stop_ctx = StopContext {
                state: &mut state,
                row: &mut row,
                evidence: &mut evidence,
                stop: &stop,
                resume_notifier: resume_notifier.as_deref(),
            };
            let Some(gates) =
                prepare_teardown_gates(cfg, teardown_letter, &serial_str, &mut io, &mut stop_ctx)
            else {
                continue;
            };
            gates
        };

        // Volume lock while pumping driver I/O so CreateFile can complete.
        teardown_diag(&format!(
            "teardown phase=VolumeLock begin (I/O pump) letter={teardown_letter}"
        ));
        if gates.has_mounted_volume() {
            let mut io = OnlineIo {
                link: &mut link,
                dlink: &mut dlink,
                backend: &mut backend,
            };
            let mut stop_ctx = StopContext {
                state: &mut state,
                row: &mut row,
                evidence: &mut evidence,
                stop: &stop,
                resume_notifier: resume_notifier.as_deref(),
            };
            let mut broker = BrokerSession {
                tenant: &mut tenant,
                stream: &mut stream,
            };
            if !lock_mounted_volume_with_io(
                teardown_letter,
                &mut gates,
                teardown_started,
                &mut stop_ctx,
                &mut io,
                &mut broker,
            ) {
                continue;
            }
        } else {
            teardown_diag("teardown phase=VolumeLock N/A (exact RAW disk)");
        }

        let gate_b_ok = {
            let mut io = OnlineIo {
                link: &mut link,
                dlink: &mut dlink,
                backend: &mut backend,
            };
            let mut stop_ctx = StopContext {
                state: &mut state,
                row: &mut row,
                evidence: &mut evidence,
                stop: &stop,
                resume_notifier: resume_notifier.as_deref(),
            };
            run_gate_b_and_teardown(
                cfg,
                teardown_letter,
                teardown_started,
                (lease.lease, lease.bytes),
                &mut gates,
                &mut stop_ctx,
                &mut io,
            )
        };
        if !gate_b_ok {
            continue;
        }
        let mut stop_ctx = StopContext {
            state: &mut state,
            row: &mut row,
            evidence: &mut evidence,
            stop: &stop,
            resume_notifier: resume_notifier.as_deref(),
        };
        let mut broker = BrokerSession {
            tenant: &mut tenant,
            stream: &mut stream,
        };
        return finalize_released_teardown(
            backend,
            &ctx,
            free,
            broker_lost_online,
            teardown_started,
            &mut stop_ctx,
            &mut broker,
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::teardown::resume_online_stop;
    use super::*;

    #[test]
    fn serial_is_16_hex() {
        let s = serial_from_run_id("run-1");
        assert_eq!(s.len(), 16);
        assert!(s.iter().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn scm_refusal_ack_schedules_same_stop_retry() {
        let stop = Arc::new(AtomicBool::new(true));
        let notifier = Arc::new(AtomicBool::new(false));
        let notifier_for_monitor = Arc::clone(&notifier);
        let stop_for_monitor = Arc::clone(&stop);
        let monitor = thread::spawn(move || {
            while !notifier_for_monitor.load(Ordering::Acquire) {
                thread::yield_now();
            }
            stop_for_monitor.store(false, Ordering::Release);
            notifier_for_monitor.store(false, Ordering::Release);
            thread::sleep(Duration::from_millis(10));
            stop_for_monitor.store(true, Ordering::Release);
        });

        resume_online_stop(&stop, Some(&notifier));
        monitor.join().unwrap();

        assert!(!notifier.load(Ordering::Acquire));
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn console_resume_clears_stop_without_scm_notifier() {
        let stop = AtomicBool::new(true);
        resume_online_stop(&stop, None);
        assert!(!stop.load(Ordering::Acquire));
    }
}
