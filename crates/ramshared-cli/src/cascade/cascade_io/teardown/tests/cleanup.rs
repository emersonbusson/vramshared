// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/teardown/tests/cleanup.rs
use super::*;

#[derive(Default)]
struct RefuseRepeatedEffects(RefCell<Vec<String>>);

impl CommandRunner for RefuseRepeatedEffects {
    fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
        self.0.borrow_mut().push(command_label(command, args));
        Err(CascadeError::UnsafeContainment(
            "a released device must not receive another teardown effect".into(),
        ))
    }
}

#[test]
fn partial_cleanup_reauthorizes_only_the_sealed_completion_phase() {
    let fixture = RecoveryFixture::new();
    fixture.ordinary_recovery_observations();
    let mut first = RecoveryRunner::new(&fixture);
    first.cleanup_failure_after_disconnect = true;
    let error = down_recover_released_peer_with(&first, &fixture.paths).unwrap_err();
    assert!(error.to_string().contains("runtime cleanup incomplete"));
    assert_eq!(first.calls.borrow().len(), 3);
    assert!(!fixture.paths.socket.exists());
    assert!(!fixture.paths.swap_dev_file.exists());
    assert!(!fixture.paths.zram_dev_file.exists());
    assert!(fixture.paths.lifecycle_binding_file.exists());

    // Repair only the injected obstruction. The full command must authorize
    // the retained binding again, without recreating the removed records.
    fs::remove_dir(&fixture.paths.cache_status_file).unwrap();
    fixture.observations(vec![Vec::new(); 4]);
    let retry = RefuseRepeatedEffects::default();
    down_recover_released_peer_with(&retry, &fixture.paths)
        .expect("fresh detached proof must permit completion without replaying device effects");
    assert!(retry.0.borrow().is_empty());
    fixture.assert_cleared();
}

#[test]
fn partial_cleanup_retry_refuses_changed_authority() {
    for changed in ["boot", "origin", "peer", "foreign", "rearmed", "node"] {
        let fixture = RecoveryFixture::new();
        fixture.ordinary_recovery_observations();
        let mut first = RecoveryRunner::new(&fixture);
        first.cleanup_failure_after_disconnect = true;
        assert!(
            down_recover_released_peer_with(&first, &fixture.paths)
                .unwrap_err()
                .to_string()
                .contains("runtime cleanup incomplete")
        );
        fs::remove_dir(&fixture.paths.cache_status_file).unwrap();
        let mut binding = fixture.binding.clone();
        let mut live = Vec::new();
        match changed {
            "boot" => binding.boot_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            "origin" => binding.origin.configuration_sha256 = "a".repeat(64),
            "peer" => binding.daemon_pid = std::process::id(),
            "foreign" => {
                live.push(observe_bound_device("/dev/nbd7", ManagedDeviceKind::Nbd).unwrap());
            }
            "rearmed" => TEST_SWAPS.with(|cell| {
                *cell.borrow_mut() = Some(format!(
                    "{FALLBACK_SWAPS}/dev/nbd0 partition 1048576 4 100\n"
                ));
            }),
            "node" => {
                binding.devices[0].dev_t = "43:99".into();
                binding.devices[0].sysfs_dev_t = "43:99".into();
            }
            _ => unreachable!(),
        }
        if changed == "peer" {
            // Keep the binding itself valid so the current live PID is the
            // refusal, rather than an unrelated serialized identity error.
            let instance = daemon_instance_id_from_pid(binding.daemon_pid).unwrap();
            binding.daemon_start_ticks = instance.rsplit('-').next().unwrap().parse().unwrap();
            binding.daemon_instance_id = instance;
        }
        write_lifecycle_binding(&fixture.paths, &binding).unwrap();
        fixture.observations(vec![live; 4]);
        let remaining = fixture
            .evidence
            .iter()
            .filter(|(path, _)| path.is_file())
            .map(|(path, _)| (path.clone(), fs::read(path).unwrap()))
            .collect::<Vec<_>>();
        let retry = RefuseRepeatedEffects::default();
        let error = down_recover_released_peer_with(&retry, &fixture.paths).expect_err(changed);
        assert!(retry.0.borrow().is_empty(), "{changed}: {error}");
        for (path, bytes) in remaining {
            assert_eq!(fs::read(path).unwrap(), bytes, "{changed}");
        }
    }
}

#[test]
fn released_completion_rechecks_peer_absence() {
    let fixture = RecoveryFixture::new();
    let mut binding = fixture.binding.clone();
    binding.daemon_pid = std::process::id();
    let instance = daemon_instance_id_from_pid(binding.daemon_pid).unwrap();
    binding.daemon_start_ticks = instance.rsplit('-').next().unwrap().parse().unwrap();
    binding.daemon_instance_id = instance;
    TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(FALLBACK_SWAPS.into()));
    let error = authorize_bound_environment_for(
        &binding,
        &fixture.paths,
        &read_swaps().unwrap(),
        &[],
        PeerState::Released,
    )
    .expect_err("every released-stage authorization must freshly reject a live peer");
    assert!(error.to_string().contains("peer is alive"), "{error}");
    fixture.assert_preserved();
}

#[test]
fn released_completion_requires_no_remaining_devices() {
    for path in ["/dev/nbd0", "/dev/nbd7", "/dev/zram0"] {
        let fixture = RecoveryFixture::new();
        let kind = device_kind_for_path(path).unwrap();
        let live = observe_bound_device(path, kind).unwrap();
        fixture.observations(vec![vec![live]]);
        TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(FALLBACK_SWAPS.into()));
        let runner = RefuseRepeatedEffects::default();
        let executor = RuntimeNbdLifecycleExecutor::new(
            &runner,
            &fixture.paths,
            &fixture.binding,
            PeerState::Released,
            None,
        );
        executor.stop_daemon().expect_err(path);
        assert!(runner.0.borrow().is_empty());
        fixture.assert_preserved();
    }
}

#[test]
fn recovery_cleanup_failure_is_reported_and_keeps_the_binding() {
    let fixture = RecoveryFixture::new();
    fixture.ordinary_recovery_observations();
    let binding = fs::read(&fixture.paths.lifecycle_binding_file).unwrap();
    let marker = fs::read(&fixture.paths.forensics_markers[0]).unwrap();
    let mut runner = RecoveryRunner::new(&fixture);
    runner.cleanup_failure_after_disconnect = true;
    let error = down_recover_released_peer_with(&runner, &fixture.paths)
        .expect_err("a failed unlink must not report completed recovery");
    assert!(
        error.to_string().contains("runtime cleanup incomplete"),
        "{error}"
    );
    assert_eq!(runner.calls.borrow().len(), 3);
    assert!(fixture.paths.cache_status_file.is_dir());
    assert_eq!(
        fs::read(&fixture.paths.lifecycle_binding_file).unwrap(),
        binding
    );
    assert_eq!(
        fs::read(&fixture.paths.forensics_markers[0]).unwrap(),
        marker
    );
}
