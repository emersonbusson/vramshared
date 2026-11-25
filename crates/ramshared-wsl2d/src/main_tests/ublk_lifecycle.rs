// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/ublk_lifecycle.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn daemon_version_flag_is_side_effect_free() {
    assert!(daemon_version_requested(&[
        "ramsharedd".to_string(),
        "--version".to_string()
    ]));
    assert!(!daemon_version_requested(&[
        "ramsharedd".to_string(),
        "--size".to_string(),
        "1024".to_string()
    ]));
}

#[test]
fn daemon_broker_setup_failure_zeroes_allocated_vram_before_return() {
    let zeroed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let result = run_broker_with_setup(
        ZeroRecordingProvider {
            zeroed: std::sync::Arc::clone(&zeroed),
        },
        4096,
        1,
        std::env::temp_dir()
            .join(format!(
                "ramshared-daemon-setup-refusal-{}.sock",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned(),
        false,
        |_force, _future| Ok(()),
        || Err("injected broker setup refusal".into()),
        Duration::from_millis(1),
    );

    assert!(
        result.is_err(),
        "injected broker setup refusal must propagate"
    );
    assert_eq!(
        *zeroed.lock().expect("test zero record lock"),
        vec![4096, 4096, CANARY_BYTES],
        "initialization, backend cleanup, then canary cleanup must all zero before return"
    );
}

#[test]
fn daemon_broker_lock_refusal_zeroes_allocated_vram_before_return() {
    let zeroed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let setup_called = std::sync::Arc::new(AtomicBool::new(false));
    let setup_called_by_closure = std::sync::Arc::clone(&setup_called);
    let result = run_broker_with_setup(
        ZeroRecordingProvider {
            zeroed: std::sync::Arc::clone(&zeroed),
        },
        4096,
        1,
        std::env::temp_dir()
            .join(format!(
                "ramshared-daemon-lock-refusal-{}.sock",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned(),
        false,
        |_force, _future| Err("injected memory-lock refusal".into()),
        move || {
            setup_called_by_closure.store(true, Ordering::SeqCst);
            Err("broker setup must not run after memory-lock refusal".into())
        },
        Duration::from_millis(1),
    );

    assert!(
        result.is_err(),
        "injected memory-lock refusal must propagate"
    );
    assert!(
        !setup_called.load(Ordering::SeqCst),
        "memory-lock refusal must stop before canary allocation or broker setup"
    );
    assert_eq!(
        *zeroed.lock().expect("test zero record lock"),
        vec![4096, 4096],
        "initialization and refusal cleanup must both wipe the allocated backend"
    );
}

#[test]
fn daemon_broker_canary_allocation_failure_zeroes_main_allocation() {
    struct FailCanaryProvider {
        calls: std::sync::atomic::AtomicUsize,
        zeroed: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
    }

    impl VramProvider for FailCanaryProvider {
        type Mem<'a>
            = ZeroRecordingMemory
        where
            Self: 'a;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                return Err(ramshared_vram::VramError::OutOfMemory);
            }
            Ok(ZeroRecordingMemory {
                bytes,
                zeroed: std::sync::Arc::clone(&self.zeroed),
            })
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((8 * GIB, 8 * GIB))
        }
    }

    let zeroed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let result = run_broker_with_setup(
        FailCanaryProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            zeroed: std::sync::Arc::clone(&zeroed),
        },
        4096,
        1,
        std::env::temp_dir()
            .join(format!(
                "ramshared-daemon-canary-refusal-{}.sock",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned(),
        false,
        |_force, _future| Ok(()),
        || panic!("broker setup must not run when canary allocation fails"),
        Duration::from_millis(1),
    );

    assert!(result.is_err(), "canary allocation failure must propagate");
    assert_eq!(
        *zeroed.lock().expect("test zero record lock"),
        vec![4096, 4096],
        "the main allocation must be wiped after canary allocation fails"
    );
}

#[test]
fn gpu_base_mapping_precedes_current_only_lock_and_future_lock_is_refused() {
    struct OrderProvider {
        calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        zeroed: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
    }

    impl VramProvider for OrderProvider {
        type Mem<'a>
            = ZeroRecordingMemory
        where
            Self: 'a;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            self.calls.lock().expect("order log").push("gpu-map");
            Ok(ZeroRecordingMemory {
                bytes,
                zeroed: std::sync::Arc::clone(&self.zeroed),
            })
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((8 * GIB, 8 * GIB))
        }
    }

    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let lock_calls = std::sync::Arc::clone(&calls);
    let result = run_broker_with_setup(
        OrderProvider {
            calls: std::sync::Arc::clone(&calls),
            zeroed: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        },
        4096,
        1,
        std::env::temp_dir()
            .join(format!(
                "ramshared-daemon-lock-order-{}.sock",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned(),
        false,
        move |_force, lock_future| {
            assert!(!lock_future, "MCL_FUTURE must never be requested");
            lock_calls.lock().expect("order log").push("mlock-current");
            Err("stop after the lock-order frontier".into())
        },
        || panic!("setup must not run after the injected lock refusal"),
        Duration::from_millis(1),
    );

    assert!(result.is_err());
    assert_eq!(
        *calls.lock().expect("order log"),
        vec!["gpu-map", "mlock-current"]
    );
}

#[test]
fn daemon_ublk_runtime_orders_lifecycle_and_rolls_back_without_device() {
    #[derive(Default)]
    struct FakeServer {
        joined: bool,
    }

    impl UblkServer for FakeServer {
        fn authorize_shutdown(&mut self, _: &ShutdownPermit) -> std::io::Result<()> {
            Ok(())
        }

        fn join(mut self: Box<Self>) -> std::io::Result<()> {
            self.joined = true;
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeRuntime {
        calls: Vec<&'static str>,
    }

    impl UblkRuntime for FakeRuntime {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            Some(ShutdownPermit::injected(()))
        }

        fn wait_for_release_progress(&mut self) {}

        fn guard_not_wsl2(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.calls.push("guard");
            Ok(())
        }

        fn lock_memory(
            &mut self,
            _force: bool,
            lock_future: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            assert!(!lock_future, "ublk keeps MCL_FUTURE disabled");
            self.calls.push("lock");
            Ok(())
        }

        fn install_shutdown_handler(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.calls.push("signal");
            Ok(())
        }

        fn add_device(
            &mut self,
            queue_depth: u16,
        ) -> Result<UblkDevice, Box<dyn std::error::Error>> {
            assert_eq!(queue_depth, 3);
            self.calls.push("add");
            Ok(UblkDevice {
                id: 41,
                queue_depth,
            })
        }

        fn set_params(
            &mut self,
            device: UblkDevice,
            sectors: u64,
        ) -> Result<(), Box<dyn std::error::Error>> {
            assert_eq!(device.id, 41);
            assert_eq!(sectors, 16);
            self.calls.push("params");
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
            assert!(matches!(backend, BackendKind::Ram));
            assert_eq!(char_path, "/dev/ublkc41");
            assert_eq!(block_path, "/dev/ublkb41");
            assert_eq!(queue_depth, 3);
            assert_eq!(size, 8192);
            self.calls.push("server");
            Ok(Box::<FakeServer>::default())
        }

        fn start_device(&mut self, device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            assert_eq!(device.id, 41);
            self.calls.push("start");
            Ok(())
        }

        fn wait_for_shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.calls.push("wait");
            Ok(())
        }

        fn swap_state(
            &mut self,
            block_path: &str,
        ) -> Result<ExactSwapState, Box<dyn std::error::Error>> {
            assert_eq!(block_path, "/dev/ublkb41");
            self.calls.push("swap-state");
            Ok(ExactSwapState::Absent)
        }

        fn spawn_swapoff(
            &mut self,
            _block_path: &str,
        ) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            panic!("swapoff must not run when strict snapshots prove absence")
        }

        fn stop_device(&mut self, device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            assert_eq!(device.id, 41);
            self.calls.push("stop");
            Ok(())
        }

        fn delete_device(&mut self, device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            assert_eq!(device.id, 41);
            self.calls.push("delete");
            Ok(())
        }
    }

    let mut runtime = FakeRuntime::default();
    run_ublk_with_runtime(8192, false, 3, BackendKind::Ram, &mut runtime)
        .expect("injected RAM ublk lifecycle");
    assert_eq!(
        runtime.calls,
        vec![
            "guard",
            "lock",
            "signal",
            "add",
            "params",
            "server",
            "start",
            "wait",
            "swap-state",
            "swap-state",
            "stop",
            "swap-state",
            "delete",
        ]
    );

    let mut refusal_runtime = FakeRuntime::default();
    assert!(
        run_ublk_with_runtime(8192, false, 3, BackendKind::Vulkan, &mut refusal_runtime).is_err()
    );
    assert!(
        refusal_runtime.calls.is_empty(),
        "Vulkan refusal must occur before a ublk runtime operation"
    );
}

#[test]
fn production_ublk_adapter_refuses_vulkan_and_regular_files_without_device_access() {
    let mut runtime = ProductionUblkRuntime::default();
    let vulkan = runtime.start_server(
        BackendKind::Vulkan,
        "/dev/ublkc-ci-fixture",
        "/dev/ublkb-ci-fixture",
        1,
        4096,
    );
    assert!(
        vulkan.is_err(),
        "Vulkan must be refused before device access"
    );

    let fixture = std::env::temp_dir().join(format!(
        "ramshared-ublk-regular-file-{}-{}",
        std::process::id(),
        unix_time_ms().unwrap_or_default()
    ));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&fixture)
        .expect("create inert regular-file adapter fixture");
    drop(file);
    let ram = runtime.start_server(
        BackendKind::Ram,
        fixture.to_str().unwrap(),
        "/dev/ublkb-ci-fixture",
        1,
        4096,
    );
    assert!(
        ram.is_err(),
        "regular files must not stand in for ublk devices"
    );
    std::fs::remove_file(fixture).expect("remove exact regular-file fixture");

    assert_eq!(
        runtime
            .swap_state("/dev/ublkb999999")
            .expect("strict read-only /proc snapshot"),
        ExactSwapState::Absent
    );
}

#[test]
fn production_nbd_runtime_read_only_probes_do_not_activate_swap() {
    let mut runtime = ProductionNbdRuntimeStarter;
    assert!(
        runtime
            .startup_budget(false)
            .expect("disabled budget probe must not open DXG")
            .is_none()
    );

    let absent = "/dev/nbd99999999";
    assert_eq!(runtime.nbd_used_kb(absent), 0);
    assert!(runtime.nbd_swap_is_explicitly_absent(absent));
    assert!(runtime.elapsed_us(Instant::now()) < 1_000_000);
}

#[test]
fn daemon_ublk_runtime_failures_delete_only_after_fresh_absence_proof() {
    #[derive(Clone, Copy)]
    enum Failure {
        Params,
        Server,
        Start,
        Wait,
    }

    struct Server(std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>);

    impl UblkServer for Server {
        fn authorize_shutdown(&mut self, _: &ShutdownPermit) -> std::io::Result<()> {
            Ok(())
        }

        fn join(self: Box<Self>) -> std::io::Result<()> {
            self.0.lock().expect("test call log").push("join");
            Ok(())
        }
    }

    struct Runtime {
        failure: Failure,
        calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl Runtime {
        fn mark(&self, call: &'static str) {
            self.calls.lock().expect("test call log").push(call);
        }

        fn fail(&self, stage: Failure) -> bool {
            std::mem::discriminant(&self.failure) == std::mem::discriminant(&stage)
        }
    }

    impl UblkRuntime for Runtime {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            Some(ShutdownPermit::injected(()))
        }

        fn wait_for_release_progress(&mut self) {}

        fn guard_not_wsl2(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("guard");
            Ok(())
        }

        fn lock_memory(
            &mut self,
            _force: bool,
            _lock_future: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("lock");
            Ok(())
        }

        fn install_shutdown_handler(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("signal");
            Ok(())
        }

        fn add_device(
            &mut self,
            queue_depth: u16,
        ) -> Result<UblkDevice, Box<dyn std::error::Error>> {
            self.mark("add");
            Ok(UblkDevice { id: 9, queue_depth })
        }

        fn set_params(
            &mut self,
            _device: UblkDevice,
            _sectors: u64,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("params");
            if self.fail(Failure::Params) {
                Err(std::io::Error::other("params failure").into())
            } else {
                Ok(())
            }
        }

        fn start_server(
            &mut self,
            _backend: BackendKind,
            _char_path: &str,
            _block_path: &str,
            _queue_depth: u16,
            _size: u64,
        ) -> Result<Box<dyn UblkServer>, Box<dyn std::error::Error>> {
            self.mark("server");
            if self.fail(Failure::Server) {
                Err(std::io::Error::other("server failure").into())
            } else {
                Ok(Box::new(Server(std::sync::Arc::clone(&self.calls))))
            }
        }

        fn start_device(&mut self, _device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("start");
            if self.fail(Failure::Start) {
                Err(std::io::Error::other("start failure").into())
            } else {
                Ok(())
            }
        }

        fn wait_for_shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("wait");
            if self.fail(Failure::Wait) {
                Err(std::io::Error::other("wait failure").into())
            } else {
                Ok(())
            }
        }

        fn swap_state(
            &mut self,
            _block_path: &str,
        ) -> Result<ExactSwapState, Box<dyn std::error::Error>> {
            self.mark("swap-state");
            Ok(ExactSwapState::Absent)
        }

        fn spawn_swapoff(
            &mut self,
            _block_path: &str,
        ) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.mark("swapoff");
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            tx.send(SwapoffOutcome::Drained).unwrap();
            rx
        }

        fn stop_device(&mut self, _device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("stop");
            Ok(())
        }

        fn delete_device(&mut self, _device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("delete");
            Ok(())
        }
    }

    for (failure, expected) in [
        (
            Failure::Params,
            vec![
                "guard",
                "lock",
                "signal",
                "add",
                "params",
                "swap-state",
                "delete",
            ],
        ),
        (
            Failure::Server,
            vec![
                "guard",
                "lock",
                "signal",
                "add",
                "params",
                "server",
                "swap-state",
                "delete",
            ],
        ),
        (
            Failure::Start,
            vec![
                "guard",
                "lock",
                "signal",
                "add",
                "params",
                "server",
                "start",
                "swap-state",
                "swap-state",
                "stop",
                "join",
                "swap-state",
                "delete",
            ],
        ),
        (
            Failure::Wait,
            vec![
                "guard",
                "lock",
                "signal",
                "add",
                "params",
                "server",
                "start",
                "wait",
                "swap-state",
                "swap-state",
                "stop",
                "join",
                "swap-state",
                "delete",
            ],
        ),
    ] {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut runtime = Runtime {
            failure,
            calls: std::sync::Arc::clone(&calls),
        };
        assert!(run_ublk_with_runtime(4096, false, 1, BackendKind::Ram, &mut runtime).is_err());
        assert_eq!(*calls.lock().expect("test call log"), expected);
    }
}
