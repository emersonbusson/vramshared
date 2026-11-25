// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/ublk_runtime.rs
//! Standalone ublk lifecycle coordination and production kernel adapter.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ramshared_wsl2d::swap::{SwapoffOutcome, spawn_swapoff};
use ramshared_wsl2d::{RamBackend, ResidencyConfig, ublk, ublk_control, ublk_server};

use super::args::BackendKind;
use super::platform_admission::guard_not_wsl2;
use super::shutdown;
use super::shutdown::ShutdownPermit;
use super::shutdown_signals::{SHUTDOWN, install_shutdown_signals};
use super::{
    BLOCK_SIZE, ExactSwapState, SECTOR, UBLK_CONTROL, canonical_ublk_identity, lock_memory,
    strict_exact_swap_state_from_text,
};

/// Unifies the two types of DT-3 server handles (VRAM-with-residency or pure RAM) for
/// a unified teardown in `run_ublk`.
pub(super) enum UblkHandle {
    Vram(ublk_server::ServerHandleDt3VramResidency),
    Ram(ublk_server::ServerHandleDt3<RamBackend>),
}

impl UblkHandle {
    pub(super) fn join(self) -> std::io::Result<()> {
        match self {
            UblkHandle::Vram(h) => h.join(),
            UblkHandle::Ram(h) => h.join().map(|_| ()),
        }
    }
}

/// A created ublk device identity. Keeping only these stable values at the
/// daemon boundary prevents the lifecycle core from touching a kernel handle.
#[derive(Clone, Copy)]
pub(super) struct UblkDevice {
    pub(super) id: u32,
    pub(super) queue_depth: u16,
}

pub(super) trait UblkServer {
    fn authorize_shutdown(&mut self, permit: &ShutdownPermit) -> std::io::Result<()>;
    fn join(self: Box<Self>) -> std::io::Result<()>;
}

struct ProductionUblkServer(UblkHandle);

impl UblkServer for ProductionUblkServer {
    fn authorize_shutdown(&mut self, permit: &ShutdownPermit) -> std::io::Result<()> {
        let lease = permit.shared_lease().ok_or_else(|| {
            std::io::Error::other("production ublk shutdown requires the release-owner lease")
        })?;
        match &self.0 {
            UblkHandle::Vram(server) => server.authorize_shutdown(lease),
            UblkHandle::Ram(_) => Ok(()),
        }
    }

    fn join(self: Box<Self>) -> std::io::Result<()> {
        self.0.join()
    }
}

/// Isolates kernel ublk control operations, signal waiting, and backend server
/// startup from the standalone ublk lifecycle state machine.
pub(super) trait UblkRuntime {
    fn guard_not_wsl2(&mut self) -> Result<(), Box<dyn std::error::Error>>;

    fn lock_memory(
        &mut self,
        force: bool,
        lock_future: bool,
    ) -> Result<(), Box<dyn std::error::Error>>;

    fn install_shutdown_handler(&mut self) -> Result<(), Box<dyn std::error::Error>>;

    fn add_device(&mut self, queue_depth: u16) -> Result<UblkDevice, Box<dyn std::error::Error>>;

    fn set_params(
        &mut self,
        device: UblkDevice,
        sectors: u64,
    ) -> Result<(), Box<dyn std::error::Error>>;

    fn start_server(
        &mut self,
        backend: BackendKind,
        char_path: &str,
        block_path: &str,
        queue_depth: u16,
        size: u64,
    ) -> Result<Box<dyn UblkServer>, Box<dyn std::error::Error>>;

    fn start_device(&mut self, device: UblkDevice) -> Result<(), Box<dyn std::error::Error>>;

    fn wait_for_shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>>;

    fn swap_state(
        &mut self,
        block_path: &str,
    ) -> Result<ExactSwapState, Box<dyn std::error::Error>>;

    fn spawn_swapoff(&mut self, block_path: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome>;

    /// Exclusive shutdown custody, held through explicit server shutdown.
    fn try_claim_shutdown(&mut self, block_path: &str) -> Option<ShutdownPermit>;

    fn wait_for_release_progress(&mut self);

    fn stop_device(&mut self, device: UblkDevice) -> Result<(), Box<dyn std::error::Error>>;

    fn delete_device(&mut self, device: UblkDevice) -> Result<(), Box<dyn std::error::Error>>;
}

pub(super) struct ProductionUblkRuntime {
    control_path: String,
    swaps_path: String,
    shutdown_flag: &'static AtomicBool,
    shutdown_poll_interval: Duration,
    release_poll_interval: Duration,
    install_signals: fn() -> Result<(), Box<dyn std::error::Error>>,
}

impl Default for ProductionUblkRuntime {
    fn default() -> Self {
        Self::production()
    }
}

impl ProductionUblkRuntime {
    pub(super) fn production() -> Self {
        Self {
            control_path: UBLK_CONTROL.to_string(),
            swaps_path: "/proc/swaps".to_string(),
            shutdown_flag: &SHUTDOWN,
            shutdown_poll_interval: Duration::from_millis(200),
            release_poll_interval: Duration::from_millis(200),
            install_signals: install_shutdown_signals,
        }
    }
}

impl UblkRuntime for ProductionUblkRuntime {
    fn guard_not_wsl2(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        guard_not_wsl2()
    }

    fn lock_memory(
        &mut self,
        force: bool,
        lock_future: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        lock_memory(force, lock_future)
    }

    fn install_shutdown_handler(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        (self.install_signals)()
    }

    fn add_device(&mut self, queue_depth: u16) -> Result<UblkDevice, Box<dyn std::error::Error>> {
        let mut spec = ublk_control::DeviceSpec::smoke_auto();
        spec.queue_depth = queue_depth;
        let report = ublk_control::add_device(&self.control_path, spec)?;
        Ok(UblkDevice {
            id: report.dev_id,
            queue_depth: report.queue_depth,
        })
    }

    fn set_params(
        &mut self,
        device: UblkDevice,
        sectors: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        ublk_control::set_params(
            &self.control_path,
            device.id,
            ublk::Params::basic_disk(sectors, 12, 12),
        )?;
        Ok(())
    }

    fn start_server(
        &mut self,
        backend: BackendKind,
        char_path: &str,
        block_path: &str,
        queue_depth: u16,
        size: u64,
    ) -> Result<Box<dyn UblkServer>, Box<dyn std::error::Error>> {
        let handle = match backend {
            BackendKind::Vram | BackendKind::Auto => {
                match ublk_server::spawn_server_dt3_vram_with_residency(
                    char_path,
                    queue_depth,
                    BLOCK_SIZE as usize,
                    size as usize,
                    BLOCK_SIZE,
                    block_path.to_string(),
                    ResidencyConfig::default(),
                ) {
                    Ok(h) => UblkHandle::Vram(h),
                    Err(e) => {
                        eprintln!(
                            "[ramsharedd] ublk VRAM server initialization failed ({e}); falling back natively to RAM backend"
                        );
                        UblkHandle::Ram(ublk_server::spawn_server_dt3(
                            char_path,
                            queue_depth,
                            BLOCK_SIZE as usize,
                            RamBackend::new(size as usize),
                        )?)
                    }
                }
            }
            BackendKind::Ram => UblkHandle::Ram(ublk_server::spawn_server_dt3(
                char_path,
                queue_depth,
                BLOCK_SIZE as usize,
                RamBackend::new(size as usize),
            )?),
            BackendKind::Vulkan => {
                return Err("ublk with --backend vulkan not supported (DT-11)".into());
            }
        };
        Ok(Box::new(ProductionUblkServer(handle)))
    }

    fn start_device(&mut self, device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
        ublk_control::start_dev(&self.control_path, device.id, std::process::id())?;
        Ok(())
    }

    fn wait_for_shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        while !self.shutdown_flag.load(Ordering::SeqCst) {
            std::thread::sleep(self.shutdown_poll_interval);
        }
        Ok(())
    }

    fn swap_state(
        &mut self,
        block_path: &str,
    ) -> Result<ExactSwapState, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(&self.swaps_path)?;
        Ok(strict_exact_swap_state_from_text(
            &text,
            block_path,
            canonical_ublk_identity,
        )?)
    }

    fn spawn_swapoff(&mut self, block_path: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        spawn_swapoff(block_path)
    }

    fn try_claim_shutdown(&mut self, block_path: &str) -> Option<ShutdownPermit> {
        ShutdownPermit::claim(block_path)
    }

    fn wait_for_release_progress(&mut self) {
        std::thread::sleep(self.release_poll_interval);
    }

    fn stop_device(&mut self, device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
        ublk_control::stop_dev(&self.control_path, device.id)?;
        Ok(())
    }

    fn delete_device(&mut self, device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
        ublk_control::delete_device(&self.control_path, device.id)?;
        Ok(())
    }
}

/// ublk path: serves `/dev/ublkbN` directly (io_uring), without socket. The DT-3 worker is the
/// owner of the VRAM/CUDA context and runs the residency (canary §9/§9.4); DEMOTE performs
/// swapoff of the served device itself. The lifecycle goes until SIGINT/SIGTERM.
/// SPEC: docs/the architecture design F2.
pub(super) fn run_ublk(
    size: u64,
    force: bool,
    queue_depth: u16,
    backend: BackendKind,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = ProductionUblkRuntime::production();
    run_ublk_with_runtime(size, force, queue_depth, backend, &mut runtime)
}

/// Ordered ublk lifecycle with explicit rollback after every post-create
/// failure. The test runtime is pure in-memory; only ProductionUblkRuntime can
/// touch a kernel device.
pub(super) fn run_ublk_with_runtime(
    size: u64,
    force: bool,
    queue_depth: u16,
    backend: BackendKind,
    runtime: &mut dyn UblkRuntime,
) -> Result<(), Box<dyn std::error::Error>> {
    // DT-11: refusal before guard, memory lock, or device creation.
    if let BackendKind::Vulkan = backend {
        return Err("ublk with --backend vulkan not supported (DT-11: ublk \
             residency server is CUDA-fixed). Use --backend vram (CUDA), or Vulkan \
             via --slices (broker) / --transport nbd."
            .into());
    }
    runtime.guard_not_wsl2()?;
    // MCL_CURRENT only: MCL_FUTURE races dxgkrnl mapping and can hang the host.
    runtime.lock_memory(force, false)?;
    runtime.install_shutdown_handler()?;

    let device = runtime.add_device(queue_depth)?;
    let block_path = format!("/dev/ublkb{}", device.id);
    let sectors = size / SECTOR;
    if let Err(error) = runtime.set_params(device, sectors) {
        let _permit = runtime
            .try_claim_shutdown(&block_path)
            .ok_or("unconfirmed release custody; ublk device is preserved")?;
        prove_ublk_swap_absent(runtime, &block_path)?;
        runtime.delete_device(device)?;
        return Err(error);
    }
    let char_path = format!("/dev/ublkc{}", device.id);
    let server =
        match runtime.start_server(backend, &char_path, &block_path, device.queue_depth, size) {
            Ok(server) => server,
            Err(error) => {
                let _permit = runtime
                    .try_claim_shutdown(&block_path)
                    .ok_or("unconfirmed release custody; ublk device is preserved")?;
                prove_ublk_swap_absent(runtime, &block_path)?;
                runtime.delete_device(device)?;
                return Err(error);
            }
        };
    let lifecycle_error = match runtime.start_device(device) {
        Err(error) => Some(error),
        Ok(()) => {
            eprintln!(
                "[ramsharedd] ublk device: {block_path} ({} MiB, qd={}, backend={})",
                size >> 20,
                device.queue_depth,
                backend.label()
            );
            eprintln!("[ramsharedd] swapon: sudo swapon {block_path}");
            eprintln!("[ramsharedd] Ctrl-C / SIGTERM to exit");
            runtime.wait_for_shutdown().err()
        }
    };
    // Returning would terminate the process that owns the server. Keep it
    // alive through refusals and pending helpers, including startup ambiguity.
    shutdown::stop_ublk(runtime, device, &block_path, server)?;
    eprintln!("[ramsharedd] ublk device removed");
    lifecycle_error.map_or(Ok(()), Err)
}

pub(super) fn prove_ublk_swap_absent(
    runtime: &mut dyn UblkRuntime,
    block_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match runtime.swap_state(block_path)? {
        ExactSwapState::Absent => Ok(()),
        ExactSwapState::Active { used_kb } => Err(format!(
            "refusing ublk stop/delete: {block_path} remains active swap (used_kb={used_kb}); device and backend preserved"
        )
        .into()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn failing_signal_installer() -> Result<(), Box<dyn std::error::Error>> {
        Err("injected ublk SIGTERM failure".into())
    }

    #[test]
    fn ublk_signal_registration_failure_refuses_before_device_creation() {
        struct ForbiddenDeviceRuntime;
        impl UblkRuntime for ForbiddenDeviceRuntime {
            fn guard_not_wsl2(&mut self) -> Result<(), Box<dyn std::error::Error>> {
                Ok(())
            }
            fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
                Ok(())
            }
            fn install_shutdown_handler(&mut self) -> Result<(), Box<dyn std::error::Error>> {
                failing_signal_installer()
            }
            fn add_device(&mut self, _: u16) -> Result<UblkDevice, Box<dyn std::error::Error>> {
                panic!("add_device must not run after signal installation failure")
            }
            fn set_params(
                &mut self,
                _: UblkDevice,
                _: u64,
            ) -> Result<(), Box<dyn std::error::Error>> {
                panic!("set_params must not run")
            }
            fn start_server(
                &mut self,
                _: BackendKind,
                _: &str,
                _: &str,
                _: u16,
                _: u64,
            ) -> Result<Box<dyn UblkServer>, Box<dyn std::error::Error>> {
                panic!("start_server must not run")
            }
            fn start_device(&mut self, _: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
                panic!("start_device must not run")
            }
            fn wait_for_shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
                panic!("wait_for_shutdown must not run")
            }
            fn swap_state(
                &mut self,
                _: &str,
            ) -> Result<ExactSwapState, Box<dyn std::error::Error>> {
                panic!("swap_state must not run")
            }
            fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
                panic!("spawn_swapoff must not run")
            }
            fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
                panic!("try_claim_shutdown must not run")
            }
            fn wait_for_release_progress(&mut self) {}
            fn stop_device(&mut self, _: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
                panic!("stop_device must not run")
            }
            fn delete_device(&mut self, _: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
                panic!("delete_device must not run")
            }
        }

        let mut rt = ForbiddenDeviceRuntime;
        let err = run_ublk_with_runtime(4096, true, 1, BackendKind::Ram, &mut rt)
            .expect_err("signal error must fail before device creation");
        assert!(err.to_string().contains("SIGTERM failure"));
    }

    #[test]
    fn production_ublk_runtime_adapters_fail_closed_without_kernel_devices() {
        static TEST_UBLK_SHUTDOWN: AtomicBool = AtomicBool::new(true);
        let root = std::env::temp_dir().join(format!(
            "ramshared-ublk-adapter-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temp test dir");
        let swaps_path = root.join("swaps");
        std::fs::write(
            &swaps_path,
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/dev/ublkb7\t\t\t\tpartition\t\t65536\t\t512\t\t10\n",
        )
        .expect("write synthetic swaps");

        let mut rt = ProductionUblkRuntime {
            control_path: root
                .join("missing-ublk-control")
                .to_string_lossy()
                .into_owned(),
            swaps_path: swaps_path.to_string_lossy().into_owned(),
            shutdown_flag: &TEST_UBLK_SHUTDOWN,
            shutdown_poll_interval: Duration::ZERO,
            release_poll_interval: Duration::ZERO,
            install_signals: failing_signal_installer,
        };

        let _ = rt.guard_not_wsl2();
        rt.lock_memory(true, false).expect("force lock ok");
        assert!(rt.install_shutdown_handler().is_err());

        let dev = UblkDevice {
            id: 7,
            queue_depth: 1,
        };
        assert!(rt.add_device(1).is_err());
        assert!(rt.set_params(dev, 8).is_err());
        assert!(rt.start_device(dev).is_err());
        assert!(rt.stop_device(dev).is_err());
        assert!(rt.delete_device(dev).is_err());
        assert!(
            rt.start_server(BackendKind::Vulkan, "/dev/null", "/dev/ublkb7", 1, 4096)
                .is_err()
        );
        assert!(
            rt.start_server(
                BackendKind::Ram,
                root.join("missing-ublkc7").to_string_lossy().as_ref(),
                "/dev/ublkb7",
                1,
                4096,
            )
            .is_err()
        );
        assert!(
            rt.start_server(
                BackendKind::Vram,
                root.join("missing-ublkc7").to_string_lossy().as_ref(),
                "/dev/ublkb7",
                1,
                4096,
            )
            .is_err()
        );
        rt.wait_for_shutdown()
            .expect("set shutdown flag finishes immediately");

        static DELAYED_SHUTDOWN: AtomicBool = AtomicBool::new(false);
        DELAYED_SHUTDOWN.store(false, Ordering::SeqCst);
        let waiter_control = root
            .join("missing-ublk-control")
            .to_string_lossy()
            .into_owned();
        let waiter_swaps = swaps_path.to_string_lossy().into_owned();
        let handle = std::thread::spawn(move || {
            let mut delayed_rt = ProductionUblkRuntime {
                control_path: waiter_control,
                swaps_path: waiter_swaps,
                shutdown_flag: &DELAYED_SHUTDOWN,
                shutdown_poll_interval: Duration::from_millis(1),
                release_poll_interval: Duration::ZERO,
                install_signals: failing_signal_installer,
            };
            delayed_rt.wait_for_shutdown().is_ok()
        });
        std::thread::sleep(Duration::from_millis(5));
        DELAYED_SHUTDOWN.store(true, Ordering::SeqCst);
        assert!(handle.join().expect("delayed shutdown waiter finishes"));

        rt.wait_for_release_progress();
        assert_eq!(
            rt.swap_state("/dev/ublkb7")
                .expect("parse active ublk swap"),
            ExactSwapState::Active { used_kb: 512 }
        );
        let swapoff_rx =
            rt.spawn_swapoff(root.join("nonexistent-ublkb7").to_string_lossy().as_ref());
        assert!(!matches!(
            swapoff_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("spawn_swapoff worker completes"),
            SwapoffOutcome::Drained
        ));
        assert!(
            prove_ublk_swap_absent(&mut rt, "/dev/ublkb7")
                .expect_err("active ublk swap must refuse delete")
                .to_string()
                .contains("active swap")
        );
        std::fs::write(&swaps_path, "not-a-valid-swaps-header\n").expect("write malformed swaps");
        assert!(rt.swap_state("/dev/ublkb7").is_err());
        std::fs::write(
            &swaps_path,
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n",
        )
        .expect("write empty swaps");
        assert_eq!(
            rt.swap_state("/dev/ublkb7")
                .expect("parse absent ublk swap"),
            ExactSwapState::Absent
        );
        prove_ublk_swap_absent(&mut rt, "/dev/ublkb7").expect("absent swap allows delete");
        let _ = rt.try_claim_shutdown("/dev/ublkb7");
        let _ = std::fs::remove_dir_all(&root);
    }
}
