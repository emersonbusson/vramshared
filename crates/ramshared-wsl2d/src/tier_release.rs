// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson | LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared | File: crates/ramshared-wsl2d/src/tier_release.rs
//! Managed-tier release and transient swap custody.
//!
//! SPEC: `swap-drain-safety`. Crash recording stays in `crash_record`.

mod helper;
mod observation;
#[cfg(test)]
use helper::ChildObservation;
use helper::{observe_helper_until, system_helper_path};
pub use observation::{
    RoutineHostMemoryGuard, RoutineHostStorageGuard, register_routine_host_memory,
    register_routine_host_storage,
};
use observation::{
    has_routine_host_storage, observed_routine_host_memory, observed_routine_host_storage_free,
    parse_drain_absorbers, parse_swap_rows, read_proc,
};

use ramshared_block::swap_absorber::SwapAbsorber;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn drain_absorbers(
    device: &str,
    absorber: Option<&SwapAbsorber>,
    io: &mut impl ReleaseIo,
) -> Option<ramshared_tier::DrainDecision> {
    let wsl_host = io.wsl_host_memory();
    parse_drain_absorbers(
        &io.read_proc("/proc/meminfo")?,
        &io.read_proc("/proc/swaps")?,
        device,
    )?
    .decision_with_host(absorber, wsl_host)
}

/// Free bytes on `root`'s filesystem (best-effort; `0` on any error).
fn free_bytes(root: &std::path::Path) -> u64 {
    rustix::fs::statvfs(root)
        .map(|stat| {
            let frsize = if stat.f_frsize == 0 {
                stat.f_bsize
            } else {
                stat.f_frsize
            };
            stat.f_bavail.saturating_mul(frsize)
        })
        .unwrap_or(0)
}

/// Candidate roots for the emergency absorber (SPEC RF-2/RF-3).
fn emergency_swap_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![std::path::PathBuf::from("/")];
    for letter in b'a'..=b'z' {
        let path = format!("/mnt/{}", (letter as char).to_ascii_lowercase());
        if path != "/mnt/c" {
            candidates.push(std::path::PathBuf::from(path));
        }
    }
    candidates
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HelperOutcome {
    Success,
    Failed,
    Uncertain,
    NotStarted,
}

/// The release owner has one in-flight operation, never an unbounded queue.
#[derive(Default)]
struct ReleaseCustody {
    child: Option<Child>,
    absorber: Option<SwapAbsorber>,
    pending_target: Option<String>,
    cleanup_waits_for_memory: bool,
}

/// Completion of an owned drain, including its transient absorber custody.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseOutcome {
    Released,
    Refused,
    Pending,
}

/// Exclusive flight identity. Keep this handle until its outcome is terminal.
/// Dropping it parks unresolved custody; it never cancels an operation.
pub struct ReleaseFlight<'a> {
    device: String,
    lease: ReleaseLease<'a>,
    started: bool,
}

fn valid_target(device: &str) -> bool {
    Path::new(device).is_absolute()
        && !device.contains('\0')
        && device
            .split('/')
            .skip(1)
            .all(|component| !matches!(component, "" | "." | ".."))
}

fn begin_release_in<'a>(slot: &'a Mutex<CustodyState>, device: &str) -> Option<ReleaseFlight<'a>> {
    if !valid_target(device) {
        return None;
    }
    Some(ReleaseFlight {
        device: device.to_string(),
        lease: ReleaseLease::claim_idle(slot)?,
        started: false,
    })
}

pub fn try_begin_release(device: &str) -> Option<ReleaseFlight<'static>> {
    begin_release_in(&RELEASE_CUSTODY, device)
}

enum CustodyState {
    Idle,
    Running,
    Retained(ReleaseCustody),
}

static RELEASE_CUSTODY: Mutex<CustodyState> = Mutex::new(CustodyState::Idle);

fn custody_is_clear(slot: &Mutex<CustodyState>) -> bool {
    slot.try_lock()
        .is_ok_and(|state| matches!(*state, CustodyState::Idle))
}

/// Read-only teardown gate: contention or unresolved custody refuses shutdown.
pub fn release_custody_is_clear() -> bool {
    custody_is_clear(&RELEASE_CUSTODY)
}

fn reconcile_release_custody_in(
    slot: &Mutex<CustodyState>,
    read: &mut impl FnMut(&str) -> Option<String>,
) -> bool {
    let Some(mut lease) = ReleaseLease::claim(slot) else {
        return false;
    };
    if !reconcile_custody(&mut lease.custody, read) {
        return false;
    }
    if let Some(device) = lease.custody.pending_target.as_ref()
        && !swap_is_absent(read("/proc/swaps"), Path::new(device))
    {
        return false;
    }
    lease.custody.pending_target = None;
    true
}

/// Non-waiting child reconciliation; all filesystem effects stay outside locks.
pub fn reconcile_release_custody() -> bool {
    reconcile_release_custody_in(&RELEASE_CUSTODY, &mut read_proc)
}

/// Keep alive through explicit backend drop; release admission stays excluded.
pub struct BackendShutdownLease<'a> {
    _lease: ReleaseLease<'a>,
    _companion: Option<Box<dyn Send + Sync>>,
}

impl BackendShutdownLease<'_> {
    /// Companion custody follows every clone of the same backend-stop Arc.
    /// Drop happens outside the release-slot lock, after backend destruction.
    pub fn retain_shutdown_guard(mut self, guard: impl Send + Sync + 'static) -> Self {
        self._companion = Some(Box::new((self._companion.take(), guard)));
        self
    }
}

fn claim_backend_shutdown_from(slot: &Mutex<CustodyState>) -> Option<BackendShutdownLease<'_>> {
    Some(BackendShutdownLease {
        _lease: ReleaseLease::claim_idle(slot)?,
        _companion: None,
    })
}

fn claim_backend_shutdown_for_in<'a>(
    slot: &'a Mutex<CustodyState>,
    device: &str,
    read: &mut impl FnMut(&str) -> Option<String>,
) -> Option<BackendShutdownLease<'a>> {
    if !valid_target(device) {
        return None;
    }
    let lease = claim_backend_shutdown_from(slot)?;
    swap_is_absent(read("/proc/swaps"), Path::new(device)).then_some(lease)
}

/// Claims the exclusive stop frontier and then proves this exact target absent.
pub fn try_claim_backend_shutdown_for(device: &str) -> Option<BackendShutdownLease<'static>> {
    claim_backend_shutdown_for_in(&RELEASE_CUSTODY, device, &mut read_proc)
}

struct ReleaseLease<'a> {
    slot: &'a Mutex<CustodyState>,
    custody: ReleaseCustody,
}

impl<'a> ReleaseLease<'a> {
    fn claim_idle(slot: &'a Mutex<CustodyState>) -> Option<Self> {
        let mut state = slot.try_lock().ok()?;
        if !matches!(*state, CustodyState::Idle) {
            return None;
        }
        *state = CustodyState::Running;
        Some(Self {
            slot,
            custody: ReleaseCustody::default(),
        })
    }

    fn claim(slot: &'a Mutex<CustodyState>) -> Option<Self> {
        let mut state = slot.try_lock().ok()?;
        if matches!(*state, CustodyState::Running) {
            return None;
        }
        let custody = match std::mem::replace(&mut *state, CustodyState::Running) {
            CustodyState::Retained(custody) => custody,
            _ => ReleaseCustody::default(),
        };
        Some(Self { slot, custody })
    }
}

impl Drop for ReleaseLease<'_> {
    fn drop(&mut self) {
        // Only transfer ownership under the lock. No child, filesystem or
        // device effect belongs here; dropping a lease never removes storage.
        let mut state = self.slot.lock().unwrap_or_else(|error| error.into_inner());
        *state = if self.custody.child.is_some()
            || self.custody.absorber.is_some()
            || self.custody.pending_target.is_some()
        {
            CustodyState::Retained(std::mem::take(&mut self.custody))
        } else {
            CustodyState::Idle
        };
    }
}

fn swap_is_absent(text: Option<String>, path: &Path) -> bool {
    text.and_then(|text| parse_swap_rows(&text))
        .is_some_and(|rows| rows.iter().all(|(name, _, _)| Path::new(name) != path))
}

fn reconcile_custody(
    custody: &mut ReleaseCustody,
    read: &mut impl FnMut(&str) -> Option<String>,
) -> bool {
    if let Some(child) = custody.child.as_mut() {
        if !matches!(child.try_wait(), Ok(Some(_))) {
            return false;
        }
        custody.child = None;
    }
    if let Some(backing) = custody.absorber.as_ref() {
        if backing
            .remove_inactive(|| swap_is_absent(read("/proc/swaps"), backing.path()))
            .is_err()
        {
            return false;
        }
        custody.absorber = None;
    }
    true
}

/// Private domain effects. Production always supplies fixed roots and helpers.
trait ReleaseIo {
    fn drain_allowed(&self) -> bool;
    fn read_proc(&mut self, path: &str) -> Option<String>;
    fn run(&mut self, command: &str, args: &[&str], deadline: Instant) -> HelperOutcome;
    fn retain(&mut self, backing: SwapAbsorber);
    fn wait_for_cleanup_memory(&mut self);
    fn remove_unactivated(&mut self, backing: &SwapAbsorber) -> bool {
        backing.remove_inactive(|| true).is_ok()
    }
    fn candidates(&self) -> Vec<PathBuf> {
        emergency_swap_candidates()
    }
    fn free_space(&self, root: &Path) -> u64 {
        free_bytes(root)
    }
    fn wsl_host_memory(&self) -> Option<Option<ramshared_tier::HostDrainMemory>> {
        None
    }
}

struct SystemRelease<'a> {
    custody: &'a mut ReleaseCustody,
    mode: ReleaseMode,
}

#[derive(Clone, Copy)]
enum ReleaseMode {
    Routine,
    LastResort,
}

fn drain_allowed(
    mode: ReleaseMode,
    platform: Option<ramshared_block::platform::PlatformKind>,
) -> bool {
    matches!(mode, ReleaseMode::LastResort)
        || ramshared_block::swap_absorber::guest_only_capacity_is_admissible(platform)
}

impl ReleaseIo for SystemRelease<'_> {
    fn drain_allowed(&self) -> bool {
        let platform = ramshared_block::platform::probe_platform().platform;
        drain_allowed(self.mode, platform)
            || (matches!(
                platform,
                Some(
                    ramshared_block::platform::PlatformKind::Wsl1
                        | ramshared_block::platform::PlatformKind::Wsl2
                )
            ) && observed_routine_host_memory().is_some())
    }
    fn wsl_host_memory(&self) -> Option<Option<ramshared_tier::HostDrainMemory>> {
        let platform = ramshared_block::platform::probe_platform().platform;
        if drain_allowed(self.mode, platform) {
            None
        } else {
            Some(observed_routine_host_memory())
        }
    }
    fn candidates(&self) -> Vec<PathBuf> {
        let platform = ramshared_block::platform::probe_platform().platform;
        if ramshared_block::swap_absorber::guest_only_capacity_is_admissible(platform)
            || has_routine_host_storage()
        {
            emergency_swap_candidates()
        } else {
            eprintln!("[ramsharedd] emergency swap refused: host backing capacity is unverified");
            Vec::new()
        }
    }

    fn free_space(&self, root: &Path) -> u64 {
        let guest_free = free_bytes(root);
        let platform = ramshared_block::platform::probe_platform().platform;
        if ramshared_block::swap_absorber::guest_only_capacity_is_admissible(platform) {
            return guest_free;
        }
        observed_routine_host_storage_free(root, guest_free).unwrap_or(0)
    }

    fn read_proc(&mut self, path: &str) -> Option<String> {
        read_proc(path)
    }

    fn run(&mut self, command: &str, args: &[&str], deadline: Instant) -> HelperOutcome {
        if self.custody.child.is_some() || self.custody.absorber.is_some() {
            return HelperOutcome::Uncertain;
        }
        if Instant::now() >= deadline {
            return HelperOutcome::NotStarted;
        }
        let Some(program) = system_helper_path(command) else {
            return HelperOutcome::Failed;
        };
        let child = match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("[ramsharedd] tier helper {command} could not start: {error}");
                return HelperOutcome::Failed;
            }
        };
        self.custody.child = Some(child);
        let outcome = match self.custody.child.as_mut() {
            Some(child) => observe_helper_until(child, deadline),
            None => HelperOutcome::Uncertain,
        };
        if outcome != HelperOutcome::Uncertain {
            self.custody.child = None;
        }
        outcome
    }

    fn retain(&mut self, backing: SwapAbsorber) {
        eprintln!(
            "[ramsharedd] emergency backing retained in release custody: {}",
            backing.path().display()
        );
        self.custody.absorber = Some(backing);
        self.custody.cleanup_waits_for_memory = false;
    }

    fn wait_for_cleanup_memory(&mut self) {
        self.custody.cleanup_waits_for_memory = true;
    }
}

fn owned_helper(
    backing: &SwapAbsorber,
    command: &str,
    args: &[&str],
    deadline: Instant,
    io: &mut impl ReleaseIo,
) -> HelperOutcome {
    if backing.validate().is_err() {
        return HelperOutcome::Uncertain;
    }
    if Instant::now() >= deadline {
        return HelperOutcome::NotStarted;
    }
    let outcome = io.run(command, args, deadline);
    if backing.validate().is_err() {
        HelperOutcome::Uncertain
    } else {
        outcome
    }
}

/// Create the transient absorber; the caller re-reads its live swap capacity.
fn create_emergency_swap(
    bytes: u64,
    deadline: Instant,
    io: &mut impl ReleaseIo,
) -> Option<SwapAbsorber> {
    const MIB: u64 = 1024 * 1024;
    // `dd` allocates whole MiB blocks, so round the requested capacity up
    // before checking disk headroom. Overflow is a refusal, never a grant.
    let want = bytes.checked_add(64 * MIB)?.checked_add(MIB - 1)? / MIB * MIB;
    let required_free = want.checked_add(64 * MIB)?;
    for root in io.candidates() {
        if Instant::now() >= deadline {
            return None;
        }
        let free = io.free_space(&root);
        if free < required_free {
            continue;
        }
        let size = want;
        let path = root.join(format!(".ramshared-emergency-{}.swap", std::process::id()));
        let Ok(backing) = SwapAbsorber::reserve(&path) else {
            continue;
        };
        let path_text = backing.effect_path().display().to_string();
        let mut outcome = owned_helper(
            &backing,
            "fallocate",
            &["-l", &size.to_string(), &path_text],
            deadline,
            io,
        );
        if outcome == HelperOutcome::Failed {
            outcome = owned_helper(
                &backing,
                "dd",
                &[
                    "if=/dev/zero",
                    &format!("of={path_text}"),
                    "bs=1M",
                    &format!("count={}", size / MIB),
                ],
                deadline,
                io,
            );
        }
        if outcome == HelperOutcome::Success {
            outcome = owned_helper(&backing, "mkswap", &[&path_text], deadline, io);
        }
        match outcome {
            HelperOutcome::Failed | HelperOutcome::NotStarted => {
                if !io.remove_unactivated(&backing) {
                    io.retain(backing);
                    return None;
                }
                continue;
            }
            HelperOutcome::Uncertain => {
                io.retain(backing);
                return None;
            }
            HelperOutcome::Success => {}
        }
        if owned_helper(&backing, "swapon", &["-p", "0", &path_text], deadline, io)
            == HelperOutcome::Success
        {
            eprintln!("[ramsharedd] emergency swap created at {}", path.display());
            return Some(backing);
        }
        // Even a terminal activation failure cannot prove kernel inactivity.
        io.retain(backing);
        return None;
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupOutcome {
    Released,
    HeadroomBlocked,
    NotStarted,
    Retained,
}

fn remove_emergency_swap(
    backing: &SwapAbsorber,
    deadline: Instant,
    io: &mut impl ReleaseIo,
) -> CleanupOutcome {
    if !io.drain_allowed() {
        return CleanupOutcome::HeadroomBlocked;
    }
    if backing.validate().is_err() {
        return CleanupOutcome::Retained;
    }
    let wsl_host = io.wsl_host_memory();
    let decision = (|| {
        let observed = parse_drain_absorbers(
            &io.read_proc("/proc/meminfo")?,
            &io.read_proc("/proc/swaps")?,
            backing.path().to_str()?,
        )?;
        backing.free_bytes(
            observed
                .rows
                .iter()
                .map(|(name, size, used)| (name.as_str(), *size, *used)),
        )?;
        observed.decision_with_host(None, wsl_host)
    })();
    let Some(decision) = decision else {
        return CleanupOutcome::Retained;
    };
    if !decision.is_safe() {
        return CleanupOutcome::HeadroomBlocked;
    }
    match owned_helper(
        backing,
        "swapoff",
        &["--", &backing.effect_path().display().to_string()],
        deadline,
        io,
    ) {
        HelperOutcome::Success
            if backing
                .remove_inactive(|| swap_is_absent(io.read_proc("/proc/swaps"), backing.path()))
                .is_ok() =>
        {
            CleanupOutcome::Released
        }
        HelperOutcome::NotStarted => CleanupOutcome::NotStarted,
        _ => CleanupOutcome::Retained,
    }
}

fn target_release_outcome(text: Option<String>, device: &str) -> ReleaseOutcome {
    match text.and_then(|text| parse_swap_rows(&text)) {
        Some(rows) if rows.iter().any(|(name, _, _)| name == device) => ReleaseOutcome::Refused,
        Some(_) => ReleaseOutcome::Released,
        None => ReleaseOutcome::Pending,
    }
}

fn release_managed_tier_with(
    device: &str,
    deadline: Instant,
    io: &mut impl ReleaseIo,
) -> ReleaseOutcome {
    if Instant::now() >= deadline {
        return ReleaseOutcome::Refused;
    }
    if !io.drain_allowed() {
        eprintln!("[ramsharedd] tier release refused: host memory is unverified");
        return ReleaseOutcome::Refused;
    }
    let Some(decision) = drain_absorbers(device, None, io) else {
        eprintln!("[ramsharedd] tier release refused: memory or exact target is unobserved");
        return ReleaseOutcome::Refused;
    };
    let emergency = if decision.is_safe() {
        None
    } else {
        if !decision.absorber_can_help() {
            eprintln!("[ramsharedd] tier release refused: {}", decision.reason);
            return ReleaseOutcome::Refused;
        }
        let Some(backing) = create_emergency_swap(decision.deficit_bytes, deadline, io) else {
            return ReleaseOutcome::Refused;
        };
        if !drain_absorbers(device, Some(&backing), io).is_some_and(|decision| decision.is_safe()) {
            io.retain(backing);
            return ReleaseOutcome::Pending;
        }
        Some(backing)
    };
    let outcome = if Instant::now() >= deadline {
        ReleaseOutcome::Refused
    } else {
        match io.run("swapoff", &["--", device], deadline) {
            HelperOutcome::Success => target_release_outcome(io.read_proc("/proc/swaps"), device),
            HelperOutcome::Failed | HelperOutcome::NotStarted => ReleaseOutcome::Refused,
            HelperOutcome::Uncertain => ReleaseOutcome::Pending,
        }
    };
    if let Some(backing) = emergency {
        let cleanup = if outcome == ReleaseOutcome::Released {
            remove_emergency_swap(&backing, deadline, io)
        } else {
            CleanupOutcome::Retained
        };
        if cleanup != CleanupOutcome::Released {
            io.retain(backing);
            if cleanup == CleanupOutcome::HeadroomBlocked {
                io.wait_for_cleanup_memory();
            }
            return ReleaseOutcome::Pending;
        }
    }
    eprintln!("[ramsharedd] managed tier release outcome: {outcome:?}");
    outcome
}

fn record_release_outcome(
    flight: &mut ReleaseFlight<'_>,
    outcome: ReleaseOutcome,
) -> ReleaseOutcome {
    flight.started = true;
    let outcome = if flight.lease.custody.child.is_some() || flight.lease.custody.absorber.is_some()
    {
        ReleaseOutcome::Pending
    } else {
        outcome
    };
    flight.lease.custody.pending_target =
        (outcome == ReleaseOutcome::Pending).then(|| flight.device.clone());
    outcome
}

fn poll_release_outcome_with(
    flight: &mut ReleaseFlight<'_>,
    read: &mut impl FnMut(&str) -> Option<String>,
) -> ReleaseOutcome {
    if !flight.started {
        return ReleaseOutcome::Refused;
    }
    let outcome = if reconcile_custody(&mut flight.lease.custody, read) {
        target_release_outcome(read("/proc/swaps"), &flight.device)
    } else {
        ReleaseOutcome::Pending
    };
    record_release_outcome(flight, outcome)
}

/// Poll only this owned flight. No helper is restarted or waited on here.
pub fn poll_release_outcome(flight: &mut ReleaseFlight<'_>) -> ReleaseOutcome {
    poll_release_outcome_with(flight, &mut read_proc)
}

fn resume_owned_cleanup_with(
    flight: &mut ReleaseFlight<'_>,
    timeout: Duration,
    read: &mut impl FnMut(&str) -> Option<String>,
    cleanup: impl FnOnce(&SwapAbsorber, Instant, &mut ReleaseCustody) -> CleanupOutcome,
) -> ReleaseOutcome {
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        return ReleaseOutcome::Pending;
    };
    if Instant::now() >= deadline {
        return ReleaseOutcome::Pending;
    }
    let outcome = poll_release_outcome_with(flight, read);
    if outcome != ReleaseOutcome::Pending
        || !flight.lease.custody.cleanup_waits_for_memory
        || flight.lease.custody.child.is_some()
    {
        return outcome;
    }
    if target_release_outcome(read("/proc/swaps"), &flight.device) != ReleaseOutcome::Released
        || Instant::now() >= deadline
    {
        return ReleaseOutcome::Pending;
    }
    let Some(backing) = flight.lease.custody.absorber.take() else {
        return ReleaseOutcome::Pending;
    };
    flight.lease.custody.cleanup_waits_for_memory = false;
    let cleanup = cleanup(&backing, deadline, &mut flight.lease.custody);
    if cleanup == CleanupOutcome::Released {
        return poll_release_outcome_with(flight, read);
    }
    flight.lease.custody.cleanup_waits_for_memory = matches!(
        cleanup,
        CleanupOutcome::HeadroomBlocked | CleanupOutcome::NotStarted
    );
    flight.lease.custody.absorber = Some(backing);
    record_release_outcome(flight, ReleaseOutcome::Pending)
}

/// Resume only cleanup that previously could not start for lack of headroom.
pub fn resume_owned_absorber_cleanup(
    flight: &mut ReleaseFlight<'_>,
    cleanup_timeout: Duration,
) -> ReleaseOutcome {
    resume_owned_cleanup_with(
        flight,
        cleanup_timeout,
        &mut read_proc,
        |backing, deadline, custody| {
            remove_emergency_swap(
                backing,
                deadline,
                &mut SystemRelease {
                    custody,
                    mode: ReleaseMode::Routine,
                },
            )
        },
    )
}

fn execute_release_flight(
    flight: &mut ReleaseFlight<'_>,
    timeout: Duration,
    execute: impl FnOnce(&str, Instant, &mut ReleaseCustody) -> ReleaseOutcome,
    read: &mut impl FnMut(&str) -> Option<String>,
) -> ReleaseOutcome {
    if flight.started {
        return poll_release_outcome_with(flight, read);
    }
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        return record_release_outcome(flight, ReleaseOutcome::Refused);
    };
    // Set before effects so unwinding cannot treat the flight as unstarted.
    flight.started = true;
    flight.lease.custody.pending_target = Some(flight.device.clone());
    let outcome = execute(&flight.device, deadline, &mut flight.lease.custody);
    record_release_outcome(flight, outcome)
}

pub fn release_managed_tier_outcome(
    flight: &mut ReleaseFlight<'_>,
    timeout: Duration,
) -> ReleaseOutcome {
    release_managed_tier_in_mode(flight, timeout, ReleaseMode::Routine)
}

/// The NBD peer is already gone. This is a best-effort attempt, not a
/// host-memory admission or a guarantee that swapped pages can be recovered.
pub fn release_managed_tier_after_peer_closed(
    flight: &mut ReleaseFlight<'_>,
    timeout: Duration,
) -> ReleaseOutcome {
    release_managed_tier_in_mode(flight, timeout, ReleaseMode::LastResort)
}

fn release_managed_tier_in_mode(
    flight: &mut ReleaseFlight<'_>,
    timeout: Duration,
    mode: ReleaseMode,
) -> ReleaseOutcome {
    execute_release_flight(
        flight,
        timeout,
        |device, deadline, custody| {
            release_managed_tier_with(device, deadline, &mut SystemRelease { custody, mode })
        },
        &mut read_proc,
    )
}

/// Fatal-watchdog last resort. The peer is already dying; host memory is not
/// proven and an attempted drain may still fail. Pending is never success.
pub fn release_managed_tier_on_fatal(device: &str, timeout: Duration) -> bool {
    if !reconcile_release_custody() {
        return false;
    }
    let Some(mut flight) = try_begin_release(device) else {
        return false;
    };
    release_managed_tier_in_mode(&mut flight, timeout, ReleaseMode::LastResort)
        == ReleaseOutcome::Released
}

#[cfg(test)]
mod release_tests;

#[cfg(test)]
mod cleanup_retry_tests;

#[cfg(test)]
mod absorber_tests;
