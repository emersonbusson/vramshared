// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/rebalance.rs
//! Attended fallback-swap rebalance.
//!
//! The fallback tier is the last-resort swap sink. When it has absorbed pages
//! that a healthy higher tier could hold, an operator may drain exactly that
//! one block device: `swapoff UUID=...` returns its pages to resident memory
//! and the higher tiers re-compress or re-spill what they can, then the device
//! is restored empty at the same priority. The drain is forward-only — page
//! placement is not rolled back — and it is attended: one exact target, one
//! plan, one effect pair, no `-a`, no `--force`, no automatic execution.
//!
//! This module owns the fallback policy. The managed-swap headroom formula,
//! the lifecycle binding, the live authorization and the mutation guard are
//! reused unchanged; nothing here re-derives a device identity or a reserve.

use super::swapoff_headroom::{HeadroomVerdict, MemInfo, SwapAmount};
use super::*;

/// A fallback can hold a large, cold region; its `swapoff` is slower than a
/// managed zram drain. Dedicated bound so a stuck kernel D-state is reported
/// rather than waited on forever.
const FALLBACK_SWAPOFF_TIMEOUT: Duration = Duration::from_secs(300);
const FALLBACK_SWAPON_TIMEOUT: Duration = Duration::from_secs(30);
/// Read-only identity probes (`blkid`). Small and bounded; never an effect.
const FALLBACK_IDENTITY_TIMEOUT: Duration = Duration::from_secs(5);

const REBALANCE_SCHEMA: &str = "ramshared-swap-rebalance/v1";

mod render;
mod selector;
use render::{render_outcome, render_plan};
use selector::{canonical_swap_uuid, parse_target_selector};

// The live host adapter (binding, authorization, `/proc`, `/dev` probes) is a
// separate effect module; the policy and the testable effect seam stay here.
mod live;
pub(crate) use live::{swap_rebalance_apply, swap_rebalance_plan};

/// What the operator named: an absolute `/dev/...` path or a canonical
/// `UUID=<uuid>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TargetSelector {
    Path(String),
    Uuid(String),
}

/// One target row observed from `/proc/swaps` plus block-device metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TargetObservation {
    pub path: String,
    pub uuid: String,
    pub dev_t: String,
    pub priority: i32,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub is_block_device: bool,
    pub is_ghost: bool,
    pub row_count: usize,
    /// UUID of every other active swap row; `None` means unreadable.
    pub other_swap_uuids: Vec<Option<String>>,
}

/// The live-lifecycle facts the pure plan consumes. Only the live wrapper may
/// set `peer_live = true`, and only after `authorize_bound_environment`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AuthorityObservation {
    pub peer_live: bool,
    pub foreign_managed_swap: bool,
}

/// One active managed tier that can absorb the drained pages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HigherTier {
    pub path: String,
    pub role: SwapTierRole,
    pub priority: i32,
    pub size_bytes: u64,
    pub used_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RebalanceTarget {
    pub path: String,
    pub uuid: String,
    pub dev_t: String,
    pub priority: i32,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub last_resort: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RebalancePlan {
    pub target: RebalanceTarget,
    pub higher_tiers: Vec<HigherTier>,
    pub available_bytes: u64,
    pub remaining_swap_free_bytes: u64,
    pub reserve_bytes: u64,
    pub margin_bytes: u64,
    pub required_bytes: u64,
    pub deficit_bytes: u64,
    pub verdict: HeadroomVerdict,
    pub reason: &'static str,
    pub would_change: bool,
    pub schema: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RebalanceOutcome {
    pub target_before: RebalanceTarget,
    pub target_after_used_bytes: Option<u64>,
    pub swapoff_error: Option<String>,
    pub swapon_error: Option<String>,
    pub restored: bool,
    pub applied: bool,
    pub result: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedObservation {
    path: String,
    dev_t: String,
    role: SwapTierRole,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EnvironmentObservation {
    root_dev_t: String,
    origin_partition_dev_t: String,
    origin_parent_dev_t: String,
    managed: Vec<ManagedObservation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RebalanceObservation {
    target: TargetObservation,
    environment: EnvironmentObservation,
    authority: AuthorityObservation,
    swaps: Vec<SwapEntry>,
    mem: MemInfo,
}

fn require_euid(euid: u32) -> Result<(), CascadeError> {
    if euid == 0 {
        return Ok(());
    }
    Err(CascadeError::Precondition(
        "fallback swap rebalance requires root (euid 0); the lifecycle binding and the plan are root-owned"
            .into(),
    ))
}

/// Pure plan from one observation frame. Every refusal is deterministic and
/// runs no command.
fn plan_from_observation(
    observation: &RebalanceObservation,
) -> Result<RebalancePlan, CascadeError> {
    let target = &observation.target;
    if target.is_ghost {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target {} is a ghost swap row; the kernel lost the device",
            target.path
        )));
    }
    if target.row_count != 1 {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target {} is not exactly one active /proc/swaps row",
            target.path
        )));
    }
    if !target.is_block_device {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target {} is not a block device",
            target.path
        )));
    }
    if target.uuid.is_empty() {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target {} has no readable swap UUID",
            target.path
        )));
    }
    if swap_tier_role(&target.path) != SwapTierRole::Fallback {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target {} is not the fallback tier",
            target.path
        )));
    }
    for other in &target.other_swap_uuids {
        match other {
            None => {
                return Err(CascadeError::Precondition(
                    "fallback rebalance refused: an active swap row has no readable UUID".into(),
                ));
            }
            Some(uuid) if *uuid == target.uuid => {
                return Err(CascadeError::Precondition(format!(
                    "fallback rebalance refused: UUID {} is also active on another swap row",
                    target.uuid
                )));
            }
            Some(_) => {}
        }
    }
    if !observation.authority.peer_live {
        return Err(CascadeError::Precondition(
            "fallback rebalance refused: the sealed lifecycle peer is not live".into(),
        ));
    }
    if observation.authority.foreign_managed_swap {
        return Err(CascadeError::Precondition(
            "fallback rebalance refused: a foreign or ambiguous managed swap is active".into(),
        ));
    }

    let environment = &observation.environment;
    if environment
        .managed
        .iter()
        .any(|managed| managed.path == target.path)
    {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target {} is a managed device",
            target.path
        )));
    }
    if target.dev_t == environment.root_dev_t {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target {} is the root filesystem device",
            target.path
        )));
    }
    if target.dev_t == environment.origin_partition_dev_t
        || target.dev_t == environment.origin_parent_dev_t
    {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target {} belongs to the sealed origin disk",
            target.path
        )));
    }
    if environment
        .managed
        .iter()
        .any(|managed| managed.dev_t == target.dev_t)
    {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target {} has a managed device number",
            target.path
        )));
    }

    let managed_tiers = active_managed_tiers(&observation.swaps);
    if managed_tiers.is_empty() {
        return Err(CascadeError::Precondition(
            "fallback rebalance refused: no active higher-priority managed swap tier".into(),
        ));
    }
    let highest_managed_priority = managed_tiers
        .iter()
        .map(|tier| tier.priority)
        .max()
        .unwrap_or(i32::MIN);
    if target.priority >= highest_managed_priority {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance refused: target priority {} is not lower than the active managed tiers",
            target.priority
        )));
    }
    for swap in &observation.swaps {
        if row_matches(swap, &target.path) {
            continue;
        }
        if swap.priority <= target.priority {
            return Err(CascadeError::Precondition(format!(
                "fallback rebalance refused: target priority {} is not the unique minimum among active swaps",
                target.priority
            )));
        }
    }

    let amounts = swap_amounts_from(&observation.swaps);
    let planned = vec![target.path.clone()];
    let (managed_bytes, remaining_free_bytes) =
        swapoff_headroom::summarize_swaps(&planned, &amounts);
    let decision = swapoff_headroom::evaluate(managed_bytes, remaining_free_bytes, observation.mem);
    let reason = if target.used_bytes == 0 {
        "already_balanced"
    } else {
        decision.reason
    };
    let verdict = if target.used_bytes == 0 {
        HeadroomVerdict::Safe
    } else {
        decision.verdict
    };

    Ok(RebalancePlan {
        target: RebalanceTarget {
            path: target.path.clone(),
            uuid: target.uuid.clone(),
            dev_t: target.dev_t.clone(),
            priority: target.priority,
            size_bytes: target.size_bytes,
            used_bytes: target.used_bytes,
            last_resort: true,
        },
        higher_tiers: managed_tiers,
        available_bytes: decision.available_bytes,
        remaining_swap_free_bytes: decision.remaining_free_bytes,
        reserve_bytes: decision.reserve_bytes,
        margin_bytes: decision.margin_bytes,
        required_bytes: decision.required_bytes,
        deficit_bytes: decision.deficit_bytes,
        verdict,
        reason,
        would_change: target.used_bytes > 0,
        schema: REBALANCE_SCHEMA,
    })
}

fn active_managed_tiers(swaps: &[SwapEntry]) -> Vec<HigherTier> {
    let mut tiers = Vec::new();
    for entry in swaps {
        let path = entry.canonical_path();
        let role = swap_tier_role(&path);
        if !matches!(role, SwapTierRole::Zram | SwapTierRole::Vram) || entry.is_ghost() {
            continue;
        }
        tiers.push(HigherTier {
            path,
            role,
            priority: entry.priority,
            size_bytes: entry.size_kb.saturating_mul(1024),
            used_bytes: entry.used_kb.saturating_mul(1024),
        });
    }
    tiers.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.path.cmp(&right.path))
    });
    tiers
}

pub(super) fn swap_row_path(entry: &SwapEntry) -> String {
    let bare = entry.bare_path();
    if bare.starts_with('/') && bare[1..].contains('/') && !bare.starts_with("/dev/") {
        bare
    } else {
        entry.canonical_path()
    }
}

/// The teardown plan's conversion pattern: keep the exact spelling matching `target.path`
/// (canonical `/dev/...` for devices, original path for multi-segment fixtures/files).
fn swap_amounts_from(swaps: &[SwapEntry]) -> Vec<SwapAmount> {
    swaps
        .iter()
        .map(|entry| SwapAmount {
            path: swap_row_path(entry),
            used_bytes: entry.used_kb.saturating_mul(1024),
            free_bytes: entry
                .size_kb
                .saturating_sub(entry.used_kb)
                .saturating_mul(1024),
        })
        .collect()
}

fn role_name(role: SwapTierRole) -> &'static str {
    match role {
        SwapTierRole::Zram => "zram",
        SwapTierRole::Vram => "vram",
        SwapTierRole::Fallback => "fallback",
    }
}

/// Compare the operator's plan with the post-admission plan. Identity and the
/// sorted higher-tier set must match; used counters may have grown.
fn ensure_same_plan(first: &RebalancePlan, fresh: &RebalancePlan) -> Result<(), CascadeError> {
    let changed = |field: &str| {
        CascadeError::Precondition(format!(
            "fallback rebalance plan changed before effect: {field}"
        ))
    };
    if first.target.path != fresh.target.path {
        return Err(changed("target path"));
    }
    if first.target.uuid != fresh.target.uuid {
        return Err(changed("target UUID"));
    }
    if first.target.dev_t != fresh.target.dev_t {
        return Err(changed("target device number"));
    }
    if first.target.priority != fresh.target.priority {
        return Err(changed("target priority"));
    }
    if first.target.size_bytes != fresh.target.size_bytes {
        return Err(changed("target size"));
    }
    if first.target.last_resort != fresh.target.last_resort {
        return Err(changed("last-resort relation"));
    }
    let identity = |plan: &RebalancePlan| {
        plan.higher_tiers
            .iter()
            .map(|tier| (tier.path.clone(), role_name(tier.role), tier.priority))
            .collect::<Vec<_>>()
    };
    if identity(first) != identity(fresh) {
        return Err(changed("higher-tier set"));
    }
    Ok(())
}

/// Best-effort restore guard. It is armed only once the target may be gone and
/// disarmed as soon as the target is proven active or the explicit restore has
/// run; it exists for an unwinding panic between those points. It only ever
/// names this UUID or this canonical path — never `swapon -a`.
struct RestoreGuard<'a, R: CommandRunner> {
    runner: &'a R,
    uuid: String,
    priority: i32,
    path: String,
    armed: bool,
}

impl<'a, R: CommandRunner> RestoreGuard<'a, R> {
    fn new(runner: &'a R, target: &RebalanceTarget) -> Self {
        Self {
            runner,
            uuid: target.uuid.clone(),
            priority: target.priority,
            path: target.path.clone(),
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn swapon_uuid(&self) -> Result<String, CascadeError> {
        let priority = self.priority.to_string();
        let uuid = format!("UUID={}", self.uuid);
        self.runner.run_bounded(
            "swapon",
            &["-p", priority.as_str(), uuid.as_str()],
            FALLBACK_SWAPON_TIMEOUT,
        )
    }

    fn swapon_path(&self) -> Result<String, CascadeError> {
        let priority = self.priority.to_string();
        self.runner.run_bounded(
            "swapon",
            &["-p", priority.as_str(), self.path.as_str()],
            FALLBACK_SWAPON_TIMEOUT,
        )
    }

    /// Restore by UUID, then once by the exact canonical path. Any other
    /// device is never named.
    fn restore_explicit(&self) -> Result<(), CascadeError> {
        match self.swapon_uuid() {
            Ok(_) => Ok(()),
            Err(uuid_error) => self.swapon_path().map(|_| ()).map_err(|path_error| {
                CascadeError::Precondition(format!(
                    "restore by UUID failed: {uuid_error}; restore by path failed: {path_error}"
                ))
            }),
        }
    }
}

impl<R: CommandRunner> Drop for RestoreGuard<'_, R> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.swapon_uuid();
            self.armed = false;
        }
    }
}

fn manual_restore_command(target: &RebalanceTarget) -> String {
    format!("swapon -p {} UUID={}", target.priority, target.uuid)
}

/// A row matches its target by the `/proc/swaps` spelling or by the canonical
/// `/dev/...` spelling. Production rows are already canonical; the raw
/// comparison keeps a test fixture path usable without a fake `/dev` tree.
fn row_matches(entry: &SwapEntry, path: &str) -> bool {
    entry.filename == path || entry.canonical_path() == path
}

fn row_for<'a>(rows: &'a [SwapEntry], path: &str) -> Option<&'a SwapEntry> {
    rows.iter().find(|entry| row_matches(entry, path))
}

fn rows_signature(rows: &[SwapEntry], excluded_path: Option<&str>) -> Vec<(String, i32)> {
    let mut signature = Vec::new();
    for entry in rows {
        let path = entry.canonical_path();
        if excluded_path.is_some_and(|excluded| row_matches(entry, excluded)) {
            continue;
        }
        signature.push((path, entry.priority));
    }
    signature.sort();
    signature
}

fn verify_restored(target: &RebalanceTarget, entry: &SwapEntry) -> Option<&'static str> {
    if !row_matches(entry, &target.path) {
        return Some("restored row path differs");
    }
    if entry.priority != target.priority {
        return Some("restored row priority differs");
    }
    if entry.size_kb.saturating_mul(1024) != target.size_bytes {
        return Some("restored row size differs");
    }
    if entry.used_kb != 0 {
        return Some("restored row is not empty");
    }
    match fs::metadata(&target.path) {
        Ok(metadata) => {
            if linux_device_number(metadata.rdev()) != target.dev_t {
                return Some("restored device number differs");
            }
        }
        Err(_) => return Some("restored device node is unreadable"),
    }
    None
}

fn outcome(
    target: &RebalanceTarget,
    target_after_used_bytes: Option<u64>,
    swapoff_error: Option<String>,
    swapon_error: Option<String>,
    restored: bool,
    applied: bool,
    result: &'static str,
) -> RebalanceOutcome {
    RebalanceOutcome {
        target_before: target.clone(),
        target_after_used_bytes,
        swapoff_error,
        swapon_error,
        restored,
        applied,
        result,
    }
}

/// Execute exactly one `swapoff UUID=<uuid>` followed by one `swapon -p
/// <priority> UUID=<uuid>`, proving the target absence and the restored row.
///
/// `read_swap_snapshot` is the strict `/proc/swaps` reader; the seam exists so
/// the exact command sequence and every restore branch are unit-testable
/// without touching a live fallback.
fn execute_plan_with_progress<R, S>(
    runner: &R,
    read_swap_snapshot: &mut S,
    target: &RebalanceTarget,
    progress: &crate::operation_progress::OperationProgressReporter,
) -> Result<RebalanceOutcome, CascadeError>
where
    R: CommandRunner,
    S: FnMut() -> Result<Vec<SwapEntry>, CascadeError>,
{
    if target.used_bytes == 0 {
        return Ok(outcome(
            target,
            Some(0),
            None,
            None,
            false,
            false,
            "already_balanced",
        ));
    }

    let before = read_swap_snapshot()?;
    let before_signature = rows_signature(&before, Some(&target.path));
    let mut guard = RestoreGuard::new(runner, target);

    let tracker = crate::operation_progress::SwapoffProgressTracker::start(
        progress.clone(),
        "swapoff",
        1,
        target.path.clone(),
        target.used_bytes,
    );
    let swapoff = runner.run_bounded(
        "swapoff",
        &[format!("UUID={}", target.uuid).as_str()],
        FALLBACK_SWAPOFF_TIMEOUT,
    );
    tracker.finish();
    let swapoff_error = swapoff.err().map(|error| error.to_string());
    if swapoff_error.is_none() {
        // A successful command can still be followed by an unreadable proof;
        // arm before the read so that failure path can restore.
        guard.arm();
    }

    match read_swap_snapshot() {
        Ok(rows) => {
            if let Some(active) = row_for(&rows, &target.path) {
                guard.disarm();
                return Ok(outcome(
                    target,
                    Some(active.used_kb.saturating_mul(1024)),
                    swapoff_error,
                    None,
                    false,
                    false,
                    "failed",
                ));
            }
            guard.arm();
            let restore = guard.restore_explicit();
            guard.disarm();
            if let Err(error) = restore {
                return Err(CascadeError::UnsafeContainment(format!(
                    "the fallback {} is absent and could not be restored ({error}); run `{}` manually",
                    target.path,
                    manual_restore_command(target)
                )));
            }
            let rows = match read_swap_snapshot() {
                Ok(rows) => rows,
                Err(error) => {
                    return Ok(outcome(
                        target,
                        None,
                        swapoff_error,
                        Some(error.to_string()),
                        true,
                        false,
                        "failed",
                    ));
                }
            };
            let Some(restored) = row_for(&rows, &target.path) else {
                return Ok(outcome(
                    target,
                    None,
                    swapoff_error,
                    None,
                    true,
                    false,
                    "failed",
                ));
            };
            if let Some(reason) = verify_restored(target, restored) {
                return Ok(outcome(
                    target,
                    Some(restored.used_kb.saturating_mul(1024)),
                    swapoff_error,
                    Some(reason.into()),
                    true,
                    false,
                    "failed",
                ));
            }
            if rows_signature(&rows, Some(&target.path)) != before_signature {
                return Ok(outcome(
                    target,
                    Some(restored.used_kb.saturating_mul(1024)),
                    swapoff_error,
                    Some("a non-target swap row changed while the fallback was drained".into()),
                    true,
                    false,
                    "failed",
                ));
            }
            let result = if swapoff_error.is_some() {
                "degraded"
            } else {
                "applied"
            };
            Ok(outcome(
                target,
                Some(0),
                swapoff_error,
                None,
                true,
                result == "applied",
                result,
            ))
        }
        Err(snapshot_error) => {
            // The absence proof is unreadable; the target may be gone. Attempt
            // the explicit restore and report critical, never a clean apply.
            guard.arm();
            let restore = guard.restore_explicit();
            guard.disarm();
            match restore {
                Ok(()) => Ok(outcome(
                    target,
                    None,
                    swapoff_error,
                    Some(snapshot_error.to_string()),
                    true,
                    false,
                    "critical",
                )),
                Err(restore_error) => Err(CascadeError::UnsafeContainment(format!(
                    "the post-swapoff /proc/swaps proof failed ({snapshot_error}) and the fallback {} could not be restored ({restore_error}); run `{}` manually",
                    target.path,
                    manual_restore_command(target)
                ))),
            }
        }
    }
}

/// Plan an already-admitted fresh state, then run the effect. The seam owns
/// the ordering that matters: no-op before custody, blocked before custody,
/// custody before the fresh observation, and the effect only after
/// `ensure_same_plan`.
fn apply_plan_with_progress<R, A, D, O, S>(
    runner: &R,
    first: RebalancePlan,
    admit: A,
    observe_fresh: O,
    read_swap_snapshot: &mut S,
    progress: &crate::operation_progress::OperationProgressReporter,
) -> Result<RebalanceOutcome, CascadeError>
where
    R: CommandRunner,
    A: FnOnce() -> Result<D, CascadeError>,
    O: FnOnce() -> Result<RebalancePlan, CascadeError>,
    S: FnMut() -> Result<Vec<SwapEntry>, CascadeError>,
{
    if first.target.used_bytes == 0 {
        return Ok(outcome(
            &first.target,
            Some(0),
            None,
            None,
            false,
            false,
            "already_balanced",
        ));
    }
    if first.verdict != HeadroomVerdict::Safe {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance is blocked: {}",
            first.reason
        )));
    }
    let _custody = admit()?;
    let fresh = observe_fresh()?;
    ensure_same_plan(&first, &fresh)?;
    if fresh.target.used_bytes == 0 {
        return Ok(outcome(
            &fresh.target,
            Some(0),
            None,
            None,
            false,
            false,
            "already_balanced",
        ));
    }
    if fresh.verdict != HeadroomVerdict::Safe {
        return Err(CascadeError::Precondition(format!(
            "fallback rebalance is blocked: {}",
            fresh.reason
        )));
    }
    execute_plan_with_progress(runner, read_swap_snapshot, &fresh.target, progress)
}

#[cfg(test)]
mod tests;
