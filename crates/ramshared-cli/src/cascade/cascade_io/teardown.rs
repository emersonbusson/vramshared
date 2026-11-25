// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/teardown.rs
//! Teardown: the plan and its execution for bringing a cascade down.
//!
//! Extracted from `cascade_io.rs` as one band of the boundary plan in
//! `workspace module layout`. Teardown is ordered, not
//! sequential guesswork: swapoff comes before the NBD disconnect, the daemon
//! stops before its socket goes, and every step is planned before any of it
//! runs. A step that cannot be proven complete leaves the ownership records
//! in place rather than declaring the device free.

use super::emergency_swap::{EmergencySwap, create as create_emergency_swap};
use super::swapoff_headroom::{self, HeadroomDecision, SwapAmount};
use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NbdLifecycleAction {
    Swapoff(BoundDeviceIdentity),
    ResetZram(BoundDeviceIdentity),
    DisconnectNbd(BoundDeviceIdentity),
    StopDaemon,
}

/// Ordered, injected teardown contract for managed swap and NBD ownership.
///
/// Every swapoff action precedes every NBD disconnect and daemon stop. The
/// executor stops at the first swapoff error, so later ownership-changing
/// actions cannot run after a failed swapoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NbdLifecyclePlan {
    actions: Vec<NbdLifecycleAction>,
}

/// Constructs the local teardown sequence from already-authorized paths.
///
/// Callers provide the swap, zram-reset, and NBD targets separately so the
/// ordering contract is testable without querying a live host.
pub(super) fn plan_nbd_lifecycle(
    binding: &LifecycleBinding,
    swaps: &[SwapEntry],
    live_devices: &[BoundDeviceIdentity],
) -> Result<NbdLifecyclePlan, CascadeError> {
    validate_lifecycle_binding(binding)?;
    prove_live_devices_are_bound(binding, live_devices)?;
    // The plan acts only on devices that are still live. A device the product
    // already released (the zram reset, the export disconnected) has nothing
    // left to release, and planning its action again made a partial teardown
    // unrecoverable: the reset/disconnect of an already-released device fails,
    // so the plan could never reach the daemon stop.
    let mut actions = Vec::new();
    let swap_paths: std::collections::HashSet<String> =
        swaps.iter().map(|entry| entry.canonical_path()).collect();
    for device in live_devices {
        if swap_paths.contains(&device.path) {
            actions.push(NbdLifecycleAction::Swapoff(device.clone()));
        }
    }
    for kind in [ManagedDeviceKind::Zram, ManagedDeviceKind::Nbd] {
        for device in live_devices.iter().filter(|device| device.kind == kind) {
            match kind {
                ManagedDeviceKind::Zram => {
                    actions.push(NbdLifecycleAction::ResetZram(device.clone()))
                }
                ManagedDeviceKind::Nbd => {
                    actions.push(NbdLifecycleAction::DisconnectNbd(device.clone()))
                }
                ManagedDeviceKind::Ublk => unreachable!("validated NBD binding excludes ublk"),
            }
        }
    }
    actions.push(NbdLifecycleAction::StopDaemon);
    Ok(NbdLifecyclePlan { actions })
}

/// Narrow side-effect boundary for the pure NBD lifecycle plan.
pub(super) trait NbdLifecycleExecutor {
    fn swapoff(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError>;
    fn reset_zram(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError>;
    fn disconnect_nbd(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError>;
    fn stop_daemon(&self) -> Result<(), CascadeError>;
}

/// Runs an injected plan. A swapoff failure is terminal for this invocation:
/// it cannot fall through to NBD disconnect or daemon stop.
pub(super) fn execute_nbd_lifecycle_plan<E: NbdLifecycleExecutor>(
    plan: &NbdLifecyclePlan,
    executor: &E,
) -> Result<(), CascadeError> {
    for action in &plan.actions {
        match action {
            NbdLifecycleAction::Swapoff(device) => executor.swapoff(device)?,
            NbdLifecycleAction::ResetZram(device) => executor.reset_zram(device)?,
            NbdLifecycleAction::DisconnectNbd(device) => executor.disconnect_nbd(device)?,
            NbdLifecycleAction::StopDaemon => executor.stop_daemon()?,
        }
    }
    Ok(())
}

pub(super) struct RuntimeNbdLifecycleExecutor<'a, R> {
    runner: &'a R,
    paths: &'a RuntimePaths,
    binding: &'a LifecycleBinding,
    /// Whether the peer that owns the origin tier is still the one the
    /// binding names. A released peer only permits the bounded recovery.
    peer: PeerState,
    /// The transient drain absorber, when one was created.
    emergency: Option<&'a EmergencySwap>,
    progress: crate::operation_progress::OperationProgressReporter,
    current_step: std::sync::atomic::AtomicU32,
}

impl<'a, R> RuntimeNbdLifecycleExecutor<'a, R> {
    pub(super) fn new(
        runner: &'a R,
        paths: &'a RuntimePaths,
        binding: &'a LifecycleBinding,
        peer: PeerState,
        emergency: Option<&'a EmergencySwap>,
    ) -> Self {
        Self {
            runner,
            paths,
            binding,
            peer,
            emergency,
            progress: crate::operation_progress::OperationProgressReporter::no_op(),
            current_step: std::sync::atomic::AtomicU32::new(0),
        }
    }

    pub(super) fn with_progress(
        mut self,
        progress: crate::operation_progress::OperationProgressReporter,
    ) -> Self {
        self.progress = progress;
        self
    }

    fn next_step(&self) -> u32 {
        self.current_step
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    fn emit_step(&self, phase: &str, target: Option<&str>) {
        let step = self.next_step();
        self.progress
            .update(crate::operation_progress::ProgressUpdate {
                phase,
                step_index: step,
                target,
                completed_bytes: 0,
                total_bytes: 0,
                throughput_mib_per_s: None,
            });
    }
}

impl<R: CommandRunner> NbdLifecycleExecutor for RuntimeNbdLifecycleExecutor<'_, R> {
    fn swapoff(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        let step = self.next_step();
        let swaps = read_swaps()?;
        authorize_bound_environment_for(
            self.binding,
            self.paths,
            &swaps,
            &detect_live_managed_devices()?,
            self.peer,
        )?;
        let emergency = self
            .emergency
            .map(|swap| swap.free_bytes(&swaps))
            .transpose()?
            .unwrap_or(0);
        let host = live_host_memory(self.paths, self.binding, self.peer);
        let headroom = headroom_for_device_with_absorber_and_host(device, &swaps, emergency, host)?;
        if !headroom.is_safe() {
            return Err(CascadeError::Precondition(headroom.refusal_message()));
        }
        let initial_used_bytes = swaps
            .iter()
            .find(|entry| entry.canonical_path() == device.path)
            .map(|entry| entry.used_kb.saturating_mul(1024))
            .unwrap_or(0);
        let pinned = bind_device_for_effect(device)?;
        let tracker = crate::operation_progress::SwapoffProgressTracker::start(
            self.progress.clone(),
            "swapoff",
            step,
            device.path.clone(),
            initial_used_bytes,
        );
        let result = self.runner.native_swapoff(
            Path::new(pinned.path()),
            self.emergency.map(EmergencySwap::backing),
        );
        tracker.finish();
        match result {
            Ok(()) => {
                prove_exact_swap_absent(device)?;
                pinned.revalidate_named_identity()?;
                eprintln!("[down] swapoff ok: {}", device.path);
                Ok(())
            }
            Err(error) => {
                // Whether the kernel refused or the device was already gone is
                // proven by `/proc/swaps`, never by a translated `strerror`.
                // An absent target is an idempotent success; a still-armed one
                // stops the plan and preserves the backend.
                let absent = prove_exact_swap_absent(device).is_ok();
                if absent {
                    eprintln!(
                        "[down] swapoff skip (freshly proven absent): {}",
                        device.path
                    );
                    Ok(())
                } else {
                    Err(CascadeError::UnsafeContainment(format!(
                        "swapoff for {} is uncertain or still active ({error}); backend preserved",
                        device.path
                    )))
                }
            }
        }
    }

    fn reset_zram(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        self.emit_step("reset_zram", Some(&device.path));
        let swaps = read_swaps()?;
        let live_before = detect_live_managed_devices()?;
        authorize_bound_environment_for(self.binding, self.paths, &swaps, &live_before, self.peer)?;
        prove_exact_swap_absent(device)?;
        let pinned = bind_device_for_effect(device)?;
        pinned.revalidate_named_identity()?;
        // zramctl does not portably accept a proc-fd target because it derives
        // the sysfs name from /dev/<name>. The kernel zram driver forbids reset
        // while any process holds an open file descriptor (returning EBUSY).
        // Revalidate the exact descriptor first, then close before invoking reset.
        drop(pinned);
        self.runner.native_zram_reset(Path::new(&device.path))?;
        let expected_after = live_before
            .iter()
            .filter(|expected| expected.kind != ManagedDeviceKind::Zram)
            .cloned()
            .collect::<Vec<_>>();
        let live_after = detect_live_managed_devices()?;
        authorize_bound_environment_for(
            self.binding,
            self.paths,
            &read_swaps()?,
            &live_after,
            self.peer,
        )?;
        prove_exact_live_device_set(self.binding, &live_after, &expected_after).map_err(
            |error| {
                CascadeError::UnsafeContainment(format!(
                    "zram reset completion is unproven for {}; lifecycle evidence retained ({error})",
                    device.path
                ))
            },
        )?;
        Ok(())
    }

    fn disconnect_nbd(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        self.emit_step("disconnect_nbd", Some(&device.path));
        let swaps = read_swaps()?;
        let live = detect_live_managed_devices()?;
        authorize_bound_environment_for(self.binding, self.paths, &swaps, &live, self.peer)?;
        prove_exact_swap_absent(device)?;
        if self.peer == PeerState::Released && !live.contains(device) {
            // Enumeration alone is not proof: require the exact kernel/node
            // identity and owner, size, holder and swap absence again.
            return released_peer::prove_detached_bound_nbd(device);
        }
        let pinned = bind_device_for_effect(device)?;
        pinned.revalidate_named_identity()?;
        // nbd-client likewise derives NBD sysfs state from the canonical name;
        // revalidate the exact descriptor first, then close before invoking disconnect.
        drop(pinned);
        self.runner.run("nbd-client", &["-d", &device.path])?;
        // The command acknowledges the request; the kernel clears the owner
        // `pid` asynchronously. Prove the completion over a bounded settle
        // window instead of losing a successful teardown to that gap.
        if self.peer == PeerState::Released {
            // The device node stays after a released client, so the ordered
            // postcondition (an empty live set) cannot hold here. The
            // released postcondition is the one that matters: the device is
            // not an active swap, has no owner and has no holders.
            released_peer::prove_detached_bound_nbd(device).map_err(|error| {
                CascadeError::UnsafeContainment(format!(
                    "NBD disconnect completion is unproven for {}; lifecycle evidence retained ({error})",
                    device.path
                ))
            })?;
            return Ok(());
        }
        prove_exact_live_device_set_settled(self.binding, self.paths, &[]).map_err(|error| {
            CascadeError::UnsafeContainment(format!(
                "NBD disconnect completion is unproven for {}; lifecycle evidence retained ({error})",
                device.path
            ))
        })?;
        Ok(())
    }

    fn stop_daemon(&self) -> Result<(), CascadeError> {
        self.emit_step("stop_daemon", Some("ramsharedd"));
        let swaps = read_swaps()?;
        let live = detect_live_managed_devices()?;
        authorize_bound_environment_for(self.binding, self.paths, &swaps, &live, self.peer)?;
        if self.peer == PeerState::Released {
            prove_exact_live_device_set(self.binding, &live, &[])?;
            for device in self
                .binding
                .devices
                .iter()
                .filter(|device| device.kind == ManagedDeviceKind::Nbd)
            {
                released_peer::prove_detached_bound_nbd(device)?;
            }
            return Ok(());
        }
        prove_exact_live_device_set(self.binding, &live, &[])?;
        stop_daemon_gracefully_at(self.paths, Duration::from_secs(10))
    }
}

/// Canonical paths the plan will actually swap off.
fn planned_swapoff_paths(plan: &NbdLifecyclePlan) -> Vec<String> {
    plan.actions
        .iter()
        .filter_map(|action| match action {
            NbdLifecycleAction::Swapoff(device) => Some(device.path.clone()),
            _ => None,
        })
        .collect()
}

fn swap_amounts(swaps: &[SwapEntry]) -> Vec<SwapAmount> {
    swaps
        .iter()
        .map(|entry| SwapAmount {
            path: entry.canonical_path(),
            used_bytes: entry.used_kb.saturating_mul(1024),
            free_bytes: entry
                .size_kb
                .saturating_sub(entry.used_kb)
                .saturating_mul(1024),
        })
        .collect()
}

#[cfg(test)]
fn headroom_for_plan_with_mem_and_absorber(
    plan: &NbdLifecyclePlan,
    swaps: &[SwapEntry],
    emergency_bytes: u64,
    mem: swapoff_headroom::MemInfo,
) -> HeadroomDecision {
    let (managed, remaining) =
        swapoff_headroom::summarize_swaps(&planned_swapoff_paths(plan), &swap_amounts(swaps));
    swapoff_headroom::evaluate_with_absorber(managed, remaining, emergency_bytes, mem)
}

fn live_host_memory(
    paths: &RuntimePaths,
    binding: &LifecycleBinding,
    peer: PeerState,
) -> Option<ramshared_tier::HostDrainMemory> {
    if peer != PeerState::Live {
        return None;
    }
    swapoff_headroom::verified_live_host_memory(
        &paths.cache_status_file,
        &binding.daemon_instance_id,
        unix_time_ms(),
    )
}

fn headroom_for_targets(
    planned: &[String],
    swaps: &[SwapEntry],
    emergency_bytes: u64,
    host: Option<ramshared_tier::HostDrainMemory>,
) -> Result<HeadroomDecision, CascadeError> {
    let (mem, free_bytes) = swapoff_headroom::read_meminfo_with_free().map_err(|reason| {
        CascadeError::Precondition(format!("swapoff headroom unavailable: {reason}"))
    })?;
    let (managed, remaining) = swapoff_headroom::summarize_swaps(planned, &swap_amounts(swaps));
    Ok(swapoff_headroom::evaluate_for_platform(
        managed,
        remaining,
        emergency_bytes,
        mem,
        free_bytes,
        swapoff_headroom::observed_platform(),
        !planned.is_empty(),
        host,
    ))
}

fn headroom_for_plan(
    plan: &NbdLifecyclePlan,
    swaps: &[SwapEntry],
) -> Result<HeadroomDecision, CascadeError> {
    headroom_for_plan_with_absorber_and_host(plan, swaps, 0, None)
}

fn headroom_for_plan_with_absorber_and_host(
    plan: &NbdLifecyclePlan,
    swaps: &[SwapEntry],
    emergency_bytes: u64,
    host: Option<ramshared_tier::HostDrainMemory>,
) -> Result<HeadroomDecision, CascadeError> {
    headroom_for_targets(&planned_swapoff_paths(plan), swaps, emergency_bytes, host)
}

#[cfg(test)]
fn headroom_for_device_with_absorber(
    device: &BoundDeviceIdentity,
    swaps: &[SwapEntry],
    emergency_bytes: u64,
) -> Result<HeadroomDecision, CascadeError> {
    headroom_for_device_with_absorber_and_host(device, swaps, emergency_bytes, None)
}

fn headroom_for_device_with_absorber_and_host(
    device: &BoundDeviceIdentity,
    swaps: &[SwapEntry],
    emergency_bytes: u64,
    host: Option<ramshared_tier::HostDrainMemory>,
) -> Result<HeadroomDecision, CascadeError> {
    headroom_for_targets(
        std::slice::from_ref(&device.path),
        swaps,
        emergency_bytes,
        host,
    )
}

pub(super) fn down_with_runtime<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
) -> Result<(), CascadeError> {
    let current_entries = read_swaps()?;
    let has_runtime_evidence = paths.pid_file.exists()
        || paths.swap_dev_file.exists()
        || paths.zram_dev_file.exists()
        || paths.socket.exists();
    let has_managed_swap = current_entries
        .iter()
        .any(SwapEntry::is_managed_or_orphan_vram_tier);
    if !paths.lifecycle_binding_file.exists() {
        if !has_runtime_evidence && !has_managed_swap {
            eprintln!("[down] no sealed lifecycle is present — no action");
            return Ok(());
        }
        return Err(CascadeError::Precondition(
            "runtime or managed-device evidence exists without an exact lifecycle binding; all devices are preserved"
                .into(),
        ));
    }
    let binding = read_lifecycle_binding(paths)?;
    let live_devices = detect_live_managed_devices()?;
    authorize_bound_environment(&binding, paths, &current_entries, &live_devices)?;
    let plan = plan_nbd_lifecycle(&binding, &current_entries, &live_devices)?;

    // SPEC swap-drain-safety RF-2/DT-3: the strict gate first. When the drain
    // cannot fit in RAM, create the transient absorber, credit only its free
    // space, and re-evaluate. The read-only `--plan` never creates one.
    let mut swaps = current_entries;
    let mut emergency: Option<EmergencySwap> = None;
    let mut headroom = headroom_for_plan_with_absorber_and_host(
        &plan,
        &swaps,
        0,
        live_host_memory(paths, &binding, PeerState::Live),
    )?;
    if !headroom.is_safe() {
        if !headroom.absorber_can_help() {
            return Err(CascadeError::Precondition(headroom.refusal_message()));
        }
        let shortfall = headroom.deficit_bytes;
        eprintln!(
            "[down] swapoff headroom: BLOCKED deficit={} B; creating a transient emergency swap",
            shortfall
        );
        let Some(created) = create_emergency_swap(runner, shortfall) else {
            return Err(CascadeError::Precondition(headroom.refusal_message()));
        };
        emergency = Some(created);
        swaps = match read_swaps() {
            Ok(swaps) => swaps,
            Err(error) => {
                if let Some(swap) = emergency.take() {
                    swap.remove(runner);
                }
                return Err(error);
            }
        };
        let absorber = emergency
            .as_ref()
            .map(|swap| swap.free_bytes(&swaps))
            .transpose()?
            .unwrap_or(0);
        headroom = match headroom_for_plan_with_absorber_and_host(
            &plan,
            &swaps,
            absorber,
            live_host_memory(paths, &binding, PeerState::Live),
        ) {
            Ok(headroom) => headroom,
            Err(error) => {
                if let Some(swap) = emergency.take() {
                    swap.remove(runner);
                }
                return Err(error);
            }
        };
        if !headroom.is_safe() {
            if let Some(swap) = emergency.take() {
                swap.remove(runner);
            }
            return Err(CascadeError::Precondition(headroom.refusal_message()));
        }
    }
    eprintln!(
        "[down] swapoff headroom: SAFE managed={} B available={} B emergency={} B remaining_free={} B reserve={} B margin={} B",
        headroom.managed_bytes,
        headroom.available_bytes,
        headroom.emergency_bytes,
        headroom.remaining_free_bytes,
        headroom.reserve_bytes,
        headroom.margin_bytes
    );

    // ALWAYS swapoff before NBD disconnect/daemon stop. A swapoff error returns
    // here, leaving runtime records, daemon, and NBD device untouched.
    let progress_guard = paths.start_operation_progress(
        crate::operation_progress::OperationKind::Teardown,
        "prepare",
        plan.actions.len().max(1) as u32,
    );
    let executor = RuntimeNbdLifecycleExecutor::new(
        runner,
        paths,
        &binding,
        PeerState::Live,
        emergency.as_ref(),
    )
    .with_progress(progress_guard.reporter());
    let outcome = execute_nbd_lifecycle_plan(&plan, &executor);
    drop(progress_guard);
    if let Some(swap) = emergency.take() {
        swap.remove(runner);
    }
    outcome?;

    super::runtime_cleanup::clear_released_runtime(paths)?;
    eprintln!("[down] cascade unmounted (swapoff-first, no broad kill)");
    Ok(())
}

/// One cascade teardown (RF-5's inverse).
///
/// Custody (ITEM-6): `down` is the activation lifecycle's own reverse, so it
/// takes the same [`crate::customer::transition::OperationOwner::Up`] custody
/// `up` does and is refused while another mutation or a package transaction is
/// in flight. Device and daemon effects stay with the existing executor.
/// Read-only `down --plan` surface. It performs the same authorization and
/// planning reads as `down`, then prints the headroom decision without any
/// mutation: no swapoff, no reset, no disconnect, no daemon stop, no file
/// removal.
pub fn down_plan(json: bool) -> Result<(), CascadeError> {
    let paths = RuntimePaths::system();
    down_plan_with_paths(&paths, json)
}

fn down_plan_with_paths(paths: &RuntimePaths, json: bool) -> Result<(), CascadeError> {
    let swaps = read_swaps()?;
    down_plan_with_swaps(paths, &swaps, json)
}

/// Read-only live drain-safety check reused by `down --plan` and seamless
/// auto-update. Returns `Ok(None)` when no sealed lifecycle or managed swap is
/// active, or `Ok(Some((is_safe, reason)))` for an active bound lifecycle.
pub(crate) fn evaluate_live_down_plan() -> Result<Option<(bool, String)>, CascadeError> {
    let paths = RuntimePaths::system();
    let swaps = read_swaps()?;
    Ok(evaluate_down_plan_with_swaps(&paths, &swaps)?
        .map(|decision| (decision.is_safe(), decision.reason.to_owned())))
}

fn evaluate_down_plan_with_swaps(
    paths: &RuntimePaths,
    swaps: &[SwapEntry],
) -> Result<Option<HeadroomDecision>, CascadeError> {
    if !paths.lifecycle_binding_file.exists() {
        let has_runtime_evidence = paths.pid_file.exists()
            || paths.swap_dev_file.exists()
            || paths.zram_dev_file.exists()
            || paths.socket.exists();
        let has_managed_swap = swaps.iter().any(SwapEntry::is_managed_or_orphan_vram_tier);
        if !has_runtime_evidence && !has_managed_swap {
            return Ok(None);
        }
        return Err(CascadeError::Precondition(
            "runtime or managed-device evidence exists without an exact lifecycle binding; all devices are preserved"
                .into(),
        ));
    }
    let binding = read_lifecycle_binding(paths)?;
    let live_devices = detect_live_managed_devices()?;
    authorize_bound_environment(&binding, paths, swaps, &live_devices)?;
    let plan = plan_nbd_lifecycle(&binding, swaps, &live_devices)?;
    let headroom = headroom_for_plan_with_absorber_and_host(
        &plan,
        swaps,
        0,
        live_host_memory(paths, &binding, PeerState::Live),
    )?;
    Ok(Some(headroom))
}

fn down_plan_with_swaps(
    paths: &RuntimePaths,
    swaps: &[SwapEntry],
    json: bool,
) -> Result<(), CascadeError> {
    let Some(headroom) = evaluate_down_plan_with_swaps(paths, swaps)? else {
        if json {
            println!("{{\"verdict\":\"no_lifecycle\"}}");
        } else {
            println!("no sealed lifecycle is present — no action");
        }
        return Ok(());
    };
    let rendered = render_plan(&headroom, json)?;
    println!("{rendered}");
    Ok(())
}

/// Renders the plan decision. Kept pure so the read-only surface can be
/// exercised without a live host and so the exact operator text is asserted
/// rather than eyeballed.
fn render_plan(decision: &HeadroomDecision, json: bool) -> Result<String, CascadeError> {
    if json {
        return serde_json::to_string(decision)
            .map_err(|error| CascadeError::Io(format!("encode swapoff plan: {error}")));
    }
    Ok(format!(
        "managed_swap={} B
available={} B
remaining_swap_free={} B (reported, not credited)
reserve={} B
margin={} B
required={} B
deficit={} B
verdict={:?}
reason={}",
        decision.managed_bytes,
        decision.available_bytes,
        decision.remaining_free_bytes,
        decision.reserve_bytes,
        decision.margin_bytes,
        decision.required_bytes,
        decision.deficit_bytes,
        decision.verdict,
        decision.reason
    ))
}

mod released_peer;
pub(super) use released_peer::down_recover_released_peer_with;
#[cfg(test)]
use released_peer::{plan_released_lifecycle, released_recovery_decision};

mod stale_recovery;
use stale_recovery::clear_stale_runtime_state;
#[cfg(test)]
use stale_recovery::device_is_present_or_armed;

/// The attended recovery entry point.
pub fn down_recover_released_peer() -> Result<(), CascadeError> {
    let _guard = super::mutation_guard::enter(crate::customer::transition::OperationOwner::Up)?;
    let runner = SystemCommandRunner;
    let paths = RuntimePaths::system();
    down_recover_released_peer_with(&runner, &paths)
}

pub fn down() -> Result<(), CascadeError> {
    down_quiet()?;
    status(false)
}

/// Ordered swapoff-first teardown without emitting status text to stdout.
/// Used by native lifecycle callers (such as `install --auto-update --json`)
/// that must preserve structured stdout contracts.
pub(crate) fn down_quiet() -> Result<(), CascadeError> {
    let _guard = super::mutation_guard::enter(crate::customer::transition::OperationOwner::Up)?;
    let runner = SystemCommandRunner;
    let paths = RuntimePaths::system();
    down_with_runtime(&runner, &paths)
}

#[cfg(test)]
#[path = "teardown/tests/mod.rs"]
mod tests;
