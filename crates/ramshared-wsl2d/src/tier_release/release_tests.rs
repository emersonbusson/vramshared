// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/tier_release/release_tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub(super) const HEADER: &str = "Filename Type Size Used Priority\n";
pub(super) const ROOMY: &str = "MemTotal: 16777216 kB\nMemAvailable: 8388608 kB\n";
pub(super) const LIMITED: &str = "MemTotal: 16777216 kB\nMemAvailable: 3145728 kB\n";
const LOW: &str = "MemTotal: 16777216 kB\nMemAvailable: 1048576 kB\n";
const TARGET: &str = "/dev/nbd0 partition 4194304 2097152 100\n";
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[test]
fn routine_release_refuses_unverified_host_before_any_effect() {
    let mut fixture = ReleaseFixture::new();
    fixture.drain_allowed = false;
    fixture.frame(ROOMY, true, None);
    assert!(!fixture.release());
    assert!(fixture.commands.is_empty());
    assert_eq!(fixture.reads.len(), 2);
}

#[test]
fn host_storage_observation_refuses_source_removed_or_replaced_during_read() {
    use std::sync::{Arc, Mutex, mpsc};

    let _serial = observation::TEST_HOST_MEMORY_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for replace in [false, true] {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let source = register_routine_host_storage(move |_, guest| {
            entered_tx.send(()).ok()?;
            release_rx
                .lock()
                .ok()?
                .recv_timeout(Duration::from_secs(10))
                .ok()?;
            Some(guest)
        });
        let reader = std::thread::spawn(|| {
            observation::observed_routine_host_storage_free(Path::new("/"), 1_024)
        });
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let replacement = replace.then(|| register_routine_host_storage(|_, _| Some(512)));
        drop(source);
        release_tx.send(()).unwrap();
        assert_eq!(reader.join().unwrap(), None);
        drop(replacement);
    }
}

#[test]
fn wsl_release_refuses_guest_space_after_host_source_disappears() {
    let _serial = observation::TEST_HOST_MEMORY_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !matches!(
        ramshared_block::platform::probe_platform().platform,
        Some(ramshared_block::platform::PlatformKind::Wsl2)
    ) {
        return;
    }
    let fixture = ReleaseFixture::new();
    let mut custody = ReleaseCustody::default();
    let adapter = SystemRelease {
        custody: &mut custody,
        mode: ReleaseMode::Routine,
    };
    let host_source = register_routine_host_storage(|_, guest| Some(guest));
    assert!(!adapter.candidates().is_empty());
    assert!(adapter.free_space(&fixture.root) > 0);
    drop(host_source);
    assert_eq!(adapter.free_space(&fixture.root), 0);
}

#[test]
fn routine_scope_requires_native_but_fatal_attempt_remains_best_effort() {
    use ramshared_block::platform::PlatformKind;
    for platform in [None, Some(PlatformKind::Wsl1), Some(PlatformKind::Wsl2)] {
        assert!(!drain_allowed(ReleaseMode::Routine, platform));
        assert!(drain_allowed(ReleaseMode::LastResort, platform));
    }
    assert!(drain_allowed(
        ReleaseMode::Routine,
        Some(PlatformKind::NativeLinux)
    ));
}

#[test]
fn live_wsl_system_adapter_refuses_routine_drain_without_effects() {
    let _serial = observation::TEST_HOST_MEMORY_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !matches!(
        ramshared_block::platform::probe_platform().platform,
        Some(
            ramshared_block::platform::PlatformKind::Wsl1
                | ramshared_block::platform::PlatformKind::Wsl2
        )
    ) {
        return;
    }
    let mut custody = ReleaseCustody::default();
    let adapter = SystemRelease {
        custody: &mut custody,
        mode: ReleaseMode::Routine,
    };
    assert!(!adapter.drain_allowed());
    assert!(custody.child.is_none());
    assert!(custody.absorber.is_none());
}

#[test]
fn wsl_routine_tier_release_admits_authenticated_host_memory_and_refuses_insufficient_host() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let _serial = observation::TEST_HOST_MEMORY_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let roomy_host = ramshared_tier::HostDrainMemory {
        total_bytes: 32 * GIB,
        available_bytes: 6 * GIB,
        commit_total_bytes: 40 * GIB,
        commit_limit_bytes: 56 * GIB,
    };
    let old_guard = register_routine_host_memory(|| None);
    let active_guard = register_routine_host_memory(move || Some(roomy_host));
    drop(old_guard);
    assert_eq!(
        observation::observed_routine_host_memory(),
        Some(roomy_host)
    );
    if matches!(
        ramshared_block::platform::probe_platform().platform,
        Some(
            ramshared_block::platform::PlatformKind::Wsl1
                | ramshared_block::platform::PlatformKind::Wsl2
        )
    ) {
        let mut custody = ReleaseCustody::default();
        let adapter = SystemRelease {
            custody: &mut custody,
            mode: ReleaseMode::Routine,
        };
        assert!(adapter.drain_allowed());
        assert_eq!(adapter.wsl_host_memory(), Some(Some(roomy_host)));
    }
    drop(active_guard);
    assert_eq!(observation::observed_routine_host_memory(), None);

    let mut allowed = ReleaseFixture::new();
    allowed.wsl_host = Some(Some(roomy_host));
    allowed.frame(
        "MemTotal: 16777216 kB\nMemFree: 1048576 kB\nMemAvailable: 8388608 kB\n",
        true,
        None,
    );
    allowed.outcomes.push_back(HelperOutcome::Success);
    allowed.swaps(false, None);
    assert!(allowed.release());
    assert_eq!(allowed.commands, ["swapoff"]);

    let mut refused = ReleaseFixture::new();
    refused.wsl_host = Some(Some(ramshared_tier::HostDrainMemory {
        available_bytes: 2 * GIB,
        ..roomy_host
    }));
    refused.frame(
        "MemTotal: 16777216 kB\nMemFree: 1048576 kB\nMemAvailable: 8388608 kB\n",
        true,
        None,
    );
    assert!(!refused.release());
    assert!(refused.commands.is_empty());
}

pub(super) struct ReleaseFixture {
    pub(super) drain_allowed: bool,
    pub(super) wsl_host: Option<Option<ramshared_tier::HostDrainMemory>>,
    pub(super) root: PathBuf,
    pub(super) reads: VecDeque<(&'static str, Option<String>)>,
    pub(super) outcomes: VecDeque<HelperOutcome>,
    pub(super) commands: Vec<String>,
    pub(super) retained: Option<SwapAbsorber>,
    deny_unlink: bool,
    pub(super) cleanup_waits_for_memory: bool,
}

impl ReleaseFixture {
    pub(super) fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ramshared-release-journey-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self {
            drain_allowed: true,
            wsl_host: None,
            root,
            reads: VecDeque::new(),
            outcomes: VecDeque::new(),
            commands: Vec::new(),
            retained: None,
            deny_unlink: false,
            cleanup_waits_for_memory: false,
        }
    }

    pub(super) fn backing_path(&self) -> PathBuf {
        self.root
            .join(format!(".ramshared-emergency-{}.swap", std::process::id()))
    }

    pub(super) fn swaps(&mut self, target: bool, absorber_used_kib: Option<u64>) {
        let absorber = absorber_used_kib.map_or_else(String::new, |used| {
            format!("{} file 2097152 {used} 0\n", self.backing_path().display())
        });
        self.reads.push_back((
            "/proc/swaps",
            Some(format!(
                "{HEADER}{}{absorber}",
                if target { TARGET } else { "" }
            )),
        ));
    }

    pub(super) fn frame(&mut self, memory: &str, target: bool, absorber_used_kib: Option<u64>) {
        self.reads
            .push_back(("/proc/meminfo", Some(memory.to_string())));
        self.swaps(target, absorber_used_kib);
    }

    pub(super) fn release(&mut self) -> bool {
        release_managed_tier_with("/dev/nbd0", Instant::now() + Duration::from_secs(2), self)
            == ReleaseOutcome::Released
    }
}

impl ReleaseIo for ReleaseFixture {
    fn drain_allowed(&self) -> bool {
        self.drain_allowed
    }
    fn wsl_host_memory(&self) -> Option<Option<ramshared_tier::HostDrainMemory>> {
        self.wsl_host
    }
    fn read_proc(&mut self, path: &str) -> Option<String> {
        let (expected, value) = self.reads.pop_front().expect("unexpected observation");
        assert_eq!(expected, path);
        value
    }

    fn run(&mut self, command: &str, args: &[&str], deadline: Instant) -> HelperOutcome {
        assert!(Instant::now() < deadline, "no effect after deadline");
        self.commands.push(command.to_string());
        let outcome = self.outcomes.pop_front().unwrap_or(HelperOutcome::Success);
        if command == "fallocate" && outcome == HelperOutcome::Success {
            std::fs::OpenOptions::new()
                .write(true)
                .open(args[2])
                .unwrap()
                .set_len(2 * 1024 * 1024 * 1024)
                .unwrap();
        }
        outcome
    }

    fn retain(&mut self, backing: SwapAbsorber) {
        assert!(self.retained.is_none(), "custody cannot be overwritten");
        self.retained = Some(backing);
    }

    fn wait_for_cleanup_memory(&mut self) {
        self.cleanup_waits_for_memory = true;
    }

    fn remove_unactivated(&mut self, backing: &SwapAbsorber) -> bool {
        !self.deny_unlink && backing.remove_inactive(|| true).is_ok()
    }

    fn candidates(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
    fn free_space(&self, _: &Path) -> u64 {
        8 * 1024 * 1024 * 1024
    }
}

impl Drop for ReleaseFixture {
    fn drop(&mut self) {
        // The fixture never arms real swap, so removing its files is safe.
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
#[test]
fn release_creator_retains_capability_when_inactive_unlink_fails() {
    let mut io = ReleaseFixture::new();
    io.deny_unlink = true;
    io.outcomes
        .extend([HelperOutcome::Failed, HelperOutcome::Failed]);
    assert!(
        create_emergency_swap(1024, Instant::now() + Duration::from_secs(1), &mut io).is_none()
    );
    assert_eq!(io.commands, ["fallocate", "dd"]);
    io.retained
        .as_ref()
        .expect("failed unlink must retain descriptor custody")
        .validate()
        .unwrap();
}

#[test]
fn release_orchestration_drains_and_verifies_the_target() {
    let mut io = ReleaseFixture::new();
    io.frame(ROOMY, true, None);
    io.swaps(false, None);
    assert!(io.release());
    assert_eq!(io.commands, ["swapoff"]);
    assert!(io.reads.is_empty());
}

#[test]
fn release_orchestration_creates_reobserves_and_cleans_owned_absorber() {
    let mut io = ReleaseFixture::new();
    io.frame(LIMITED, true, None);
    io.frame(LIMITED, true, Some(0));
    io.swaps(false, Some(0));
    io.frame(ROOMY, false, Some(0));
    io.swaps(false, None);
    assert!(io.release());
    assert_eq!(
        io.commands,
        ["fallocate", "mkswap", "swapon", "swapoff", "swapoff"]
    );
    assert!(!io.backing_path().exists());
    assert!(io.retained.is_none());
    assert!(io.reads.is_empty());
}

#[test]
fn release_orchestration_refuses_resident_shortage_and_missing_observations() {
    for (memory, swaps) in [
        (None, None),
        (Some("malformed"), Some(HEADER.to_string())),
        (Some(ROOMY), None),
        (Some(ROOMY), Some("malformed".to_string())),
        (Some(ROOMY), Some(HEADER.to_string())),
        (Some(LOW), Some(format!("{HEADER}{TARGET}"))),
    ] {
        let mut io = ReleaseFixture::new();
        io.reads
            .push_back(("/proc/meminfo", memory.map(str::to_string)));
        io.reads.push_back(("/proc/swaps", swaps));
        assert!(!io.release());
        assert!(io.commands.is_empty());
    }
}

#[test]
fn release_orchestration_retains_absorber_on_uncertain_or_failed_drain() {
    for outcome in [HelperOutcome::Failed, HelperOutcome::Uncertain] {
        let mut io = ReleaseFixture::new();
        io.frame(LIMITED, true, None);
        io.frame(LIMITED, true, Some(0));
        io.outcomes.extend([
            HelperOutcome::Success,
            HelperOutcome::Success,
            HelperOutcome::Success,
            outcome,
        ]);
        assert!(!io.release());
        assert_eq!(io.commands, ["fallocate", "mkswap", "swapon", "swapoff"]);
        io.retained.as_ref().unwrap().validate().unwrap();
        assert_eq!(io.retained.as_ref().unwrap().path(), io.backing_path());
    }
}

#[test]
fn release_orchestration_requires_fresh_capacity_and_absence() {
    for scenario in [
        "missing_memory",
        "missing_absorber",
        "reserve",
        "still_active",
        "malformed_after",
    ] {
        let mut io = ReleaseFixture::new();
        io.frame(LIMITED, true, None);
        match scenario {
            "missing_memory" => io.reads.push_back(("/proc/meminfo", None)),
            "missing_absorber" => io.frame(LIMITED, true, None),
            "reserve" => io.frame(LOW, true, Some(0)),
            _ => {
                io.frame(LIMITED, true, Some(0));
                if scenario == "still_active" {
                    io.swaps(true, Some(0));
                } else {
                    io.reads
                        .push_back(("/proc/swaps", Some("malformed".to_string())));
                }
            }
        }
        assert!(!io.release(), "{scenario}");
        io.retained.as_ref().unwrap().validate().unwrap();
        let expected = if matches!(scenario, "still_active" | "malformed_after") {
            4
        } else {
            3
        };
        assert_eq!(io.commands.len(), expected, "{scenario}");
    }
}

#[test]
fn release_cleanup_retains_occupied_absorber_without_resident_capacity() {
    let mut io = ReleaseFixture::new();
    io.frame(LIMITED, true, None);
    io.frame(LIMITED, true, Some(0));
    io.swaps(false, Some(1048576));
    io.frame(LIMITED, false, Some(1048576));
    assert!(
        !io.release(),
        "active cleanup cannot report complete release"
    );
    assert_eq!(io.commands, ["fallocate", "mkswap", "swapon", "swapoff"]);
    io.retained.as_ref().unwrap().validate().unwrap();
}

#[test]
fn release_budget_refuses_absorber_self_credit() {
    let io = ReleaseFixture::new();
    let backing = SwapAbsorber::reserve(&io.backing_path()).unwrap();
    backing.file().set_len(4 * 1024 * 1024 * 1024).unwrap();
    let swaps = format!(
        "{HEADER}{} file 4194304 2097152 0\n",
        backing.path().display()
    );
    let observation =
        parse_drain_absorbers(LIMITED, &swaps, backing.path().to_str().unwrap()).unwrap();
    assert!(observation.decision(Some(&backing)).is_none());
}

#[test]
fn release_helper_deadline_retains_unobserved_child() {
    let mut custody = ReleaseCustody::default();
    let mut io = SystemRelease {
        custody: &mut custody,
        mode: ReleaseMode::Routine,
    };
    assert_eq!(
        io.run(
            "/usr/bin/sleep",
            &["0.08"],
            Instant::now() + Duration::from_millis(2)
        ),
        HelperOutcome::Uncertain
    );
    let child = custody
        .child
        .as_mut()
        .expect("pending helper must remain owned");
    assert_eq!(
        observe_helper_until(child, Instant::now() + Duration::from_secs(1)),
        HelperOutcome::Success
    );
    custody.child = None;
    let mut io = SystemRelease {
        custody: &mut custody,
        mode: ReleaseMode::Routine,
    };
    assert_eq!(
        io.run(
            "/usr/bin/true",
            &[],
            Instant::now() - Duration::from_millis(1)
        ),
        HelperOutcome::NotStarted
    );
    assert!(
        custody.child.is_none(),
        "expired admission must not create a child"
    );
}

#[test]
fn release_custody_refuses_concurrent_and_unreconciled_use() {
    let slot = std::sync::Mutex::new(CustodyState::Idle);
    let mut first = ReleaseLease::claim(&slot).unwrap();
    assert!(ReleaseLease::claim(&slot).is_none());
    // Ordinary child fixture; no swap helper or real swap is involved.
    first.custody.child = Some(
        std::process::Command::new("/usr/bin/sleep")
            .arg("0.08")
            .spawn()
            .unwrap(),
    );
    drop(first);
    let mut resumed = ReleaseLease::claim(&slot).unwrap();
    assert!(!reconcile_custody(&mut resumed.custody, &mut |_| panic!(
        "live child blocks all filesystem reconciliation"
    )));
    assert!(ReleaseLease::claim(&slot).is_none());
    assert_eq!(
        observe_helper_until(
            resumed.custody.child.as_mut().unwrap(),
            Instant::now() + Duration::from_secs(1)
        ),
        HelperOutcome::Success
    );
    assert!(reconcile_custody(&mut resumed.custody, &mut |_| panic!(
        "no backing to observe"
    )));
    assert!(resumed.custody.child.is_none());
    drop(resumed);
    assert!(ReleaseLease::claim(&slot).is_some());
}

#[test]
fn release_creator_retains_pending_allocation_format_and_activation() {
    for successes in 0..3 {
        let mut io = ReleaseFixture::new();
        io.outcomes
            .extend(std::iter::repeat_n(HelperOutcome::Success, successes));
        io.outcomes.push_back(HelperOutcome::Uncertain);
        assert!(
            create_emergency_swap(1024, Instant::now() + Duration::from_secs(1), &mut io).is_none()
        );
        assert_eq!(
            io.commands.len(),
            successes + 1,
            "a pending helper prevents every later effect"
        );
        io.retained.as_ref().unwrap().validate().unwrap();
    }
}

#[test]
fn release_orchestration_preserves_failed_or_unobserved_cleanup() {
    for outcome in [
        HelperOutcome::Failed,
        HelperOutcome::Uncertain,
        HelperOutcome::Success,
    ] {
        let mut io = ReleaseFixture::new();
        io.frame(LIMITED, true, None);
        io.frame(LIMITED, true, Some(0));
        io.swaps(false, Some(0));
        io.frame(ROOMY, false, Some(0));
        if outcome == HelperOutcome::Success {
            io.reads.push_back(("/proc/swaps", None));
        }
        io.outcomes.extend([
            HelperOutcome::Success,
            HelperOutcome::Success,
            HelperOutcome::Success,
            HelperOutcome::Success,
            outcome,
        ]);
        assert!(!io.release(), "failed or unobserved cleanup stays pending");
        assert_eq!(io.commands.len(), 5);
        io.retained.as_ref().unwrap().validate().unwrap();
    }
}

#[test]
fn release_orchestration_refuses_backing_collision_and_expired_effects() {
    let mut io = ReleaseFixture::new();
    std::fs::write(io.backing_path(), b"unrelated file").unwrap();
    io.frame(LIMITED, true, None);
    assert!(!io.release());
    assert_eq!(std::fs::read(io.backing_path()).unwrap(), b"unrelated file");
    let expired = Instant::now() - Duration::from_millis(1);
    assert_eq!(
        release_managed_tier_with("/dev/nbd0", expired, &mut io),
        ReleaseOutcome::Refused
    );
    assert!(create_emergency_swap(1024, expired, &mut io).is_none());
    assert!(io.commands.is_empty());
    assert!(!release_managed_tier_on_fatal(
        "/dev/ramshared-absent-test",
        Duration::MAX
    ));
}

#[test]
fn release_helper_observation_errors_remain_uncertain() {
    struct BrokenChild;
    impl ChildObservation for BrokenChild {
        fn observe(&mut self) -> std::io::Result<Option<bool>> {
            Err(std::io::Error::other("fixture cannot observe child"))
        }
    }
    assert_eq!(
        observe_helper_until(&mut BrokenChild, Instant::now()),
        HelperOutcome::Uncertain
    );
}

#[test]
fn release_helper_refuses_unclassified_bare_programs() {
    let mut custody = ReleaseCustody::default();
    let mut io = SystemRelease {
        custody: &mut custody,
        mode: ReleaseMode::Routine,
    };
    let unclassified = "true";
    assert_eq!(
        io.run(unclassified, &[], Instant::now() + Duration::from_secs(1)),
        HelperOutcome::Failed
    );
    assert!(custody.child.is_none());
}

#[test]
fn release_helpers_resolve_to_fixed_system_paths() {
    for (name, path) in [
        ("fallocate", "/usr/bin/fallocate"),
        ("dd", "/usr/bin/dd"),
        ("mkswap", "/usr/sbin/mkswap"),
        ("swapon", "/usr/sbin/swapon"),
        ("swapoff", "/usr/sbin/swapoff"),
    ] {
        assert_eq!(system_helper_path(name), Some(path));
        assert!(Path::new(path).is_absolute());
    }
    assert_eq!(system_helper_path("unclassified-helper"), None);
}

#[test]
fn release_system_adapter_observes_terminal_helpers_and_preserves_custody() {
    let _serial = observation::TEST_HOST_MEMORY_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = ReleaseFixture::new();
    let mut custody = ReleaseCustody::default();
    let mut io = SystemRelease {
        custody: &mut custody,
        mode: ReleaseMode::Routine,
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    assert_eq!(
        io.run("/usr/bin/true", &[], deadline),
        HelperOutcome::Success
    );
    assert_eq!(
        io.run("/usr/bin/false", &[], deadline),
        HelperOutcome::Failed
    );
    assert_eq!(
        io.run("/ramshared-missing-test-helper", &[], deadline),
        HelperOutcome::Failed
    );
    let input = fixture.root.join("observation");
    std::fs::write(&input, ROOMY).unwrap();
    assert_eq!(
        io.read_proc(input.to_str().unwrap()).as_deref(),
        Some(ROOMY)
    );
    std::fs::write(
        &input,
        vec![b'x'; ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES + 1],
    )
    .unwrap();
    assert!(io.read_proc(input.to_str().unwrap()).is_none());
    std::fs::write(&input, [0xff]).unwrap();
    assert!(io.read_proc(input.to_str().unwrap()).is_none());
    assert!(io.read_proc("/ramshared-missing-observation").is_none());
    assert_eq!(
        !io.candidates().is_empty(),
        ramshared_block::swap_absorber::guest_only_capacity_is_admissible(
            ramshared_block::platform::probe_platform().platform,
        )
    );
    if ramshared_block::swap_absorber::guest_only_capacity_is_admissible(
        ramshared_block::platform::probe_platform().platform,
    ) {
        assert!(io.free_space(&fixture.root) > 0);
    } else {
        assert_eq!(io.free_space(&fixture.root), 0);
    }
    io.retain(SwapAbsorber::reserve(&fixture.backing_path()).unwrap());
    assert_eq!(
        io.run("/usr/bin/true", &[], deadline),
        HelperOutcome::Uncertain
    );
    custody.absorber.as_ref().unwrap().validate().unwrap();
}

#[test]
fn release_owned_helper_refuses_changed_identity_and_expired_admission() {
    let mut io = ReleaseFixture::new();
    let backing = SwapAbsorber::reserve(&io.backing_path()).unwrap();
    let expired = Instant::now() - Duration::from_millis(1);
    assert_eq!(
        owned_helper(&backing, "fallocate", &[], expired, &mut io),
        HelperOutcome::NotStarted
    );
    std::fs::rename(backing.path(), io.root.join("original")).unwrap();
    std::fs::write(backing.path(), b"foreign").unwrap();
    assert_eq!(
        owned_helper(
            &backing,
            "fallocate",
            &[],
            Instant::now() + Duration::from_secs(1),
            &mut io
        ),
        HelperOutcome::Uncertain
    );
    assert_eq!(
        remove_emergency_swap(&backing, Instant::now() + Duration::from_secs(1), &mut io),
        CleanupOutcome::Retained
    );
    assert!(io.commands.is_empty());
    let mut custody = ReleaseCustody {
        child: None,
        absorber: Some(backing),
        ..ReleaseCustody::default()
    };
    assert!(!reconcile_custody(&mut custody, &mut |_| Some(
        HEADER.to_string()
    )));
    assert_eq!(std::fs::read(io.backing_path()).unwrap(), b"foreign");
}

#[test]
fn release_custody_reconciles_only_terminal_inactive_backing() {
    let fixture = ReleaseFixture::new();
    let backing = SwapAbsorber::reserve(&fixture.backing_path()).unwrap();
    let slot = std::sync::Mutex::new(CustodyState::Idle);
    let mut lease = ReleaseLease::claim(&slot).unwrap();
    lease.custody.absorber = Some(backing);
    drop(lease);
    for observation in [
        None,
        Some("malformed".to_string()),
        Some(format!(
            "{HEADER}{} file 0 0 0\n",
            fixture.backing_path().display()
        )),
    ] {
        let mut lease = ReleaseLease::claim(&slot).unwrap();
        assert!(!reconcile_custody(&mut lease.custody, &mut |_| observation.clone()));
        lease.custody.absorber.as_ref().unwrap().validate().unwrap();
    }
    let mut lease = ReleaseLease::claim(&slot).unwrap();
    assert!(reconcile_custody(&mut lease.custody, &mut |_| Some(
        HEADER.to_string()
    )));
    assert!(lease.custody.absorber.is_none());
    assert!(!fixture.backing_path().exists());
    drop(lease);
    assert!(ReleaseLease::claim(&slot).is_some());
}

#[test]
fn release_custody_shutdown_gate_requires_idle_ownership() {
    let fixture = ReleaseFixture::new();
    let slot = Mutex::new(CustodyState::Idle);
    assert!(custody_is_clear(&slot));
    let mut lease = ReleaseLease::claim(&slot).unwrap();
    assert!(!custody_is_clear(&slot));
    assert!(!reconcile_release_custody_in(&slot, &mut |_| panic!(
        "running slot has no observer"
    )));
    lease.custody.absorber = Some(SwapAbsorber::reserve(&fixture.backing_path()).unwrap());
    drop(lease);
    assert!(!custody_is_clear(&slot));
    assert!(!reconcile_release_custody_in(&slot, &mut |_| None));
    assert!(!custody_is_clear(&slot));
    assert!(reconcile_release_custody_in(&slot, &mut |_| Some(
        HEADER.to_string()
    )));
    assert!(custody_is_clear(&slot));
}

#[test]
fn release_shutdown_lease_excludes_release_until_backend_drop() {
    let fixture = ReleaseFixture::new();
    let slot = Mutex::new(CustodyState::Idle);
    let shutdown = claim_backend_shutdown_from(&slot).unwrap();
    assert!(ReleaseLease::claim(&slot).is_none());
    assert!(claim_backend_shutdown_from(&slot).is_none());
    assert!(!custody_is_clear(&slot));
    drop(shutdown);
    let mut release = ReleaseLease::claim(&slot).unwrap();
    release.custody.absorber = Some(SwapAbsorber::reserve(&fixture.backing_path()).unwrap());
    drop(release);
    assert!(claim_backend_shutdown_from(&slot).is_none());
    assert!(reconcile_release_custody_in(&slot, &mut |_| Some(
        HEADER.to_string()
    )));
    assert!(claim_backend_shutdown_from(&slot).is_some());
}

#[test]
fn release_flight_keeps_one_owner_and_never_adopts_another_target() {
    let slot = Mutex::new(CustodyState::Idle);
    let mut flight = begin_release_in(&slot, "/dev/nbd0").unwrap();
    assert!(begin_release_in(&slot, "/dev/nbd1").is_none());
    assert!(!reconcile_release_custody_in(&slot, &mut |_| panic!(
        "owned flight cannot be consumed"
    )));
    let mut executions = 0;
    assert_eq!(
        execute_release_flight(
            &mut flight,
            Duration::from_secs(1),
            |device, _, _| {
                assert_eq!(device, "/dev/nbd0");
                executions += 1;
                ReleaseOutcome::Pending
            },
            &mut |_| panic!("initial execution does not poll")
        ),
        ReleaseOutcome::Pending
    );
    assert_eq!(
        execute_release_flight(
            &mut flight,
            Duration::from_secs(1),
            |_, _, _| {
                executions += 1;
                ReleaseOutcome::Released
            },
            &mut |_| Some(HEADER.to_string())
        ),
        ReleaseOutcome::Released
    );
    assert_eq!(executions, 1, "a flight never repeats its effects");
    assert!(claim_backend_shutdown_from(&slot).is_none());
    drop(flight);
    assert!(begin_release_in(&slot, "/dev/nbd1").is_some());
}

#[test]
fn release_flight_poll_requires_fresh_exact_target_absence() {
    let slot = Mutex::new(CustodyState::Idle);
    let mut flight = begin_release_in(&slot, "/dev/nbd0").unwrap();
    record_release_outcome(&mut flight, ReleaseOutcome::Pending);
    for (snapshot, expected) in [
        (None, ReleaseOutcome::Pending),
        (Some("malformed".to_string()), ReleaseOutcome::Pending),
        (Some(format!("{HEADER}{TARGET}")), ReleaseOutcome::Refused),
        (
            Some(format!("{HEADER}/dev/nbd01 partition 1024 0 0\n")),
            ReleaseOutcome::Released,
        ),
    ] {
        assert_eq!(
            poll_release_outcome_with(&mut flight, &mut |_| snapshot.clone()),
            expected
        );
    }
}

#[test]
fn release_flight_remains_pending_until_owned_backing_is_inactive() {
    let fixture = ReleaseFixture::new();
    let slot = Mutex::new(CustodyState::Idle);
    let mut flight = begin_release_in(&slot, "/dev/nbd0").unwrap();
    flight.lease.custody.absorber = Some(SwapAbsorber::reserve(&fixture.backing_path()).unwrap());
    assert_eq!(
        record_release_outcome(&mut flight, ReleaseOutcome::Released),
        ReleaseOutcome::Pending
    );
    let active = format!("{HEADER}{} file 0 0 0\n", fixture.backing_path().display());
    assert_eq!(
        poll_release_outcome_with(&mut flight, &mut |_| Some(active.clone())),
        ReleaseOutcome::Pending
    );
    assert!(fixture.backing_path().exists());
    assert_eq!(
        poll_release_outcome_with(&mut flight, &mut |_| Some(HEADER.to_string())),
        ReleaseOutcome::Released
    );
    assert!(!fixture.backing_path().exists());
}

#[test]
fn release_flight_drop_preserves_pending_target_verification() {
    let slot = Mutex::new(CustodyState::Idle);
    let mut flight = begin_release_in(&slot, "/dev/nbd0").unwrap();
    record_release_outcome(&mut flight, ReleaseOutcome::Pending);
    drop(flight);
    assert!(!custody_is_clear(&slot));
    assert!(begin_release_in(&slot, "/dev/nbd1").is_none());
    assert!(!reconcile_release_custody_in(&slot, &mut |_| None));
    assert!(!reconcile_release_custody_in(&slot, &mut |_| Some(
        format!("{HEADER}{TARGET}")
    )));
    assert!(reconcile_release_custody_in(&slot, &mut |_| Some(
        HEADER.to_string()
    )));
    assert!(custody_is_clear(&slot));
}

#[test]
fn release_shutdown_refuses_noncanonical_target_spellings() {
    let slot = Mutex::new(CustodyState::Idle);
    for target in [
        "/dev/../dev/nbd0",
        "/dev/./nbd0",
        "/dev//nbd0",
        "/dev/nbd0/",
    ] {
        assert!(
            claim_backend_shutdown_for_in(&slot, target, &mut |_| Some(format!(
                "{HEADER}{TARGET}"
            )))
            .is_none(),
            "an active target alias cannot establish absence: {target}"
        );
        assert!(begin_release_in(&slot, target).is_none());
        assert!(custody_is_clear(&slot));
    }
    for target in ["/dev/nbd0", "/dev/ublkb7"] {
        let lease = claim_backend_shutdown_for_in(&slot, target, &mut |_| Some(HEADER.into()));
        assert!(lease.is_some(), "canonical exact absence remains valid");
        drop(lease);
    }
}

#[test]
fn release_shutdown_for_requires_fresh_exact_target_absence() {
    let slot = Mutex::new(CustodyState::Idle);
    assert!(begin_release_in(&slot, "relative").is_none());
    for snapshot in [
        None,
        Some("malformed".to_string()),
        Some(format!("{HEADER}{TARGET}")),
    ] {
        assert!(
            claim_backend_shutdown_for_in(&slot, "/dev/nbd0", &mut |_| snapshot.clone()).is_none()
        );
        assert!(custody_is_clear(&slot));
    }
    let guard =
        claim_backend_shutdown_for_in(&slot, "/dev/nbd0", &mut |_| Some(HEADER.to_string()))
            .unwrap();
    assert!(begin_release_in(&slot, "/dev/nbd0").is_none());
    drop(guard);
    assert!(custody_is_clear(&slot));
}

#[test]
fn companion_helper_freeze_follows_the_last_backend_arc() {
    use ramshared_exec::retained_process::{HelperState, ServingCommandOwner};
    use std::sync::{Arc, OnceLock};
    static OWNER: OnceLock<ServingCommandOwner> = OnceLock::new();
    let owner = OWNER.get_or_init(ServingCommandOwner::default);
    struct DropProof(Arc<Mutex<CustodyState>>, Arc<AtomicUsize>);
    impl Drop for DropProof {
        fn drop(&mut self) {
            assert!(
                self.0.try_lock().is_ok(),
                "companion drop must run outside the tier lock"
            );
            self.1.fetch_add(1, Ordering::SeqCst);
        }
    }
    let slot = Arc::new(Mutex::new(CustodyState::Idle));
    let drops = Arc::new(AtomicUsize::new(0));
    let helpers = owner.try_shutdown().unwrap();
    let lease = claim_backend_shutdown_from(&slot)
        .unwrap()
        .retain_shutdown_guard(helpers)
        .retain_shutdown_guard(DropProof(Arc::clone(&slot), Arc::clone(&drops)));
    let outer = Arc::new(lease);
    let worker = Arc::clone(&outer);
    drop(outer);
    assert_eq!(owner.state(), HelperState::Closing);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(claim_backend_shutdown_from(&slot).is_none());
    drop(worker);
    assert_eq!(owner.state(), HelperState::Idle);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(claim_backend_shutdown_from(&slot).is_some());
    owner
        .run(
            &mut std::process::Command::new("/bin/true"),
            Duration::from_secs(1),
            1024,
        )
        .unwrap();
}
