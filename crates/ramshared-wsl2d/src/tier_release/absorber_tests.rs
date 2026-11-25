// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/tier_release/absorber_tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;

fn create_emergency_swap_in(
    bytes: u64,
    candidates: &[PathBuf],
    free_space: &dyn Fn(&Path) -> u64,
    run: &mut impl FnMut(&str, &[&str]) -> bool,
) -> Option<SwapAbsorber> {
    struct Creator<'a, F> {
        roots: &'a [PathBuf],
        free: &'a dyn Fn(&Path) -> u64,
        run: &'a mut F,
        retained: Option<SwapAbsorber>,
    }
    impl<F: FnMut(&str, &[&str]) -> bool> ReleaseIo for Creator<'_, F> {
        fn drain_allowed(&self) -> bool {
            true
        }
        fn read_proc(&mut self, _: &str) -> Option<String> {
            panic!("creator does not observe procfs")
        }
        fn run(&mut self, command: &str, args: &[&str], _: Instant) -> HelperOutcome {
            if (self.run)(command, args) {
                HelperOutcome::Success
            } else {
                HelperOutcome::Failed
            }
        }
        fn wait_for_cleanup_memory(&mut self) {}
        fn retain(&mut self, backing: SwapAbsorber) {
            self.retained = Some(backing);
        }
        fn candidates(&self) -> Vec<PathBuf> {
            self.roots.to_vec()
        }
        fn free_space(&self, root: &Path) -> u64 {
            (self.free)(root)
        }
    }
    create_emergency_swap(
        bytes,
        Instant::now() + Duration::from_secs(2),
        &mut Creator {
            roots: candidates,
            free: free_space,
            run,
            retained: None,
        },
    )
}

#[test]
fn release_budget_does_not_credit_generic_fallback() {
    let memory = "MemTotal: 16777216 kB\nMemAvailable: 3145728 kB\n";
    let swaps = "Filename Type Size Used Priority\n/dev/nbd0 partition 4194304 2097152 100\n/dev/fallback partition 8388608 0 -2\n";
    let observed = super::parse_drain_absorbers(memory, swaps, "/dev/nbd0").unwrap();
    assert!(!observed.decision(None).unwrap().is_safe());
}

#[test]
fn release_budget_credits_only_the_live_owned_absorber() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-daemon-owned-credit-{}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let backing = SwapAbsorber::reserve(&root.join("owned.swap")).unwrap();
    backing.file().set_len(2 * 1024 * 1024 * 1024).unwrap();
    let memory = "MemTotal: 16777216 kB\nMemAvailable: 3145728 kB\n";
    let swaps = format!(
        "Filename Type Size Used Priority\n/dev/nbd0 partition 4194304 2097152 100\n{} file 2097152 0 0\n",
        backing.path().display()
    );
    let observed = parse_drain_absorbers(memory, &swaps, "/dev/nbd0").unwrap();
    assert!(!observed.decision(None).unwrap().is_safe());
    assert!(observed.decision(Some(&backing)).unwrap().is_safe());
    std::fs::rename(backing.path(), root.join("old.swap")).unwrap();
    std::fs::write(backing.path(), b"foreign").unwrap();
    assert!(observed.decision(Some(&backing)).is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn emergency_swap_candidates_never_start_on_c() {
    let candidates = emergency_swap_candidates();
    assert_eq!(candidates[0], std::path::PathBuf::from("/"));
    assert!(
        !candidates
            .iter()
            .any(|p| p == std::path::Path::new("/mnt/c"))
    );
}

fn cleanup_fixture(
    outcome: HelperOutcome,
    absent: Option<bool>,
) -> (super::release_tests::ReleaseFixture, SwapAbsorber) {
    use super::release_tests::{ROOMY, ReleaseFixture};
    let mut io = ReleaseFixture::new();
    let backing = SwapAbsorber::reserve(&io.backing_path()).unwrap();
    backing.file().set_len(2 * 1024 * 1024 * 1024).unwrap();
    io.frame(ROOMY, false, Some(0));
    io.outcomes.push_back(outcome);
    if let Some(absent) = absent {
        if absent {
            io.swaps(false, None);
        } else {
            io.reads.push_back(("/proc/swaps", None));
        }
    }
    (io, backing)
}

#[test]
fn emergency_cleanup_preserves_backing_when_swapoff_fails() {
    let (mut io, backing) = cleanup_fixture(HelperOutcome::Failed, None);
    assert_eq!(
        remove_emergency_swap(&backing, Instant::now() + Duration::from_secs(1), &mut io),
        CleanupOutcome::Retained
    );
    backing.validate().unwrap();
}

#[test]
fn emergency_cleanup_retains_backing_when_host_memory_is_unverified() {
    let (mut io, backing) = cleanup_fixture(HelperOutcome::Success, Some(true));
    io.drain_allowed = false;
    assert_eq!(
        remove_emergency_swap(&backing, Instant::now() + Duration::from_secs(1), &mut io),
        CleanupOutcome::HeadroomBlocked
    );
    assert!(io.commands.is_empty());
    backing.validate().unwrap();
}

#[test]
fn emergency_cleanup_preserves_backing_when_absence_is_unproven() {
    let (mut io, backing) = cleanup_fixture(HelperOutcome::Success, Some(false));
    assert_eq!(
        remove_emergency_swap(&backing, Instant::now() + Duration::from_secs(1), &mut io),
        CleanupOutcome::Retained
    );
    assert!(backing.path().exists());
}

#[test]
fn emergency_cleanup_unlinks_only_after_verified_release() {
    let (mut io, backing) = cleanup_fixture(HelperOutcome::Success, Some(true));
    assert_eq!(
        remove_emergency_swap(&backing, Instant::now() + Duration::from_secs(1), &mut io),
        CleanupOutcome::Released
    );
    assert!(!backing.path().exists());
}

#[test]
fn emergency_creation_preserves_existing_probe_files_and_symlink_targets() {
    for symlink in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "ramshared-daemon-probe-custody-{}-{symlink}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("existing-data");
        let probe = root.join(".ramshared-emergency-swap-probe");
        std::fs::write(&target, b"unrelated data").unwrap();
        if symlink {
            std::os::unix::fs::symlink(&target, &probe).unwrap();
        } else {
            std::fs::write(&probe, b"existing probe data").unwrap();
        }
        let swap = create_emergency_swap_in(
            1024,
            std::slice::from_ref(&root),
            &|_| 1024 * 1024 * 1024,
            &mut |_, _| true,
        );
        let preserved = if symlink {
            std::fs::read_link(&probe).ok() == Some(target.clone())
        } else {
            std::fs::read(&probe).ok().as_deref() == Some(b"existing probe data")
        };
        let target_bytes = std::fs::read(&target).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(swap.is_some());
        assert!(
            preserved,
            "the creator cannot delete or truncate a probe path"
        );
        assert_eq!(target_bytes, b"unrelated data");
    }
}

#[test]
fn emergency_creation_cleans_partial_failed_allocation() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-partial-allocation-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let result = create_emergency_swap_in(
        1024,
        std::slice::from_ref(&root),
        &|_| 1024 * 1024 * 1024,
        &mut |command, args| {
            let path = if command == "fallocate" {
                args[2]
            } else {
                args[1].strip_prefix("of=").unwrap()
            };
            std::fs::write(path, b"partial allocation").unwrap();
            false
        },
    );
    assert!(result.is_none());
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "failed allocations must not leak backing"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn emergency_creation_protects_backing_before_formatting() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "ramshared-backing-permissions-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let mut observed_mode = None;
    let result = create_emergency_swap_in(
        1024,
        std::slice::from_ref(&root),
        &|_| 1024 * 1024 * 1024,
        &mut |command, args| {
            if command == "fallocate" {
                std::fs::write(args[2], b"owned backing").unwrap();
                assert_eq!(
                    std::fs::metadata(args[2]).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            } else if command == "mkswap" {
                observed_mode =
                    Some(std::fs::metadata(args[0]).unwrap().permissions().mode() & 0o777);
            }
            true
        },
    )
    .unwrap();
    assert_eq!(observed_mode, Some(0o600));
    std::fs::remove_file(result.path()).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn emergency_creation_preserves_preexisting_backing() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-backing-collision-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!(".ramshared-emergency-{}.swap", std::process::id()));
    std::fs::write(&path, b"existing backing must survive").unwrap();
    let mut calls = 0;
    let result = create_emergency_swap_in(
        1024,
        std::slice::from_ref(&root),
        &|_| 1024 * 1024 * 1024,
        &mut |_, _| {
            calls += 1;
            true
        },
    );
    assert!(result.is_none());
    assert_eq!(calls, 0);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"existing backing must survive"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn emergency_creation_retains_backing_after_uncertain_activation() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-activation-uncertain-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let result = create_emergency_swap_in(
        1024,
        std::slice::from_ref(&root),
        &|_| 1024 * 1024 * 1024,
        &mut |_, _| false,
    );
    assert!(result.is_none());
    // First prove failed allocation cleanup, then simulate helpers whose
    // activation status cannot establish whether the kernel armed it.
    let result = create_emergency_swap_in(
        1024,
        std::slice::from_ref(&root),
        &|_| 1024 * 1024 * 1024,
        &mut |command, _| command != "swapon",
    );
    assert!(result.is_none());
    let path = root.join(format!(".ramshared-emergency-{}.swap", std::process::id()));
    assert!(
        path.exists(),
        "an uncertain activation must retain its backing"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn emergency_creation_uses_dd_fallback_and_orders_activation() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-allocation-fallback-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let mut calls = Vec::new();
    let result = create_emergency_swap_in(
        64 * 1024 * 1024,
        &[root.join("missing"), root.clone()],
        &|_| 1024 * 1024 * 1024,
        &mut |command, args| {
            calls.push((
                command.to_string(),
                args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
            ));
            command != "fallocate"
        },
    )
    .unwrap();
    assert_eq!(
        calls.iter().map(|call| call.0.as_str()).collect::<Vec<_>>(),
        ["fallocate", "dd", "mkswap", "swapon"]
    );
    assert_eq!(calls[1].1[3], "count=128");
    assert_eq!(
        calls[3].1,
        ["-p", "0", result.effect_path().to_str().unwrap()]
    );
    assert!(result.path().starts_with(&root));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn emergency_creation_refuses_insufficient_space_without_helpers() {
    let root =
        std::env::temp_dir().join(format!("ramshared-allocation-space-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for free in [0, 64 * 1024 * 1024] {
        assert!(
            create_emergency_swap_in(
                0,
                std::slice::from_ref(&root),
                &|_| free,
                &mut |_, _| panic!("no space must not execute a helper")
            )
            .is_none()
        );
    }
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn emergency_creation_requires_successful_format_before_activation() {
    let root =
        std::env::temp_dir().join(format!("ramshared-format-refusal-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut calls = Vec::new();
    let result = create_emergency_swap_in(
        1024,
        std::slice::from_ref(&root),
        &|_| 1024 * 1024 * 1024,
        &mut |command, args| {
            calls.push(command.to_string());
            if command == "fallocate" {
                std::fs::write(args[2], b"owned backing").unwrap();
            }
            command != "mkswap"
        },
    );
    assert!(result.is_none());
    assert_eq!(calls, ["fallocate", "mkswap"]);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn drain_observation_requires_the_exact_target_and_complete_counters() {
    let mem = "MemTotal: 8192 kB\nMemAvailable: 4096 kB\n";
    let swaps = "Filename Type Size Used Priority\n/dev/nbd0 partition 2048 1024 50\n/dev/sdc partition 8192 4096 -2\n";
    assert_eq!(
        parse_drain_absorbers(mem, swaps, "/dev/nbd0").map(|d| (
            d.memory.available_bytes,
            d.rows[1].1 - d.rows[1].2,
            d.target_used
        )),
        Some((4096 * 1024, 4096 * 1024, 1024 * 1024))
    );
    assert_eq!(parse_drain_absorbers(mem, swaps, "/dev/missing"), None);
    assert_eq!(parse_drain_absorbers("", swaps, "/dev/nbd0"), None);
    assert_eq!(
        parse_drain_absorbers("MemAvailable: 4096 MB\n", swaps, "/dev/nbd0"),
        None
    );
    assert_eq!(
        parse_drain_absorbers(
            "MemAvailable: 4096 kB\nMemAvailable: 1 kB\n",
            swaps,
            "/dev/nbd0"
        ),
        None
    );
    assert_eq!(
        parse_drain_absorbers(
            "MemAvailable: 18446744073709551615 kB\n",
            swaps,
            "/dev/nbd0"
        ),
        None
    );
    assert_eq!(parse_drain_absorbers(mem, "malformed", "/dev/nbd0"), None);
    assert_eq!(
        parse_drain_absorbers(
            mem,
            "Filename Type Size Used Priority\n/dev/nbd0 partition 10 20 50\n",
            "/dev/nbd0"
        ),
        None
    );
    assert_eq!(
        parse_drain_absorbers(
            mem,
            &format!("{swaps}/dev/nbd0 partition 2048 0 50\n"),
            "/dev/nbd0"
        ),
        None
    );
}

#[test]
fn free_bytes_is_zero_for_a_missing_mount() {
    assert_eq!(
        free_bytes(std::path::Path::new("/mnt/nonexistent-ramshared")),
        0
    );
}

#[test]
fn emergency_backing_rounds_up_and_keeps_space_reserve() {
    const MIB: u64 = 1024 * 1024;
    let root =
        std::env::temp_dir().join(format!("ramshared-absorber-round-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    assert!(
        create_emergency_swap_in(
            u64::MAX,
            std::slice::from_ref(&root),
            &|_| u64::MAX,
            &mut |_, _| panic!("overflow must refuse")
        )
        .is_none()
    );
    assert!(
        create_emergency_swap_in(
            1,
            std::slice::from_ref(&root),
            &|_| 129 * MIB - 1,
            &mut |_, _| panic!("insufficient space")
        )
        .is_none()
    );
    let mut calls = Vec::new();
    let backing = create_emergency_swap_in(
        1,
        std::slice::from_ref(&root),
        &|_| 129 * MIB,
        &mut |command, args| {
            calls.push((
                command.to_string(),
                args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
            ));
            command != "fallocate"
        },
    )
    .unwrap();
    assert_eq!(calls[0].1[1], (65 * MIB).to_string());
    assert_eq!(calls[1].1[3], "count=65");
    drop(backing);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn emergency_dd_fallback_writes_full_rounded_extent_on_real_filesystem() {
    use std::process::{Command, Stdio};
    const MIB: u64 = 1024 * 1024;
    let root =
        std::env::temp_dir().join(format!("ramshared-absorber-real-dd-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let backing = create_emergency_swap_in(
        1,
        std::slice::from_ref(&root),
        &|_| free_bytes(&root),
        &mut |command, args| match command {
            "fallocate" => false,
            "dd" => Command::new("/usr/bin/dd")
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success()),
            "mkswap" | "swapon" => true,
            _ => false,
        },
    )
    .unwrap();
    assert_eq!(backing.file().metadata().unwrap().len(), 65 * MIB);
    backing.remove_inactive(|| true).unwrap();
    std::fs::remove_dir(root).unwrap();
}
