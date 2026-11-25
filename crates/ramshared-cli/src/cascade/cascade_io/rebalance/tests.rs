// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/rebalance/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

const TARGET_UUID: &str = "11111111-2222-4333-8444-555555555555";
const OTHER_UUID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const ZRAM_PATH: &str = "/dev/zram0";
const FALLBACK_PATH: &str = "/dev/sdf";
const FALLBACK_DEV_T: &str = "8:80";

fn execute_plan_with<R, S>(
    runner: &R,
    read_swap_snapshot: &mut S,
    target: &RebalanceTarget,
) -> Result<RebalanceOutcome, CascadeError>
where
    R: CommandRunner,
    S: FnMut() -> Result<Vec<SwapEntry>, CascadeError>,
{
    let noop = crate::operation_progress::OperationProgressReporter::no_op();
    execute_plan_with_progress(runner, read_swap_snapshot, target, &noop)
}

fn apply_plan_with<R, A, D, O, S>(
    runner: &R,
    first: RebalancePlan,
    admit: A,
    observe_fresh: O,
    read_swap_snapshot: &mut S,
) -> Result<RebalanceOutcome, CascadeError>
where
    R: CommandRunner,
    A: FnOnce() -> Result<D, CascadeError>,
    O: FnOnce() -> Result<RebalancePlan, CascadeError>,
    S: FnMut() -> Result<Vec<SwapEntry>, CascadeError>,
{
    let noop = crate::operation_progress::OperationProgressReporter::no_op();
    apply_plan_with_progress(
        runner,
        first,
        admit,
        observe_fresh,
        read_swap_snapshot,
        &noop,
    )
}

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

fn entry(path: &str, size_kb: u64, used_kb: u64, priority: i32) -> SwapEntry {
    SwapEntry {
        filename: path.to_string(),
        size_kb,
        used_kb,
        priority,
    }
}

fn temp_device(name: &str) -> (String, String, u64) {
    let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!(
        "ramshared-rebalance-{}-{name}-{sequence}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create fixture directory");
    let path = root.join("fallback");
    fs::write(&path, vec![7u8; 1024 * 1024]).expect("write fixture device");
    let metadata = fs::metadata(&path).expect("fixture metadata");
    let text = path.to_string_lossy().into_owned();
    (text, linux_device_number(metadata.rdev()), metadata.len())
}

fn base_observation(
    path: &str,
    dev_t: &str,
    size_bytes: u64,
    used_bytes: u64,
) -> RebalanceObservation {
    let target = entry(path, size_bytes / 1024, used_bytes / 1024, -2);
    let zram = entry(ZRAM_PATH, 1_000_000, 0, 100);
    RebalanceObservation {
        target: TargetObservation {
            path: path.to_string(),
            uuid: TARGET_UUID.into(),
            dev_t: dev_t.to_string(),
            priority: -2,
            size_bytes,
            used_bytes,
            is_block_device: true,
            is_ghost: false,
            row_count: 1,
            other_swap_uuids: vec![Some(OTHER_UUID.into())],
        },
        environment: EnvironmentObservation {
            root_dev_t: "8:0".into(),
            origin_partition_dev_t: "8:16".into(),
            origin_parent_dev_t: "8:0".into(),
            managed: vec![ManagedObservation {
                path: ZRAM_PATH.into(),
                dev_t: "252:0".into(),
                role: SwapTierRole::Zram,
            }],
        },
        authority: AuthorityObservation {
            peer_live: true,
            foreign_managed_swap: false,
        },
        swaps: vec![target, zram],
        mem: MemInfo {
            total_bytes: 16 * 1024 * 1024 * 1024,
            available_bytes: 8 * 1024 * 1024 * 1024,
        },
    }
}

fn safe_plan(used_bytes: u64) -> RebalancePlan {
    plan_from_observation(&base_observation(
        FALLBACK_PATH,
        FALLBACK_DEV_T,
        1024 * 1024,
        used_bytes,
    ))
    .expect("safe plan")
}

fn blocked_plan(used_bytes: u64) -> RebalancePlan {
    let mut observation = base_observation(FALLBACK_PATH, FALLBACK_DEV_T, 1024 * 1024, used_bytes);
    observation.mem.available_bytes = 64 * 1024 * 1024;
    plan_from_observation(&observation).expect("blocked plan")
}

#[derive(Default)]
struct RecordingRunner {
    calls: RefCell<Vec<String>>,
    results: RefCell<VecDeque<Result<String, String>>>,
}

impl RecordingRunner {
    fn with_results(results: Vec<Result<String, String>>) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            results: RefCell::new(results.into()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn record(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
        self.calls
            .borrow_mut()
            .push(format!("{command} {}", args.join(" ")));
        match self.results.borrow_mut().pop_front() {
            Some(Ok(stdout)) => Ok(stdout),
            Some(Err(message)) => Err(CascadeError::Shell {
                cmd: command.into(),
                msg: message,
            }),
            None => Ok(String::new()),
        }
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
        self.record(command, args)
    }

    fn run_bounded(
        &self,
        command: &str,
        args: &[&str],
        _timeout: Duration,
    ) -> Result<String, CascadeError> {
        self.record(command, args)
    }
}

fn snapshot_queue(
    frames: Vec<Result<Vec<SwapEntry>, String>>,
) -> impl FnMut() -> Result<Vec<SwapEntry>, CascadeError> {
    let queue = RefCell::new(VecDeque::from(frames));
    move || match queue.borrow_mut().pop_front() {
        Some(Ok(rows)) => Ok(rows),
        Some(Err(message)) => Err(CascadeError::Io(message)),
        None => Err(CascadeError::Io("test snapshot exhausted".into())),
    }
}

fn restored_rows(path: &str, used_kb: u64, priority: i32) -> Vec<SwapEntry> {
    vec![
        entry(path, 1024, used_kb, priority),
        entry(ZRAM_PATH, 1_000_000, 0, 100),
    ]
}

#[test]
fn parse_target_accepts_path_and_uuid_and_refuses_other_shapes() {
    assert_eq!(
        parse_target_selector("/dev/sdc").expect("path"),
        TargetSelector::Path("/dev/sdc".into())
    );
    assert_eq!(
        parse_target_selector("UUID=ABCdef01-2345-4678-8abc-def012345678").expect("uuid"),
        TargetSelector::Uuid("abcdef01-2345-4678-8abc-def012345678".into())
    );
    for refused in [
        "sdc",
        "/dev/",
        "/dev/../sdc",
        "uuid=abcdef01-2345-4678-8abc-def012345678",
        "UUID=abcdef01-2345-4678-8abc",
        "UUID=zzzzzzzz-2345-4678-8abc-def012345678",
        "--force",
    ] {
        assert!(
            parse_target_selector(refused).is_err(),
            "{refused} must be refused"
        );
    }
}

#[test]
fn plan_requires_exact_active_fallback_block_device() {
    let (path, dev_t, size_bytes) = temp_device("identity");
    let plan = plan_from_observation(&base_observation(&path, &dev_t, size_bytes, 4096))
        .expect("the real fallback shape plans");
    assert!(plan.target.last_resort);
    assert_eq!(plan.verdict, HeadroomVerdict::Safe);

    let mut not_block = base_observation(&path, &dev_t, size_bytes, 4096);
    not_block.target.is_block_device = false;
    assert!(
        plan_from_observation(&not_block)
            .expect_err("a file is not a block device")
            .to_string()
            .contains("not a block device")
    );

    let mut no_uuid = base_observation(&path, &dev_t, size_bytes, 4096);
    no_uuid.target.uuid.clear();
    assert!(
        plan_from_observation(&no_uuid)
            .expect_err("a missing UUID refuses")
            .to_string()
            .contains("no readable swap UUID")
    );

    let mut managed_role = base_observation(&path, &dev_t, size_bytes, 4096);
    managed_role.target.path = ZRAM_PATH.into();
    assert!(
        plan_from_observation(&managed_role)
            .expect_err("a managed tier is not the fallback")
            .to_string()
            .contains("not the fallback tier")
    );
}

#[test]
fn plan_refuses_swap_file_ghost_and_duplicate_row() {
    let (path, dev_t, size_bytes) = temp_device("ghost");
    let mut ghost = base_observation(&path, &dev_t, size_bytes, 4096);
    ghost.target.is_ghost = true;
    assert!(
        plan_from_observation(&ghost)
            .expect_err("a ghost refuses")
            .to_string()
            .contains("ghost swap row")
    );

    let mut duplicate = base_observation(&path, &dev_t, size_bytes, 4096);
    duplicate.target.row_count = 2;
    assert!(
        plan_from_observation(&duplicate)
            .expect_err("a duplicate row refuses")
            .to_string()
            .contains("exactly one active /proc/swaps row")
    );
}

#[test]
fn plan_refuses_duplicate_or_unreadable_uuid() {
    let (path, dev_t, size_bytes) = temp_device("uuid");
    let mut duplicate = base_observation(&path, &dev_t, size_bytes, 4096);
    duplicate.target.other_swap_uuids = vec![Some(TARGET_UUID.into())];
    assert!(
        plan_from_observation(&duplicate)
            .expect_err("a cloned UUID refuses")
            .to_string()
            .contains("also active on another swap row")
    );

    let mut unreadable = base_observation(&path, &dev_t, size_bytes, 4096);
    unreadable.target.other_swap_uuids = vec![None];
    assert!(
        plan_from_observation(&unreadable)
            .expect_err("an unreadable other UUID refuses")
            .to_string()
            .contains("no readable UUID")
    );
}

#[test]
fn plan_refuses_root_origin_and_managed_devices() {
    let (path, dev_t, size_bytes) = temp_device("environment");

    let mut root = base_observation(&path, &dev_t, size_bytes, 4096);
    root.environment.root_dev_t = dev_t.clone();
    assert!(
        plan_from_observation(&root)
            .expect_err("the root device refuses")
            .to_string()
            .contains("root filesystem device")
    );

    let mut origin = base_observation(&path, &dev_t, size_bytes, 4096);
    origin.environment.origin_partition_dev_t = dev_t.clone();
    assert!(
        plan_from_observation(&origin)
            .expect_err("the origin refuses")
            .to_string()
            .contains("sealed origin disk")
    );

    let mut managed_path = base_observation(&path, &dev_t, size_bytes, 4096);
    managed_path.environment.managed[0].path = path.clone();
    assert!(
        plan_from_observation(&managed_path)
            .expect_err("a managed path refuses")
            .to_string()
            .contains("is a managed device")
    );

    let mut managed_dev = base_observation(&path, &dev_t, size_bytes, 4096);
    managed_dev.environment.managed[0].dev_t = dev_t;
    assert!(
        plan_from_observation(&managed_dev)
            .expect_err("a managed device number refuses")
            .to_string()
            .contains("managed device number")
    );
}

#[test]
fn plan_requires_unique_last_resort_and_higher_tier() {
    let (path, dev_t, size_bytes) = temp_device("priority");

    let mut tie = base_observation(&path, &dev_t, size_bytes, 4096);
    tie.swaps.push(entry("/dev/sdb", 1024, 0, -2));
    assert!(
        plan_from_observation(&tie)
            .expect_err("a priority tie refuses")
            .to_string()
            .contains("unique minimum")
    );

    let mut no_higher = base_observation(&path, &dev_t, size_bytes, 4096);
    no_higher.swaps = vec![entry(&path, 1024, 4, -2), entry("/dev/sdb", 1024, 0, -1)];
    assert!(
        plan_from_observation(&no_higher)
            .expect_err("no managed tier refuses")
            .to_string()
            .contains("no active higher-priority managed swap tier")
    );

    let mut not_last = base_observation(&path, &dev_t, size_bytes, 4096);
    not_last.target.priority = 200;
    assert!(
        plan_from_observation(&not_last)
            .expect_err("a priority above the managed tier refuses")
            .to_string()
            .contains("not lower than the active managed tiers")
    );
}

#[test]
fn plan_refuses_dead_peer_or_foreign_managed_swap() {
    let (path, dev_t, size_bytes) = temp_device("authority");

    let mut dead = base_observation(&path, &dev_t, size_bytes, 4096);
    dead.authority.peer_live = false;
    assert!(
        plan_from_observation(&dead)
            .expect_err("a dead peer refuses")
            .to_string()
            .contains("peer is not live")
    );

    let mut foreign = base_observation(&path, &dev_t, size_bytes, 4096);
    foreign.authority.foreign_managed_swap = true;
    assert!(
        plan_from_observation(&foreign)
            .expect_err("a foreign managed swap refuses")
            .to_string()
            .contains("foreign or ambiguous managed swap")
    );
}

#[test]
fn headroom_is_resident_only_and_strict() {
    let plan = safe_plan(100 * 1024 * 1024);
    let zram_free = 1_000_000u64 * 1024;
    assert_eq!(plan.remaining_swap_free_bytes, zram_free);
    assert_eq!(plan.available_bytes, 8 * 1024 * 1024 * 1024);
    assert_eq!(
        plan.required_bytes,
        plan.target.used_bytes + plan.reserve_bytes + plan.margin_bytes
    );
    assert_eq!(
        plan.reserve_bytes,
        (16u64 * 1024 * 1024 * 1024).div_ceil(10)
    );
    assert_eq!(plan.margin_bytes, 512 * 1024 * 1024);
    assert_eq!(plan.deficit_bytes, 0);

    let blocked = blocked_plan(100 * 1024 * 1024);
    assert_eq!(blocked.verdict, HeadroomVerdict::Blocked);
    assert!(blocked.deficit_bytes > 0);
    assert_eq!(blocked.reason, "mem_available_below_reserve");
}

#[test]
fn blocked_apply_runs_no_swap_command() {
    let runner = RecordingRunner::default();
    let mut snapshots = snapshot_queue(Vec::new());
    let error = apply_plan_with(
        &runner,
        blocked_plan(4096),
        || -> Result<(), CascadeError> { panic!("custody must not be taken") },
        || -> Result<RebalancePlan, CascadeError> { panic!("fresh plan must not run") },
        &mut snapshots,
    )
    .expect_err("a blocked apply refuses");
    assert!(error.to_string().contains("blocked"));
    assert!(runner.calls().is_empty());
}

#[test]
fn apply_refuses_when_custody_admission_is_denied() {
    let runner = RecordingRunner::default();
    let mut snapshots = snapshot_queue(Vec::new());
    let error = apply_plan_with(
        &runner,
        safe_plan(4096),
        || -> Result<(), CascadeError> {
            Err(CascadeError::Precondition(
                "customer mutation custody refused: active_session".into(),
            ))
        },
        || -> Result<RebalancePlan, CascadeError> { panic!("fresh plan must not run") },
        &mut snapshots,
    )
    .expect_err("a refused admission refuses the apply");
    assert!(error.to_string().contains("active_session"));
    assert!(runner.calls().is_empty());
}

#[test]
fn plan_refuses_non_root_with_typed_refusal() {
    let error = require_euid(1000).expect_err("a non-root euid refuses");
    assert!(matches!(error, CascadeError::Precondition(_)));
    assert!(error.to_string().contains("requires root"));
    assert!(require_euid(0).is_ok());
}

#[test]
fn apply_refuses_changed_target_before_effect() {
    let runner = RecordingRunner::default();
    let first = safe_plan(4096);
    let mut fresh = first.clone();
    fresh.target.dev_t = "9:9".into();
    let mut snapshots = snapshot_queue(Vec::new());
    let error = apply_plan_with(
        &runner,
        first,
        || Ok(()),
        || Ok(fresh.clone()),
        &mut snapshots,
    )
    .expect_err("a changed target refuses");
    assert!(error.to_string().contains("changed before effect"));
    assert!(runner.calls().is_empty());
}

#[test]
fn apply_refuses_changed_higher_tier_set() {
    let runner = RecordingRunner::default();
    let first = safe_plan(4096);
    let mut fresh = first.clone();
    fresh.higher_tiers[0].priority = 77;
    let mut snapshots = snapshot_queue(Vec::new());
    let error = apply_plan_with(
        &runner,
        first,
        || Ok(()),
        || Ok(fresh.clone()),
        &mut snapshots,
    )
    .expect_err("a changed higher-tier set refuses");
    assert!(error.to_string().contains("higher-tier set"));
    assert!(runner.calls().is_empty());
}

#[test]
fn target_used_growth_replans_from_fresh_state() {
    let runner = RecordingRunner::with_results(vec![Ok(String::new()), Ok(String::new())]);
    let (path, dev_t, size_bytes) = temp_device("growth");
    let first = plan_from_observation(&base_observation(&path, &dev_t, size_bytes, 4096))
        .expect("first plan");
    let mut fresh = first.clone();
    fresh.target.used_bytes = 8 * 1024;
    let mut snapshots = snapshot_queue(vec![
        Ok(vec![
            entry(&path, 1024, 8, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ]),
        Ok(vec![entry(ZRAM_PATH, 1_000_000, 0, 100)]),
        Ok(restored_rows(&path, 0, -2)),
    ]);
    let outcome = apply_plan_with(
        &runner,
        first,
        || Ok(()),
        || Ok(fresh.clone()),
        &mut snapshots,
    )
    .expect("the fresh plan is safe");
    assert_eq!(outcome.target_before.used_bytes, 8 * 1024);
    assert_eq!(outcome.result, "applied");
}

#[test]
fn zero_used_target_is_a_noop_without_commands() {
    let runner = RecordingRunner::default();
    let mut snapshots = snapshot_queue(Vec::new());
    let outcome = apply_plan_with(
        &runner,
        safe_plan(0),
        || -> Result<(), CascadeError> { panic!("a no-op takes no custody") },
        || -> Result<RebalancePlan, CascadeError> { panic!("a no-op needs no fresh plan") },
        &mut snapshots,
    )
    .expect("an empty target is a no-op");
    assert_eq!(outcome.result, "already_balanced");
    assert!(!outcome.applied);
    assert!(runner.calls().is_empty());
    let rendered = render_outcome(&outcome);
    assert!(
        rendered.contains("swapoff=not_run swapon=not_run"),
        "{rendered}"
    );
    assert!(!rendered.contains("swapoff=ok"), "{rendered}");
}

#[test]
fn apply_runs_only_exact_swapoff_then_swapon() {
    let runner = RecordingRunner::with_results(vec![Ok(String::new()), Ok(String::new())]);
    let (path, dev_t, size_bytes) = temp_device("applied");
    let target = plan_from_observation(&base_observation(&path, &dev_t, size_bytes, 4096))
        .expect("plan")
        .target;
    let mut snapshots = snapshot_queue(vec![
        Ok(vec![
            entry(&path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ]),
        Ok(vec![entry(ZRAM_PATH, 1_000_000, 0, 100)]),
        Ok(restored_rows(&path, 0, -2)),
    ]);
    let outcome = execute_plan_with(&runner, &mut snapshots, &target).expect("apply");
    assert_eq!(outcome.result, "applied");
    assert!(outcome.applied);
    assert!(outcome.restored);
    assert_eq!(outcome.target_after_used_bytes, Some(0));
    assert_eq!(
        runner.calls(),
        vec![
            format!("swapoff UUID={TARGET_UUID}"),
            format!("swapon -p -2 UUID={TARGET_UUID}"),
        ]
    );
}

#[test]
fn apply_stops_when_target_remains() {
    let runner = RecordingRunner::with_results(vec![Ok(String::new())]);
    let (path, dev_t, size_bytes) = temp_device("remains");
    let target = plan_from_observation(&base_observation(&path, &dev_t, size_bytes, 4096))
        .expect("plan")
        .target;
    let mut snapshots = snapshot_queue(vec![
        Ok(vec![
            entry(&path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ]),
        Ok(vec![
            entry(&path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ]),
    ]);
    let outcome = execute_plan_with(&runner, &mut snapshots, &target).expect("outcome");
    assert_eq!(outcome.result, "failed");
    assert!(!outcome.restored);
    assert_eq!(outcome.target_after_used_bytes, Some(4096));
    assert_eq!(runner.calls(), vec![format!("swapoff UUID={TARGET_UUID}")]);
}

#[test]
fn apply_restores_after_nonzero_swapoff_when_target_is_absent() {
    let runner = RecordingRunner::with_results(vec![
        Err("kernel still held a page".into()),
        Ok(String::new()),
    ]);
    let (path, dev_t, size_bytes) = temp_device("degraded");
    let target = plan_from_observation(&base_observation(&path, &dev_t, size_bytes, 4096))
        .expect("plan")
        .target;
    let mut snapshots = snapshot_queue(vec![
        Ok(vec![
            entry(&path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ]),
        Ok(vec![entry(ZRAM_PATH, 1_000_000, 0, 100)]),
        Ok(restored_rows(&path, 0, -2)),
    ]);
    let outcome = execute_plan_with(&runner, &mut snapshots, &target).expect("outcome");
    assert_eq!(outcome.result, "degraded");
    assert!(outcome.restored);
    assert!(!outcome.applied);
    assert!(outcome.swapoff_error.is_some());
    assert_eq!(
        runner.calls(),
        vec![
            format!("swapoff UUID={TARGET_UUID}"),
            format!("swapon -p -2 UUID={TARGET_UUID}"),
        ]
    );
}

#[test]
fn apply_reports_restore_failure() {
    let runner = RecordingRunner::with_results(vec![
        Ok(String::new()),
        Err("uuid restore refused".into()),
        Err("path restore refused".into()),
    ]);
    let (path, dev_t, size_bytes) = temp_device("restore-failed");
    let target = plan_from_observation(&base_observation(&path, &dev_t, size_bytes, 4096))
        .expect("plan")
        .target;
    let mut snapshots = snapshot_queue(vec![
        Ok(vec![
            entry(&path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ]),
        Ok(vec![entry(ZRAM_PATH, 1_000_000, 0, 100)]),
    ]);
    let error = execute_plan_with(&runner, &mut snapshots, &target)
        .expect_err("a failed restore is unsafe containment");
    assert!(matches!(error, CascadeError::UnsafeContainment(_)));
    assert!(error.to_string().contains(&manual_restore_command(&target)));
    assert_eq!(runner.calls().len(), 3);

    // Second case: the absence proof itself is unreadable; restore fails.
    let runner = RecordingRunner::with_results(vec![
        Ok(String::new()),
        Err("uuid restore refused".into()),
        Err("path restore refused".into()),
    ]);
    let mut unreadable_frames = snapshot_queue(vec![
        Ok(vec![
            entry(&path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ]),
        Err("proc/swaps unreadable".into()),
    ]);
    let error = execute_plan_with(&runner, &mut unreadable_frames, &target)
        .expect_err("an unreadable proof with a failed restore is unsafe");
    assert!(error.to_string().contains("post-swapoff"));
    assert!(error.to_string().contains(&manual_restore_command(&target)));
}

#[test]
fn apply_reports_restored_but_not_empty() {
    let runner = RecordingRunner::with_results(vec![Ok(String::new()), Ok(String::new())]);
    let (path, dev_t, size_bytes) = temp_device("not-empty");
    let target = plan_from_observation(&base_observation(&path, &dev_t, size_bytes, 4096))
        .expect("plan")
        .target;
    let mut snapshots = snapshot_queue(vec![
        Ok(vec![
            entry(&path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ]),
        Ok(vec![entry(ZRAM_PATH, 1_000_000, 0, 100)]),
        Ok(restored_rows(&path, 4, -2)),
    ]);
    let outcome = execute_plan_with(&runner, &mut snapshots, &target).expect("outcome");
    assert_eq!(outcome.result, "failed");
    assert!(outcome.restored);
    assert_eq!(outcome.target_after_used_bytes, Some(4096));
    assert_eq!(runner.calls().len(), 2);
    assert!(render_outcome(&outcome).contains("target_after_used=4096"));
    let unverified = RebalanceOutcome {
        target_after_used_bytes: None,
        ..outcome
    };
    assert!(render_outcome(&unverified).contains("target_after_used=unverified"));
}

#[test]
fn apply_reports_changed_restored_identity() {
    let runner = RecordingRunner::with_results(vec![Ok(String::new()), Ok(String::new())]);
    let (path, dev_t, size_bytes) = temp_device("changed-identity");
    let target = plan_from_observation(&base_observation(&path, &dev_t, size_bytes, 4096))
        .expect("plan")
        .target;
    let mut snapshots = snapshot_queue(vec![
        Ok(vec![
            entry(&path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ]),
        Ok(vec![entry(ZRAM_PATH, 1_000_000, 0, 100)]),
        Ok(restored_rows(&path, 0, 7)),
    ]);
    let outcome = execute_plan_with(&runner, &mut snapshots, &target).expect("outcome");
    assert_eq!(outcome.result, "failed");
    assert_eq!(
        outcome.swapon_error.as_deref(),
        Some("restored row priority differs")
    );
}

#[test]
fn plan_json_has_stable_fields() {
    let plan = safe_plan(4096);
    let rendered = render_plan(&plan, true).expect("JSON plan");
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value["schema"], REBALANCE_SCHEMA);
    assert_eq!(value["verdict"], "safe");
    assert_eq!(value["reason"], "safe");
    assert_eq!(value["would_change"], true);
    assert_eq!(value["target"]["uuid"], TARGET_UUID);
    assert_eq!(value["target"]["last_resort"], true);
    assert_eq!(value["target"]["used_bytes"], 4096);
    assert_eq!(value["higher_tiers"][0]["role"], "zram");
    assert_eq!(value["higher_tiers"][0]["priority"], 100);

    let text = render_plan(&plan, false).expect("text plan");
    assert!(text.contains("target=/dev/sdf"));
    assert!(text.contains("higher_tier=/dev/zram0 role=zram priority=100"));
    assert!(text.contains("verdict=safe reason=safe would_change=true"));
    let blocked = render_plan(&blocked_plan(4096), false).expect("blocked text plan");
    assert!(blocked.contains("verdict=blocked"));

    let empty = safe_plan(0);
    let value: serde_json::Value =
        serde_json::from_str(&render_plan(&empty, true).expect("JSON plan")).expect("valid JSON");
    assert_eq!(value["reason"], "already_balanced");
    assert_eq!(value["would_change"], false);
    assert_eq!(value["verdict"], "safe");
}

#[test]
fn live_rebalance_plan_and_apply_validate_uuid_symlinks_and_execute_with_custody() {
    use super::live::{observe_target_at, swap_rebalance_apply_with, swap_rebalance_plan_with};
    use crate::cascade::cascade_io::tests_suite::{
        ParentSeams, ScriptedRunner, bound_device_fixture, controlled_child,
        seal_runtime_lifecycle, set_live_device_snapshots, set_swap_snapshots,
    };

    let (target_path, _dev_t, _size_bytes) = temp_device("live-adapter");
    let root = Path::new(&target_path).parent().unwrap().to_path_buf();
    let paths = RuntimePaths::under(&root);
    let uuid_dir = root.join("by-uuid");
    fs::create_dir_all(&uuid_dir).unwrap();
    let link = uuid_dir.join(TARGET_UUID);
    std::os::unix::fs::symlink(&target_path, &link).unwrap();

    let daemon = controlled_child();
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    let swap_line = format!(
        "Filename Type Size Used Priority\n{target_path} partition 1024 4 -2\n/dev/zram0 partition 1000000 0 100\n"
    );
    let _seams = ParentSeams::install(&swap_line, 0);
    let blkid_label = format!("blkid -s UUID -o value {target_path}");
    let zram_blkid = "blkid -s UUID -o value /dev/zram0".to_string();

    // Non-root is refused before touching anything.
    let runner_nonroot = ScriptedRunner::new(vec![]);
    assert!(
        swap_rebalance_plan_with(
            &runner_nonroot,
            &paths,
            &uuid_dir,
            &root,
            1000,
            "UUID=11111111-2222-4333-8444-555555555555",
            true,
            |_| true,
        )
        .is_err()
    );
    assert!(
        swap_rebalance_apply_with(
            &runner_nonroot,
            &paths,
            &uuid_dir,
            &root,
            1000,
            &format!("UUID={TARGET_UUID}"),
            |_| true,
            || Ok(()),
        )
        .is_err()
    );

    // Regular file without block-device override is refused by plan.
    set_live_device_snapshots([Ok(devices.clone())]);
    let non_block_runner = ScriptedRunner::new(vec![
        (blkid_label.clone(), Ok(TARGET_UUID.into())),
        (zram_blkid.clone(), Ok(OTHER_UUID.into())),
    ]);
    assert!(
        swap_rebalance_plan_with(
            &non_block_runner,
            &paths,
            &uuid_dir,
            &root,
            0,
            &format!("UUID={TARGET_UUID}"),
            false,
            |meta| meta.file_type().is_block_device(),
        )
        .is_err()
    );

    // live plan works cleanly against injectable by-uuid & sealed runtime paths.
    set_live_device_snapshots([
        Ok(devices.clone()),
        Ok(devices.clone()),
        Ok(devices.clone()),
    ]);
    let plan_runner = ScriptedRunner::new(vec![
        (blkid_label.clone(), Ok(TARGET_UUID.into())),
        (zram_blkid.clone(), Ok(OTHER_UUID.into())),
    ]);
    swap_rebalance_plan_with(
        &plan_runner,
        &paths,
        &uuid_dir,
        &root,
        0,
        &format!("UUID={TARGET_UUID}"),
        true,
        |_| true,
    )
    .expect("live rebalance plan");

    // UUID resolving to a different file refuses safely.
    let other_file = root.join("other-swap");
    fs::write(&other_file, b"other").unwrap();
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(&other_file, &link).unwrap();
    let bad_uuid_obs = observe_target_at(
        &ScriptedRunner::new(vec![
            (blkid_label.clone(), Ok(TARGET_UUID.into())),
            (zram_blkid.clone(), Ok(OTHER_UUID.into())),
        ]),
        &paths,
        &[
            entry(&target_path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ],
        MemInfo {
            total_bytes: 16 * 1024 * 1024 * 1024,
            available_bytes: 8 * 1024 * 1024 * 1024,
        },
        &TargetSelector::Uuid(TARGET_UUID.into()),
        &uuid_dir,
        &root,
        |_| true,
    );
    assert!(bad_uuid_obs.is_err());
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(&target_path, &link).unwrap();

    // Absent swap row and unreadable path-selected UUID refuse.
    assert!(
        observe_target_at(
            &ScriptedRunner::new(vec![(zram_blkid.clone(), Ok(OTHER_UUID.into()))]),
            &paths,
            &[entry(ZRAM_PATH, 1_000_000, 0, 100)],
            MemInfo {
                total_bytes: 16 * 1024 * 1024 * 1024,
                available_bytes: 8 * 1024 * 1024 * 1024,
            },
            &TargetSelector::Path(target_path.clone()),
            &uuid_dir,
            &root,
            |_| true,
        )
        .is_err()
    );

    // Apply end-to-end through `swap_rebalance_apply_with`.
    let swapoff_after = "Filename Type Size Used Priority\n/dev/zram0 partition 1000000 0 100\n";
    let restored = format!(
        "Filename Type Size Used Priority\n{target_path} partition 1024 0 -2\n/dev/zram0 partition 1000000 0 100\n"
    );
    let _swap_leak: &'static str = Box::leak(swap_line.clone().into_boxed_str());
    let _restored_leak: &'static str = Box::leak(restored.into_boxed_str());
    set_swap_snapshots([
        Ok(_swap_leak),
        Ok(_swap_leak),
        Ok(_swap_leak),
        Ok(swapoff_after),
        Ok(_restored_leak),
    ]);
    let _nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    set_live_device_snapshots([Ok(devices.clone()), Ok(devices.clone()), Ok(devices)]);
    let apply_runner = ScriptedRunner::new(vec![
        (blkid_label.clone(), Ok(TARGET_UUID.into())),
        (zram_blkid.clone(), Ok(OTHER_UUID.into())),
        (blkid_label, Ok(TARGET_UUID.into())),
        (zram_blkid, Ok(OTHER_UUID.into())),
        (format!("swapoff UUID={TARGET_UUID}"), Ok(String::new())),
        (
            format!("swapon -p -2 UUID={TARGET_UUID}"),
            Ok(String::new()),
        ),
    ]);
    assert!(
        swap_rebalance_apply_with(
            &apply_runner,
            &paths,
            &uuid_dir,
            &root,
            0,
            &target_path,
            |_| true,
            || Ok(()),
        )
        .is_err()
    );
    swap_rebalance_apply_with(
        &apply_runner,
        &paths,
        &uuid_dir,
        &root,
        0,
        &format!("UUID={TARGET_UUID}"),
        |_| true,
        || Ok(()),
    )
    .expect("live rebalance apply");

    // Additional live adapter branches: missing blkid UUID for Path, missing by-uuid symlink,
    // Blocked plan verdict, Degraded outcome, and non-root public wrappers.
    let no_uuid_obs = observe_target_at(
        &ScriptedRunner::new(vec![
            (
                format!("blkid -s UUID -o value {target_path}"),
                Err(CascadeError::Precondition("no uuid".into())),
            ),
            (
                format!("blkid -s UUID -o value {ZRAM_PATH}"),
                Ok(OTHER_UUID.into()),
            ),
        ]),
        &paths,
        &[
            entry(&target_path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ],
        MemInfo {
            total_bytes: 16 * 1024 * 1024 * 1024,
            available_bytes: 8 * 1024 * 1024 * 1024,
        },
        &TargetSelector::Path(target_path.clone()),
        &uuid_dir,
        &root,
        |_| true,
    );
    assert!(no_uuid_obs.is_err());

    let missing_uuid_dir = root.join("missing-by-uuid");
    let no_link_obs = observe_target_at(
        &ScriptedRunner::new(vec![
            (
                format!("blkid -s UUID -o value {target_path}"),
                Ok(TARGET_UUID.into()),
            ),
            (
                format!("blkid -s UUID -o value {ZRAM_PATH}"),
                Ok(OTHER_UUID.into()),
            ),
        ]),
        &paths,
        &[
            entry(&target_path, 1024, 4, -2),
            entry(ZRAM_PATH, 1_000_000, 0, 100),
        ],
        MemInfo {
            total_bytes: 16 * 1024 * 1024 * 1024,
            available_bytes: 8 * 1024 * 1024 * 1024,
        },
        &TargetSelector::Uuid(TARGET_UUID.into()),
        &missing_uuid_dir,
        &root,
        |_| true,
    );
    assert!(no_link_obs.is_err());

    // Blocked verdict in `swap_rebalance_plan_with`: target used exceeds any host RAM/headroom.
    let tight_swap = format!(
        "Filename Type Size Used Priority\n{target_path} partition 100000000000 100000000000 -2\n/dev/zram0 partition 1000 1000 100\n"
    );
    let _tight_leak: &'static str = Box::leak(tight_swap.into_boxed_str());
    set_swap_snapshots([Ok(_tight_leak)]);
    let blocked_devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    set_live_device_snapshots([Ok(blocked_devices)]);
    let blocked_runner = ScriptedRunner::new(vec![
        (
            format!("blkid -s UUID -o value {target_path}"),
            Ok(TARGET_UUID.into()),
        ),
        (
            format!("blkid -s UUID -o value {ZRAM_PATH}"),
            Ok(OTHER_UUID.into()),
        ),
    ]);
    assert!(
        swap_rebalance_plan_with(
            &blocked_runner,
            &paths,
            &uuid_dir,
            &root,
            0,
            &format!("UUID={TARGET_UUID}"),
            true,
            |_| true,
        )
        .is_err()
    );

    // Degraded outcome in `swap_rebalance_apply_with`: swapoff errors, target absent, swapon succeeds.
    set_swap_snapshots([
        Ok(_swap_leak),
        Ok(_swap_leak),
        Ok(_swap_leak),
        Ok(swapoff_after),
        Ok(_restored_leak),
    ]);
    let dev_snap = seal_runtime_lifecycle(&paths, daemon.id(), true);
    set_live_device_snapshots([Ok(dev_snap.clone()), Ok(dev_snap.clone()), Ok(dev_snap)]);
    let degraded_runner = ScriptedRunner::new(vec![
        (
            format!("blkid -s UUID -o value {target_path}"),
            Ok(TARGET_UUID.into()),
        ),
        (
            format!("blkid -s UUID -o value {ZRAM_PATH}"),
            Ok(OTHER_UUID.into()),
        ),
        (
            format!("blkid -s UUID -o value {target_path}"),
            Ok(TARGET_UUID.into()),
        ),
        (
            format!("blkid -s UUID -o value {ZRAM_PATH}"),
            Ok(OTHER_UUID.into()),
        ),
        (
            format!("swapoff UUID={TARGET_UUID}"),
            Err(CascadeError::Precondition("swapoff nonzero".into())),
        ),
        (
            format!("swapon -p -2 UUID={TARGET_UUID}"),
            Ok(String::new()),
        ),
    ]);
    let degraded_err = swap_rebalance_apply_with(
        &degraded_runner,
        &paths,
        &uuid_dir,
        &root,
        0,
        &format!("UUID={TARGET_UUID}"),
        |_| true,
        || Ok(()),
    )
    .expect_err("degraded result returns error");
    assert!(
        degraded_err.to_string().contains("result=degraded"),
        "{degraded_err}"
    );

    if rustix::process::geteuid().as_raw() != 0 {
        assert!(swap_rebalance_plan("UUID=11111111-2222-4333-8444-555555555555", false).is_err());
        assert!(swap_rebalance_apply("UUID=11111111-2222-4333-8444-555555555555").is_err());
    }

    let _ = fs::remove_dir_all(&root);
}
