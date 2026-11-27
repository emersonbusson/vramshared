// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/writeback_startup/tests.rs

use super::*;
use crate::cascade::cascade_io::tests_suite::{
    ParentSeams, ScriptedRunner, TestDir, bound_device_fixture, install_two_tier_down_snapshots,
    seal_runtime_lifecycle, set_live_device_snapshots, set_swap_snapshots, spawn_fixture_daemon,
};

const EMPTY: &str = "Filename Type Size Used Priority\n";
const ACTIVE: &str = "Filename Type Size Used Priority\n/dev/zram0 partition 65536 0 200\n";

fn steps() -> Vec<(String, Result<String, CascadeError>)> {
    [
        ("modprobe zram", ""),
        ("zramctl --find", "/dev/zram0"),
        (
            "sh -c printf %s /dev/nbd0 > /sys/block/zram0/backing_dev",
            "",
        ),
        ("bash -c echo lzo-rle > /sys/block/zram0/comp_algorithm", ""),
        ("bash -c echo 67108864 > /sys/block/zram0/disksize", ""),
    ]
    .into_iter()
    .map(|(command, result)| (command.into(), Ok(result.into())))
    .collect()
}

fn setup_snapshots(nbd: &BoundDeviceIdentity, zram: &BoundDeviceIdentity, complete: bool) {
    set_live_device_snapshots([
        Ok(vec![nbd.clone()]),
        Ok(vec![zram.clone()]),
        Ok(if complete {
            vec![nbd.clone(), zram.clone()]
        } else {
            vec![nbd.clone()]
        }),
    ]);
}

enum Activation {
    Succeeds,
    FailedAndDrained,
    FailedAndPreserved,
}

struct WritebackRunner<'a> {
    paths: &'a RuntimePaths,
    inner: ScriptedRunner,
    activation: Activation,
}

impl CommandRunner for WritebackRunner<'_> {
    fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
        if command == "mkswap" || command == "swapon" {
            let binding = read_lifecycle_binding(self.paths)?;
            assert_eq!(binding.devices.len(), 2);
            assert!(
                binding
                    .devices
                    .iter()
                    .any(|device| device.path == "/dev/zram0")
            );
            if command == "swapon" {
                match self.activation {
                    Activation::Succeeds => (),
                    Activation::FailedAndDrained => {
                        install_two_tier_down_snapshots(&binding.devices, false, true);
                    }
                    Activation::FailedAndPreserved => {
                        set_swap_snapshots([Ok(ACTIVE), Ok(ACTIVE), Ok(ACTIVE)]);
                        set_live_device_snapshots([
                            Ok(binding.devices.clone()),
                            Ok(binding.devices),
                        ]);
                    }
                }
            }
        }
        self.inner.run(command, args)
    }
}

#[test]
fn writeback_seals_zram_before_activation_and_preserves_failed_seal()
-> Result<(), Box<dyn std::error::Error>> {
    for complete in [true, false] {
        let _seams = ParentSeams::install(EMPTY, 0);
        let fixture = TestDir::new();
        let paths = RuntimePaths::under(&fixture.path);
        let daemon = spawn_fixture_daemon(&fixture, "");
        let nbd = seal_runtime_lifecycle(&paths, daemon.id(), false).remove(0);
        let zram = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
        let original = read_lifecycle_binding(&paths)?;
        pin_compressor(&paths)?;
        setup_snapshots(&nbd, &zram, complete);
        let mut commands = steps();
        if complete {
            commands.extend([
                ("mkswap /dev/zram0".into(), Ok(String::new())),
                ("swapon -p 200 /dev/zram0".into(), Ok(String::new())),
            ]);
        }
        let runner = WritebackRunner {
            paths: &paths,
            inner: ScriptedRunner::new(commands),
            activation: Activation::Succeeds,
        };

        let outcome = arm_zram(&runner, &paths, 64, 200, &nbd.path, daemon.id());
        if complete {
            assert_eq!(outcome?, zram.path);
        } else {
            assert!(matches!(outcome, Err(CascadeError::UnsafeContainment(_))));
            assert_eq!(read_lifecycle_binding(&paths)?, original);
            assert_eq!(read_bound_device_record(&paths.zram_dev_file)?, zram);
            assert!(!process_is_gone_or_zombie(daemon.id()));
        }
        assert!(runner.inner.responses.borrow().is_empty());
    }
    Ok(())
}

#[test]
fn writeback_startup_rollback_keeps_the_backend_on_refusal()
-> Result<(), Box<dyn std::error::Error>> {
    for drain_succeeds in [true, false] {
        let _seams = ParentSeams::install(EMPTY, 0);
        let fixture = TestDir::new();
        let paths = RuntimePaths::under(&fixture.path);
        let daemon = spawn_fixture_daemon(&fixture, "exit 0");
        let nbd = seal_runtime_lifecycle(&paths, daemon.id(), false).remove(0);
        let zram = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
        pin_compressor(&paths)?;
        arm_forensics_at(&paths);
        setup_snapshots(&nbd, &zram, true);
        let mut commands = steps();
        commands.extend([
            ("mkswap /dev/zram0".into(), Ok(String::new())),
            (
                "swapon -p 200 /dev/zram0".into(),
                Err(CascadeError::Io("swapon acknowledgement lost".into())),
            ),
            (
                "swapoff -- /dev/zram0".into(),
                if drain_succeeds {
                    Ok(String::new())
                } else {
                    Err(CascadeError::Io("swapoff refused".into()))
                },
            ),
        ]);
        if drain_succeeds {
            commands.extend([
                ("zramctl -r /dev/zram0".into(), Ok(String::new())),
                ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
            ]);
        }
        let runner = WritebackRunner {
            paths: &paths,
            inner: ScriptedRunner::new(commands),
            activation: if drain_succeeds {
                Activation::FailedAndDrained
            } else {
                Activation::FailedAndPreserved
            },
        };
        let outcome = arm_zram(&runner, &paths, 64, 200, &nbd.path, daemon.id());
        if drain_succeeds {
            assert!(
                matches!(outcome, Err(CascadeError::Shell { .. })),
                "{outcome:?}"
            );
            assert!(!paths.lifecycle_binding_file.exists());
            assert!(process_is_gone_or_zombie(daemon.id()));
        } else {
            assert!(
                matches!(outcome, Err(CascadeError::UnsafeContainment(ref error)) if error.contains("swapoff refused")),
                "{outcome:?}"
            );
            assert_eq!(read_lifecycle_binding(&paths)?.devices.len(), 2);
            assert!(paths.forensics_markers[0].exists());
            assert!(!process_is_gone_or_zombie(daemon.id()));
        }
        assert!(runner.inner.responses.borrow().is_empty());
    }
    Ok(())
}

fn pin_compressor(paths: &RuntimePaths) -> std::io::Result<()> {
    let zram = paths.sys_block.join("zram0");
    fs::create_dir_all(&zram)?;
    fs::write(zram.join("comp_algorithm"), "[lzo-rle] lz4")
}

#[test]
fn disconnect_refuses_a_bound_zram_still_in_the_swap_table()
-> Result<(), Box<dyn std::error::Error>> {
    let _seams = ParentSeams::install(ACTIVE, 0);
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    let nbd = devices
        .into_iter()
        .find(|device| device.kind == ManagedDeviceKind::Nbd)
        .ok_or("fixture omitted the NBD")?;
    set_live_device_snapshots([Ok(vec![nbd.clone()])]);
    let binding = read_lifecycle_binding(&paths)?;
    let runner = ScriptedRunner::new(Vec::new());
    let executor = super::super::teardown::RuntimeNbdLifecycleExecutor::new(
        &runner,
        &paths,
        &binding,
        PeerState::Live,
        None,
    );
    let result = super::super::teardown::NbdLifecycleExecutor::disconnect_nbd(&executor, &nbd);
    assert!(
        matches!(result, Err(CascadeError::UnsafeContainment(ref error))
            if error.contains("zram still has swap")),
        "{result:?}"
    );
    assert!(runner.calls.borrow().is_empty());
    Ok(())
}

#[test]
fn writeback_refuses_attachment_drift_and_active_tiers() -> Result<(), Box<dyn std::error::Error>> {
    let _seams = ParentSeams::install(EMPTY, 0);
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let nbd = seal_runtime_lifecycle(&paths, daemon.id(), false).remove(0);
    let runner = ScriptedRunner::new(Vec::new());

    let changed = arm_zram(&runner, &paths, 64, 200, &nbd.path, daemon.id() + 1);
    assert!(
        matches!(changed, Err(CascadeError::UnsafeContainment(ref error))
            if error.contains("exact NBD-only lifecycle")),
        "{changed:?}"
    );
    const ACTIVE_NBD: &str = "Filename Type Size Used Priority\n/dev/nbd0 partition 65536 0 100\n";
    set_swap_snapshots([Ok(ACTIVE_NBD)]);
    set_live_device_snapshots([Ok(vec![nbd.clone()])]);
    let armed = arm_zram(&runner, &paths, 64, 200, &nbd.path, daemon.id());
    assert!(
        matches!(armed, Err(CascadeError::UnsafeContainment(ref error))
            if error.contains("outside the swap table")),
        "{armed:?}"
    );

    let zram = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
    let binding = read_lifecycle_binding(&paths)?;
    let mut stale = binding.clone();
    stale.daemon_pid += 1;
    let drifted = seal_zram(&paths, stale, &zram);
    assert!(
        matches!(drifted, Err(CascadeError::UnsafeContainment(ref error))
            if error.contains("changed before")),
        "{drifted:?}"
    );

    set_swap_snapshots([Ok(ACTIVE)]);
    set_live_device_snapshots([Ok(vec![nbd, zram.clone()])]);
    let active = seal_zram(&paths, binding.clone(), &zram);
    assert!(
        matches!(active, Err(CascadeError::UnsafeContainment(ref error))
            if error.contains("already active as swap")),
        "{active:?}"
    );
    assert_eq!(read_lifecycle_binding(&paths)?, binding);
    assert!(runner.calls.borrow().is_empty());
    Ok(())
}

#[test]
fn zram_preparation_refusal_preserves_writeback_backend() -> Result<(), Box<dyn std::error::Error>>
{
    for command_index in [1, 2, 4] {
        let _seams = ParentSeams::install(EMPTY, 0);
        let fixture = TestDir::new();
        let paths = RuntimePaths::under(&fixture.path);
        let daemon = spawn_fixture_daemon(&fixture, "exit 0");
        let nbd = seal_runtime_lifecycle(&paths, daemon.id(), false).remove(0);
        let binding = read_lifecycle_binding(&paths)?;
        set_live_device_snapshots([Ok(vec![nbd.clone()])]);
        let mut commands = steps();
        commands.truncate(command_index + 1);
        commands[command_index].1 =
            Err(CascadeError::UnsafeContainment("unverified backing".into()));
        let runner = ScriptedRunner::new(commands);

        let outcome = arm_zram(&runner, &paths, 64, 200, &nbd.path, daemon.id());
        assert!(
            matches!(outcome, Err(CascadeError::UnsafeContainment(ref message))
            if message == "unverified backing"),
            "{outcome:?}"
        );
        assert!(runner.responses.borrow().is_empty());
        assert_eq!(read_lifecycle_binding(&paths)?, binding);
        assert!(!process_is_gone_or_zombie(daemon.id()));
    }
    Ok(())
}
