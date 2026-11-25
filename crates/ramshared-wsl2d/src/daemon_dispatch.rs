// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/daemon_dispatch.rs
//! Daemon action execution dispatch and backend window zeroing helper.

use std::path::PathBuf;

use ramshared_block::BlockBackend;

use crate::args::{AppArgs, BackendKind};
use crate::broker_runtime::run_broker_ram;
use crate::native_origin;
use crate::nbd_origin::{self, run_nbd};
use crate::origin_runtime::{self, OpenedOrigin};
use crate::platform_admission::DaemonAction;
use crate::spill_pump;
use crate::ublk_runtime::run_ublk;

/// Zeroes the `[base, base+len)` window of the backend in 1 MiB chunks (slice hygiene, DT-17).
/// Runs on the thread owning the backend (single CUDA worker) — `WMsg::ZeroExport`.
pub(crate) fn zero_window<B: BlockBackend>(
    backend: &mut B,
    base: u64,
    len: u64,
) -> Result<(), ramshared_block::IoError> {
    const CHUNK: usize = 1 << 20;
    let buf = vec![0u8; CHUNK.min(len as usize)];
    let mut off = 0u64;
    while off < len {
        let n = ((len - off) as usize).min(buf.len());
        backend.write_at(base + off, &buf[..n])?;
        off += n as u64;
    }
    Ok(())
}

/// The production shell is deliberately behind this small interface so the
/// safety-critical argv/plan boundary can be tested with a recording runner.
pub(crate) trait DaemonActionRunner {
    fn execute(&mut self, action: DaemonAction) -> Result<(), Box<dyn std::error::Error>>;
}

pub(crate) fn arm_spill_pump_at(device: &str, health_path: PathBuf) {
    let config = spill_pump::PumpConfig::new(device);
    // DT-10: the file is the only pump-status surface. The
    // hook is detached with the pump thread and never joined.
    spill_pump::spawn(config, spill_pump::PumpPolicy::default(), move |health| {
        if let Err(error) = spill_pump::write_health(&health_path, health) {
            eprintln!("[ramsharedd] pump-health write: {error}");
        }
    });
    eprintln!(
        "[ramsharedd] spill pump: armed for the tier {device} (budget={} pages, idle={}s)",
        spill_pump::DEFAULT_BUDGET_PAGES,
        spill_pump::DEFAULT_IDLE_SECONDS
    );
}

pub(crate) fn execute_broker_action_with(
    args: AppArgs,
    run_broker_fn: impl FnOnce(
        u64,
        u16,
        String,
        Option<std::net::SocketAddr>,
        Option<(String, u16)>,
        std::net::SocketAddr,
        Option<PathBuf>,
    ) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let AppArgs {
        backend,
        slices,
        slice_bytes,
        sock,
        listen_nbd_addr,
        arbiter_addr,
        advertise_tcp,
        telemetry_jsonl,
        ..
    } = args;
    let arbiter_addr =
        arbiter_addr.ok_or("--slices requires --arbiter-listen IP:PORT (broker control point)")?;
    if backend != BackendKind::Ram {
        return Err(
            "GPU-backed --slices is unsupported because its driver calls are synchronous; use the single NBD --origin-manifest path for isolated, revocable GPU caching".into(),
        );
    }
    run_broker_fn(
        slice_bytes,
        slices,
        sock,
        listen_nbd_addr,
        advertise_tcp,
        arbiter_addr,
        telemetry_jsonl,
    )
}

pub(crate) fn execute_nbd_action_with(
    args: AppArgs,
    open_origin_fn: impl FnOnce(
        Option<&str>,
        Option<&native_origin::NativeOriginRequest>,
        u64,
    ) -> Result<Option<(OpenedOrigin, u64)>, Box<dyn std::error::Error>>,
    arm_pump_fn: impl FnOnce(&str),
    run_nbd_fn: impl FnOnce(
        Option<OpenedOrigin>,
        u64,
        String,
        bool,
        String,
        usize,
    ) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let AppArgs {
        backend,
        mut size,
        origin,
        sock,
        force,
        nbd_dev,
        serve_threads,
        zram_pump_tier,
        native_origin,
        ..
    } = args;
    let validated_origin = open_origin_fn(origin.as_deref(), native_origin.as_ref(), size)?;
    let validated_origin = validated_origin
        .map(|(origin, logical_bytes)| {
            size = logical_bytes;
            origin
        })
        .ok_or(nbd_origin::PRODUCT_ORIGIN_REQUIRED)?;
    if backend == BackendKind::Ram {
        return Err("--backend ram has no single NBD path; use --slices (broker) or ublk".into());
    }
    // The spill pump (SPEC zram-writeback-vram-tier DT-8) owns
    // the kernel-side writeback of the tier's cold pages. Its
    // loss is safe: the zram stays full and the overflow
    // reaches the fallback.
    if let Some(device) = zram_pump_tier.as_deref() {
        arm_pump_fn(device);
    }
    run_nbd_fn(
        Some(validated_origin),
        size,
        sock,
        force,
        nbd_dev,
        serve_threads,
    )
}

pub(crate) struct ProductionDaemonRunner;

impl DaemonActionRunner for ProductionDaemonRunner {
    fn execute(&mut self, action: DaemonAction) -> Result<(), Box<dyn std::error::Error>> {
        match action {
            DaemonAction::Broker(args) => execute_broker_action_with(args, run_broker_ram),
            DaemonAction::Nbd(args) => execute_nbd_action_with(
                args,
                origin_runtime::open_configured,
                |device| arm_spill_pump_at(device, PathBuf::from(spill_pump::PUMP_HEALTH_PATH)),
                run_nbd,
            ),
            DaemonAction::Ublk(args) => {
                run_ublk(args.size, args.force, args.queue_depth, args.backend)
            }
        }
    }
}
