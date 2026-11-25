// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/teardown/tests/mod.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

pub(super) use std::cell::RefCell;

pub(super) use super::swapoff_headroom::{
    HeadroomVerdict, MARGIN_BYTES, MIN_RESERVE_BYTES, MemInfo,
};

pub(super) use super::*;

pub(super) const GIB: u64 = 1024 * 1024 * 1024;

thread_local! {
    static ORIGIN_DEVICE_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub(super) struct OriginDeviceFixture(Option<PathBuf>);

impl OriginDeviceFixture {
    fn new(root: PathBuf) -> Self {
        Self(ORIGIN_DEVICE_ROOT.with(|slot| slot.replace(Some(root))))
    }
}

impl Drop for OriginDeviceFixture {
    fn drop(&mut self) {
        ORIGIN_DEVICE_ROOT.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

pub(super) fn fixture_origin_path(recorded: &str) -> Option<PathBuf> {
    ORIGIN_DEVICE_ROOT.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|root| root.join(recorded.trim_start_matches("/dev/")))
    })
}

pub(super) fn device(path: &str, kind: ManagedDeviceKind) -> BoundDeviceIdentity {
    BoundDeviceIdentity {
        kind,
        path: path.to_string(),
        dev_t: "0:0".to_string(),
        sysfs_path: format!("/sys/block/{}", path.trim_start_matches("/dev/")),
        sysfs_dev_t: "0:0".to_string(),
        kernel_owner_instance_id: None,
    }
}

/// A binding whose peer is gone: only the device set matters to the
/// released plan, and the peer fields are never consulted there.
pub(super) fn released_binding(devices: &[BoundDeviceIdentity]) -> LifecycleBinding {
    LifecycleBinding {
        schema_version: 1,
        boot_id: "11111111-2222-4333-8444-555555555555".into(),
        daemon_invocation_id: "0123456789abcdef0123456789abcdef".into(),
        daemon_pid: 0,
        daemon_start_ticks: 0,
        daemon_instance_id: "0-0".into(),
        export_socket: BoundSocketIdentity {
            path: "/run/ramshared/wsl2d.sock".into(),
            filesystem_dev: 0,
            inode: 0,
        },
        origin: BoundOriginIdentity {
            partuuid: "00000000-0000-0000-0000-000000000000".into(),
            ptuuid: "00000000-0000-0000-0000-000000000000".into(),
            partition_dev_t: "0:0".into(),
            parent_dev_t: "0:0".into(),
            expected_swap_uuid: "00000000-0000-0000-0000-000000000000".into(),
            host_manifest_sha256: "0".repeat(64),
            configuration_sha256: "0".repeat(64),
        },
        devices: devices.to_vec(),
    }
}

pub(super) fn swap(path: &str, used: u64) -> SwapEntry {
    SwapEntry {
        filename: path.to_string(),
        size_kb: 8 * 1024 * 1024,
        used_kb: used / 1024,
        priority: -2,
    }
}

pub(super) const FALLBACK_SWAPS: &str =
    "Filename Type Size Used Priority\n/dev/sdc partition 4194304 1024 -2\n";

pub(super) const RECOVERY_SWAPS: &str = "Filename Type Size Used Priority\n/dev/zram0 partition 1048576 0 200\n/dev/sdc partition 4194304 1024 -2\n";

/// Real sealed records and isolated observations drive the production
/// recovery entry point. The command boundary never reaches host devices.
pub(super) struct RecoveryFixture {
    _platform: swapoff_headroom::NativePlatformFixture,
    root: PathBuf,
    paths: RuntimePaths,
    binding: LifecycleBinding,
    evidence: Vec<(PathBuf, Vec<u8>)>,
}

impl RecoveryFixture {
    fn new() -> Self {
        let platform = swapoff_headroom::NativePlatformFixture::new();
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("recovery-{}-{sequence}", std::process::id()));
        fs::create_dir(&root).expect("new recovery fixture");
        let paths = RuntimePaths::under(&root);
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let nbd = observe_bound_device("/dev/nbd0", ManagedDeviceKind::Nbd).unwrap();
        let zram = observe_bound_device("/dev/zram0", ManagedDeviceKind::Zram).unwrap();
        let mut binding = released_binding(&[nbd, zram.clone()]);
        let origin = read_sealed_origin_config().unwrap();
        binding.origin = BoundOriginIdentity {
            partuuid: origin.partuuid,
            ptuuid: origin.ptuuid,
            partition_dev_t: origin.partition_dev_t,
            parent_dev_t: origin.parent_dev_t,
            expected_swap_uuid: origin.expected_swap_uuid,
            host_manifest_sha256: origin.host_manifest_sha256,
            configuration_sha256: origin.configuration_sha256,
        };
        binding.daemon_pid = i32::MAX as u32;
        binding.daemon_start_ticks = 100;
        binding.daemon_instance_id = format!("{}-100", binding.daemon_pid);
        assert!(daemon_instance_id_from_pid(binding.daemon_pid).is_none());
        fs::write(&paths.socket, "released peer socket evidence").unwrap();
        binding.export_socket = socket_identity(&paths.socket).unwrap();
        write_nbd_path_record(&paths.swap_dev_file, "/dev/nbd0").unwrap();
        write_bound_device_record(&paths.zram_dev_file, &zram).unwrap();
        write_lifecycle_binding(&paths, &binding).unwrap();
        for path in [
            &paths.pid_file,
            &paths.capacity_status_file,
            &paths.cache_status_file,
            &paths.supervisor_status_file,
            &paths.cache_target_request_file,
            &paths.reclaim_request_file,
        ] {
            fs::write(path, "retained lifecycle evidence").unwrap();
        }
        let marker = &paths.forensics_markers[0];
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(marker, "isolated fixture evidence").unwrap();
        let evidence = fs::read_dir(&paths.runtime_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .chain(paths.forensics_markers.iter().cloned())
            .map(|path| {
                let contents = fs::read(&path).unwrap();
                (path, contents)
            })
            .collect();
        TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(RECOVERY_SWAPS.into()));
        TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().clear());
        TEST_SWAPS_ERROR.with(|cell| *cell.borrow_mut() = None);
        Self {
            _platform: platform,
            root,
            paths,
            binding,
            evidence,
        }
    }

    fn observations(&self, snapshots: Vec<Vec<BoundDeviceIdentity>>) {
        TEST_LIVE_MANAGED_DEVICES.with(|queue| {
            *queue.borrow_mut() = snapshots.into_iter().map(Ok).collect();
        });
    }

    fn stale_only() -> Self {
        let mut fixture = Self::new();
        for (path, _) in &fixture.evidence {
            fs::remove_file(path).unwrap();
        }
        let stale = observe_bound_device("/dev/zram999999", ManagedDeviceKind::Zram).unwrap();
        write_bound_device_record(&fixture.paths.zram_dev_file, &stale).unwrap();
        fixture.evidence = vec![(
            fixture.paths.zram_dev_file.clone(),
            fs::read(&fixture.paths.zram_dev_file).unwrap(),
        )];
        TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(FALLBACK_SWAPS.into()));
        fixture
    }

    fn ordinary_recovery_observations(&self) {
        let nbd = vec![self.binding.devices[0].clone()];
        self.observations(vec![
            self.binding.devices.clone(),
            self.binding.devices.clone(),
            self.binding.devices.clone(),
            nbd.clone(),
            nbd,
            Vec::new(),
        ]);
    }

    fn assert_preserved(&self) {
        for (path, contents) in &self.evidence {
            assert_eq!(fs::read(path).unwrap(), *contents, "{}", path.display());
        }
    }

    fn assert_cleared(&self) {
        for (path, _) in &self.evidence {
            assert!(!path.exists(), "{}", path.display());
        }
        assert_eq!(
            read_swaps().unwrap(),
            parse_proc_swaps(FALLBACK_SWAPS).unwrap()
        );
    }
}

impl Drop for RecoveryFixture {
    fn drop(&mut self) {
        TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
        TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().clear());
        TEST_SWAPS_ERROR.with(|cell| *cell.borrow_mut() = None);
        TEST_LIVE_MANAGED_DEVICES.with(|queue| queue.borrow_mut().clear());
        fs::remove_dir_all(&self.root).unwrap();
    }
}

pub(super) struct RecoveryRunner<'a> {
    fixture: &'a RecoveryFixture,
    calls: RefCell<Vec<String>>,
    failure: Option<&'static str>,
    release_before_failure: bool,
    rearm_after_disconnect: bool,
    cleanup_failure_after_disconnect: bool,
}

impl<'a> RecoveryRunner<'a> {
    fn new(fixture: &'a RecoveryFixture) -> Self {
        Self {
            fixture,
            calls: RefCell::new(Vec::new()),
            failure: None,
            release_before_failure: false,
            rearm_after_disconnect: false,
            cleanup_failure_after_disconnect: false,
        }
    }
}

impl CommandRunner for RecoveryRunner<'_> {
    fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
        self.fixture.assert_preserved();
        let label = command_label(command, args);
        self.calls.borrow_mut().push(label.clone());
        let fail = self.failure == Some(command);
        if label == "swapoff -- /dev/zram0" && (!fail || self.release_before_failure) {
            TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(FALLBACK_SWAPS.into()));
        }
        if label == "nbd-client -d /dev/nbd0" && self.rearm_after_disconnect {
            TEST_SWAPS.with(|cell| {
                *cell.borrow_mut() = Some(format!(
                    "{FALLBACK_SWAPS}/dev/nbd0 partition 1048576 0 100\n"
                ));
            });
        }
        if label == "nbd-client -d /dev/nbd0" && self.cleanup_failure_after_disconnect {
            fs::remove_file(&self.fixture.paths.cache_status_file).unwrap();
            fs::create_dir(&self.fixture.paths.cache_status_file).unwrap();
        }
        if fail {
            Err(CascadeError::Shell {
                cmd: label,
                msg: "injected release failure".into(),
            })
        } else {
            Ok(String::new())
        }
    }
}

#[test]
fn bound_recovery_clears_evidence_only_after_verified_release() {
    let fixture = RecoveryFixture::new();
    fixture.ordinary_recovery_observations();
    let runner = RecoveryRunner::new(&fixture);
    down_recover_released_peer_with(&runner, &fixture.paths).unwrap();
    assert_eq!(
        *runner.calls.borrow(),
        [
            "swapoff -- /dev/zram0",
            "zramctl -r /dev/zram0",
            "nbd-client -d /dev/nbd0"
        ]
    );
    fixture.assert_cleared();
    assert!(TEST_LIVE_MANAGED_DEVICES.with(|queue| queue.borrow().is_empty()));
    assert!(down_recover_released_peer_with(&runner, &fixture.paths).is_err());
    assert_eq!(runner.calls.borrow().len(), 3, "retry has no device effect");
}

#[test]
fn bound_recovery_preserves_all_evidence_after_each_failed_effect() {
    for (command, count) in [("swapoff", 1), ("zramctl", 2), ("nbd-client", 3)] {
        let fixture = RecoveryFixture::new();
        fixture.ordinary_recovery_observations();
        let mut runner = RecoveryRunner::new(&fixture);
        runner.failure = Some(command);
        let error = down_recover_released_peer_with(&runner, &fixture.paths).unwrap_err();
        assert!(
            error.to_string().contains("injected release failure"),
            "{error}"
        );
        assert_eq!(runner.calls.borrow().len(), count, "{command}");
        fixture.assert_preserved();
    }
}

#[test]
fn bound_recovery_accepts_failed_swapoff_only_after_proven_absence() {
    let fixture = RecoveryFixture::new();
    fixture.ordinary_recovery_observations();
    let mut runner = RecoveryRunner::new(&fixture);
    runner.failure = Some("swapoff");
    runner.release_before_failure = true;
    down_recover_released_peer_with(&runner, &fixture.paths).unwrap();
    assert_eq!(runner.calls.borrow().len(), 3);
    fixture.assert_cleared();
}

#[test]
fn bound_recovery_preserves_evidence_when_disconnect_completion_is_unproven() {
    let fixture = RecoveryFixture::new();
    fixture.ordinary_recovery_observations();
    let mut runner = RecoveryRunner::new(&fixture);
    runner.rearm_after_disconnect = true;
    let error = down_recover_released_peer_with(&runner, &fixture.paths).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("disconnect completion is unproven"),
        "{error}"
    );
    assert_eq!(runner.calls.borrow().len(), 3);
    fixture.assert_preserved();
}

#[test]
fn bound_recovery_refuses_blocked_headroom_without_an_effect() {
    let fixture = RecoveryFixture::new();
    fixture.observations(vec![fixture.binding.devices.clone()]);
    // The observation exceeds all resident RAM, independently of the
    // host's current pressure. No memory or swap is actually allocated.
    let total = swapoff_headroom::read_meminfo().unwrap().total_bytes / 1024;
    TEST_SWAPS.with(|cell| {
        *cell.borrow_mut() = Some(format!(
            "{FALLBACK_SWAPS}/dev/zram0 partition {} {} 200\n",
            total + 1,
            total + 1,
        ));
    });
    let runner = RecoveryRunner::new(&fixture);
    let error = down_recover_released_peer_with(&runner, &fixture.paths).unwrap_err();
    assert!(
        error.to_string().contains("managed_swap_exceeds_headroom"),
        "{error}"
    );
    assert!(runner.calls.borrow().is_empty());
    fixture.assert_preserved();
}

#[test]
fn bound_recovery_revalidates_identity_and_authority_before_effects() {
    for refusal in [
        "live-peer",
        "armed-origin",
        "foreign-swap",
        "boot",
        "origin",
    ] {
        let mut fixture = RecoveryFixture::new();
        match refusal {
            "live-peer" => {
                fixture.binding.daemon_pid = std::process::id();
                fixture.binding.daemon_instance_id =
                    daemon_instance_id_from_pid(std::process::id()).unwrap();
                fixture.binding.daemon_start_ticks = fixture
                    .binding
                    .daemon_instance_id
                    .rsplit_once('-')
                    .unwrap()
                    .1
                    .parse()
                    .unwrap();
            }
            "armed-origin" | "foreign-swap" => TEST_SWAPS.with(|cell| {
                let path = if refusal == "armed-origin" {
                    "/dev/nbd0"
                } else {
                    "/dev/nbd1"
                };
                *cell.borrow_mut() = Some(format!("{RECOVERY_SWAPS}{path} partition 1024 0 100\n"));
            }),
            "boot" => fixture.binding.boot_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            "origin" => fixture.binding.origin.configuration_sha256 = "0".repeat(64),
            _ => unreachable!(),
        }
        write_lifecycle_binding(&fixture.paths, &fixture.binding).unwrap();
        for (path, contents) in &mut fixture.evidence {
            *contents = fs::read(path).unwrap();
        }
        fixture.observations(vec![fixture.binding.devices.clone()]);
        let runner = RecoveryRunner::new(&fixture);
        assert!(
            down_recover_released_peer_with(&runner, &fixture.paths).is_err(),
            "{refusal}"
        );
        assert!(runner.calls.borrow().is_empty(), "{refusal}");
        fixture.assert_preserved();
    }
}

#[test]
fn released_zram_reset_uses_the_observed_remaining_device_set() {
    let fixture = RecoveryFixture::new();
    let zram = fixture.binding.devices[1].clone();
    TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(FALLBACK_SWAPS.into()));
    // A prior release already removed NBD from the live set. Resetting
    // the remaining zram must not demand that NBD become live again.
    fixture.observations(vec![vec![zram.clone()], Vec::new()]);
    let runner = RecoveryRunner::new(&fixture);
    let executor = RuntimeNbdLifecycleExecutor::new(
        &runner,
        &fixture.paths,
        &fixture.binding,
        PeerState::Released,
        None,
    );
    executor
        .reset_zram(&zram)
        .expect("verify the actual remaining set");
    assert_eq!(*runner.calls.borrow(), ["zramctl -r /dev/zram0"]);
    fixture.assert_preserved();
}

#[test]
fn released_zram_reset_preserves_evidence_when_the_device_remains_live() {
    let fixture = RecoveryFixture::new();
    let zram = fixture.binding.devices[1].clone();
    TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(FALLBACK_SWAPS.into()));
    fixture.observations(vec![vec![zram.clone()], vec![zram.clone()]]);
    let runner = RecoveryRunner::new(&fixture);
    let executor = RuntimeNbdLifecycleExecutor::new(
        &runner,
        &fixture.paths,
        &fixture.binding,
        PeerState::Released,
        None,
    );
    let error = executor.reset_zram(&zram).unwrap_err();
    assert!(
        error.to_string().contains("reset completion is unproven"),
        "{error}"
    );
    assert_eq!(*runner.calls.borrow(), ["zramctl -r /dev/zram0"]);
    fixture.assert_preserved();
}

#[test]
fn released_zram_reset_refuses_foreign_or_disappearing_remaining_devices() {
    let foreign = observe_bound_device("/dev/nbd1", ManagedDeviceKind::Nbd).unwrap();
    for live_after in [vec![foreign], Vec::new()] {
        let fixture = RecoveryFixture::new();
        let zram = fixture.binding.devices[1].clone();
        TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(FALLBACK_SWAPS.into()));
        fixture.observations(vec![fixture.binding.devices.clone(), live_after]);
        let runner = RecoveryRunner::new(&fixture);
        let executor = RuntimeNbdLifecycleExecutor::new(
            &runner,
            &fixture.paths,
            &fixture.binding,
            PeerState::Released,
            None,
        );
        assert!(executor.reset_zram(&zram).is_err());
        assert_eq!(*runner.calls.borrow(), ["zramctl -r /dev/zram0"]);
        fixture.assert_preserved();
    }
}

#[test]
fn unbound_stale_recovery_preserves_live_or_unproved_peer_custody() {
    for evidence in [
        "live-pid",
        "invalid-pid",
        "socket",
        "cache",
        "marker",
        "foreign-swap",
    ] {
        let mut fixture = RecoveryFixture::stale_only();
        let path = match evidence {
            "live-pid" | "invalid-pid" => Some(&fixture.paths.pid_file),
            "socket" => Some(&fixture.paths.socket),
            "cache" => Some(&fixture.paths.cache_status_file),
            "marker" => Some(&fixture.paths.forensics_markers[0]),
            "foreign-swap" => None,
            _ => unreachable!(),
        };
        if let Some(path) = path {
            let content = if evidence == "live-pid" {
                std::process::id().to_string()
            } else {
                "unproved lifecycle custody".into()
            };
            fs::write(path, content).unwrap();
            fixture
                .evidence
                .push((path.clone(), fs::read(path).unwrap()));
        } else {
            TEST_SWAPS.with(|cell| {
                *cell.borrow_mut() =
                    Some(format!("{FALLBACK_SWAPS}/dev/nbd7 partition 1024 0 100\n"))
            });
        }
        let runner = RecoveryRunner::new(&fixture);
        let result = down_recover_released_peer_with(&runner, &fixture.paths);
        assert!(
            result.is_err(),
            "{evidence} must preserve ambiguous custody"
        );
        assert!(runner.calls.borrow().is_empty());
        fixture.assert_preserved();
    }
}

#[test]
fn unbound_stale_recovery_preserves_a_dangling_peer_socket() {
    let fixture = RecoveryFixture::stale_only();
    let missing = fixture.root.join("unavailable-peer-socket");
    std::os::unix::fs::symlink(&missing, &fixture.paths.socket).unwrap();
    let runner = RecoveryRunner::new(&fixture);
    assert!(down_recover_released_peer_with(&runner, &fixture.paths).is_err());
    assert!(runner.calls.borrow().is_empty());
    fixture.assert_preserved();
    assert_eq!(fs::read_link(&fixture.paths.socket).unwrap(), missing);
}

#[derive(Default)]
pub(super) struct RecordingExecutor {
    calls: RefCell<Vec<&'static str>>,
}

impl NbdLifecycleExecutor for RecordingExecutor {
    fn swapoff(&self, _device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        self.calls.borrow_mut().push("swapoff");
        Ok(())
    }
    fn reset_zram(&self, _device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        self.calls.borrow_mut().push("reset-zram");
        Ok(())
    }
    fn disconnect_nbd(&self, _device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        self.calls.borrow_mut().push("disconnect-nbd");
        Ok(())
    }
    fn stop_daemon(&self) -> Result<(), CascadeError> {
        self.calls.borrow_mut().push("stop-daemon");
        Ok(())
    }
}

#[test]
fn blocked_swapoff_headroom_runs_no_command() {
    let zram = device("/dev/zram0", ManagedDeviceKind::Zram);
    let plan = NbdLifecyclePlan {
        actions: vec![
            NbdLifecycleAction::Swapoff(zram.clone()),
            NbdLifecycleAction::ResetZram(zram),
            NbdLifecycleAction::StopDaemon,
        ],
    };
    let swaps = vec![swap("/dev/zram0", 8 * GIB)];
    let decision = headroom_for_plan_with_mem_and_absorber(
        &plan,
        &swaps,
        0,
        MemInfo {
            total_bytes: 16 * GIB,
            available_bytes: GIB,
        },
    );
    assert!(!decision.is_safe(), "{decision:?}");

    // This is the gate shape: execution is reachable only after SAFE.
    let executor = RecordingExecutor::default();
    if decision.is_safe() {
        execute_nbd_lifecycle_plan(&plan, &executor).expect("safe plan");
    }
    assert!(executor.calls.borrow().is_empty());
}

#[path = "cleanup.rs"]
mod cleanup;
#[path = "headroom_and_plan.rs"]
mod headroom_and_plan;
