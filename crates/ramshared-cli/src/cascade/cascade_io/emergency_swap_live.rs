// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/emergency_swap_live.rs
//! Real emergency swap lifecycle in the disposable kernel fixture.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

/// Runs only in the disposable kernel fixture, while its controller owns
/// the NBD workload and holds this process responsible for the swap file.
#[test]
#[ignore = "requires the isolated s18 kernel fixture"]
fn isolated_kernel_emergency_lifecycle() {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap();
    assert!(
        cmdline
            .split_whitespace()
            .any(|v| v == "ktest.scenario=s18")
    );
    let runner = super::super::command_runner::SystemCommandRunner;
    let emergency = create_in(
        &runner,
        448 * MIB,
        &[PathBuf::from("/mnt/emergency")],
        &linux_free_bytes,
    )
    .expect("create real emergency swap");
    assert!(
        emergency
            .free_bytes(&crate::cascade::read_swaps().unwrap())
            .unwrap()
            > 0
    );
    std::fs::write("/tmp/emergency-ready", emergency.path.to_str().unwrap()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    while !Path::new("/tmp/emergency-release").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture release timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(emergency.remove(&runner), "remove real emergency swap");
    assert!(!emergency.path.exists());
    std::fs::write("/tmp/emergency-removed", b"ok").unwrap();
}

/// A two-boot, disposable ext4 fixture. The guest init owns the kernel,
/// backing disk and the separately paced pageout process.
#[test]
#[ignore = "requires the disposable two-boot KVM swap fixture"]
fn isolated_kernel_retained_absorber_across_real_boots() {
    use super::super::absorber_record::with_kernel_boot_id;
    use super::super::command_runner::SystemCommandRunner;

    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap();
    assert!(
        cmdline
            .split_whitespace()
            .any(|value| value == "ktest.scenario=absorber-reboot")
    );
    let root = PathBuf::from("/mnt/absorber");
    let paths = RuntimePaths::under(&root);
    let phase = std::env::var("RAMSHARED_ABSORBER_LAB_PHASE").unwrap();
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let boot_id = boot_id.trim();

    with_kernel_boot_id(|| match phase.as_str() {
        "activate" => {
            std::fs::create_dir(&paths.runtime_dir).unwrap();
            let store = AbsorberRecordStore::open(&paths).unwrap();
            let swap = create_in_before_activation(
                &SystemCommandRunner,
                16 * MIB,
                std::slice::from_ref(&root),
                &linux_free_bytes,
                &|backing| store.publish(backing),
            )
            .expect("activate the guest's private ext4 swap file");
            assert!(
                swap.free_bytes(&crate::cascade::read_swaps().unwrap())
                    .unwrap()
                    >= 16 * MIB
            );
            std::fs::write(root.join("first-boot-id"), boot_id).unwrap();
            std::fs::write(root.join("owned-path"), swap.path.to_str().unwrap()).unwrap();
            std::fs::write(root.join("unrelated"), b"keep this file").unwrap();
            let record = paths.runtime_dir.join("emergency-absorber.json");
            let on_disk: serde_json::Value =
                serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
            assert_eq!(on_disk["boot_id"], boot_id);
            println!("KTEST-ABSORBER-ACTIVATED={boot_id}");
        }
        "observe" => {
            assert_eq!(
                std::fs::read_to_string(root.join("first-boot-id")).unwrap(),
                boot_id
            );
            let swaps = crate::cascade::read_swaps().unwrap();
            let restored = retained(&paths, &swaps, false).unwrap().unwrap();
            let row = swaps
                .iter()
                .find(|entry| Path::new(&entry.bare_path()) == restored.path)
                .unwrap();
            assert!(
                row.used_kb > 0,
                "the page probe must still occupy kernel swap"
            );
            assert!(restored.free_bytes(&swaps).is_ok());
            println!("KTEST-ABSORBER-SAME-BOOT-OCCUPIED-KIB={}", row.used_kb);
        }
        "retire" => {
            let prior = std::fs::read_to_string(root.join("first-boot-id")).unwrap();
            assert_ne!(prior, boot_id);
            let path = PathBuf::from(std::fs::read_to_string(root.join("owned-path")).unwrap());
            let store = AbsorberRecordStore::open(&paths).unwrap();
            assert!(
                store.load().is_err(),
                "first-boot receipt must withhold live credit"
            );
            assert!(path.is_file());
            let swaps = crate::cascade::read_swaps().unwrap();
            assert!(
                !swaps
                    .iter()
                    .any(|entry| Path::new(&entry.bare_path()) == path)
            );
            assert!(retained(&paths, &swaps, false).unwrap().is_none());
            assert!(path.is_file());

            let path_text = path.to_str().unwrap();
            let runner = SystemCommandRunner;
            // An administrator can reactivate the old file in the new kernel.
            // The product must withhold cleanup and leave that kernel row alone.
            runner.run("swapon", &["-p", "0", path_text]).unwrap();
            let reactivated = crate::cascade::read_swaps().unwrap();
            assert!(
                reactivated
                    .iter()
                    .any(|entry| entry.bare_path() == path_text)
            );
            assert!(retained(&paths, &reactivated, true).is_err());
            assert!(path.is_file());
            assert!(store.load_any_boot().unwrap().is_some());
            assert!(
                crate::cascade::read_swaps()
                    .unwrap()
                    .iter()
                    .any(|entry| entry.bare_path() == path_text)
            );
            println!("KTEST-ABSORBER-PRIOR-BOOT-ACTIVE-REFUSED=pass");
            // The disposable fixture owns this deliberately empty swap row.
            runner.run("swapoff", &["--", path_text]).unwrap();
            let swaps = crate::cascade::read_swaps().unwrap();
            assert!(retained(&paths, &swaps, true).unwrap().is_none());
            assert!(retained(&paths, &swaps, true).unwrap().is_none());
            assert!(store.load_any_boot().unwrap().is_none());
            assert!(!path.exists());
            assert_eq!(
                std::fs::read(root.join("unrelated")).unwrap(),
                b"keep this file"
            );
            println!("KTEST-ABSORBER-PRIOR-BOOT-CLEANED={prior}->{boot_id}");
        }
        "stage-absent" => {
            std::fs::create_dir(&paths.runtime_dir).unwrap();
            let store = AbsorberRecordStore::open(&paths).unwrap();
            let swap = create_in_before_activation(
                &SystemCommandRunner,
                16 * MIB,
                std::slice::from_ref(&root),
                &linux_free_bytes,
                &|backing| store.publish(backing),
            )
            .expect("activate the disposable guest's private swap file");
            assert!(
                swap.free_bytes(&crate::cascade::read_swaps().unwrap())
                    .is_ok()
            );
            store.mark_active_observed(swap.backing()).unwrap();
            std::fs::write(root.join("absent-boot-id"), boot_id).unwrap();
            std::fs::write(root.join("owned-path"), swap.path.to_str().unwrap()).unwrap();
            std::fs::write(root.join("unrelated"), b"keep this file").unwrap();
            assert!(swap.remove(&SystemCommandRunner));
            assert!(!swap.path.exists());
            assert!(store.load().unwrap().is_some());
            println!("KTEST-ABSORBER-ABSENCE-STAGED={boot_id}");
        }
        "retire-absent" => {
            let prior = std::fs::read_to_string(root.join("absent-boot-id")).unwrap();
            assert_ne!(prior, boot_id);
            let path = PathBuf::from(std::fs::read_to_string(root.join("owned-path")).unwrap());
            let store = AbsorberRecordStore::open(&paths).unwrap();
            let swaps = crate::cascade::read_swaps().unwrap();
            assert!(!path.exists());
            assert!(retained(&paths, &swaps, false).unwrap().is_none());
            assert!(store.load_any_boot().unwrap().is_some());
            assert!(retained(&paths, &swaps, true).unwrap().is_none());
            assert!(store.load_any_boot().unwrap().is_none());
            assert!(retained(&paths, &swaps, true).unwrap().is_none());
            assert_eq!(
                std::fs::read(root.join("unrelated")).unwrap(),
                b"keep this file"
            );
            println!("KTEST-ABSORBER-ABSENCE-CLEARED={prior}->{boot_id}");
        }
        _ => panic!("unknown absorber fixture phase {phase}"),
    });
}
