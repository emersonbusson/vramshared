// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/ledger.rs
//! Durable reservation ledger admission, release, binding, and dead-owner recovery.

use super::{
    ADMISSION_STATE_SCHEMA_VERSION, AdmissionState, LedgerLock, LedgerPaths,
    MAX_CONTROL_EVIDENCE_AGE_MS, MAX_LEDGER_BYTES, OwnerIdentity, OwnerProbe,
    RESERVATION_LEDGER_SCHEMA_VERSION, Reservation, ReservationLedger, ScopeStatusAdapter,
    SupervisorStatus, SystemOwnerProbe, WorkloadClass, acquire_lock, identity_is_current,
    named_identity, path_is_present, quarantine_snapshot, read_current_supervisor_status_at,
    read_host_resume_lease_if_present, unique_nonce, unix_time_ms, valid_scope_unit,
    valid_systemd_invocation_id, validate_regular,
};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, fsync, openat, renameat};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

pub(super) fn admission_state_allows_at(
    path: &Path,
    current: &OwnerIdentity,
    supervisor: &SupervisorStatus,
    now_ms: u64,
    probe: &dyn OwnerProbe,
) -> Result<(), String> {
    let text = fs::read_to_string(path)
        .map_err(|_| "admission state evidence is unavailable".to_string())?;
    let state: AdmissionState = serde_json::from_str(&text)
        .map_err(|_| "admission state evidence is malformed".to_string())?;
    if state.schema_version != ADMISSION_STATE_SCHEMA_VERSION
        || state.written_at_unix_ms > now_ms
        || now_ms.saturating_sub(state.written_at_unix_ms) > MAX_CONTROL_EVIDENCE_AGE_MS
        || state.supervisor_identity != supervisor.supervisor_identity
        || !identity_is_current(&state.supervisor_identity, &current.boot_id, probe)
    {
        return Err("admission state evidence is stale, foreign, or invalid".into());
    }
    if !state.admission_open || state.control_state != "HEALTHY" {
        return Err(format!(
            "admission state closes new workloads for control state {}",
            state.control_state
        ));
    }
    Ok(())
}

pub(crate) fn publish_admission_state_at(
    root: &Path,
    supervisor_identity: &OwnerIdentity,
    control_state: &str,
    written_at_unix_ms: u64,
) -> Result<(), String> {
    let paths = LedgerPaths::new(root);
    let lock = acquire_lock(&paths, supervisor_identity, &SystemOwnerProbe)?;
    let state = AdmissionState {
        schema_version: ADMISSION_STATE_SCHEMA_VERSION,
        admission_open: control_state == "HEALTHY",
        control_state: control_state.to_string(),
        supervisor_identity: supervisor_identity.clone(),
        written_at_unix_ms,
    };
    let contents = format!(
        "{}\n",
        serde_json::to_string(&state).map_err(|error| error.to_string())?
    );
    store_named_locked(&lock, "admission-state.json", contents.as_bytes())
}

/// The one transient error signature this surface retries.
///
/// `flock(LOCK_EX | LOCK_NB)` returns `EWOULDBLOCK` when another ledger actor
/// — typically a reservation add/release — holds the transition lock for a
/// bounded critical section. That contention resolves on its own, so killing
/// a long-lived supervisor over it is a defect. Every other ledger error is
/// deterministic (identity, path, I/O) and must fail fast instead.
///
/// The signature is a stable substring of the `acquire_lock` busy message so
/// callers can classify without the string having to match verbatim. A
/// self-held conflicting lock also reports this signature and is likewise
/// retried; if the conflict is genuine self-deadlock the retry budget is
/// exhausted and the original error is returned unchanged.
pub(crate) const LEDGER_BUSY_SIGNATURE: &str = "reservation ledger transition is busy";

pub(crate) fn is_ledger_busy(error: &str) -> bool {
    error.contains(LEDGER_BUSY_SIGNATURE)
}

/// `publish_admission_state_at`, retried only on the transient busy signature.
///
/// Each attempt republishes the same immutable state bytes for a given
/// `written_at_unix_ms`, so a replay after a released lock is idempotent
///. The retry budget is bounded and deliberately lives at this
/// action boundary rather than inside `acquire_lock`, which must keep
/// reporting `EWOULDBLOCK` immediately so callers that require strict ledger
/// exclusivity can still observe interleaving.
pub(crate) fn publish_admission_state_at_with_transient_retry(
    root: &Path,
    supervisor_identity: &OwnerIdentity,
    control_state: &str,
    written_at_unix_ms: u64,
) -> Result<(), String> {
    retry_transient_ledger_publish(|| {
        publish_admission_state_at(root, supervisor_identity, control_state, written_at_unix_ms)
    })
}

/// Retry loop over a ledger publication attempt.
///
/// Split out from `publish_admission_state_at_with_transient_retry` so the
/// retry policy itself is observable: tests count invocations to prove that a
/// deterministic error is attempted exactly once and only the busy signature
/// consumes the budget.
pub(super) fn retry_transient_ledger_publish(
    mut publish: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    const MAX_ATTEMPTS: u32 = 8;
    const RETRY_BACKOFF: Duration = Duration::from_millis(25);
    let mut last_error = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        match publish() {
            Ok(()) => return Ok(()),
            Err(error) => {
                let transient = is_ledger_busy(&error);
                last_error = error;
                if !transient || attempt + 1 == MAX_ATTEMPTS {
                    return Err(last_error);
                }
                std::thread::sleep(RETRY_BACKOFF);
            }
        }
    }
    Err(last_error)
}

pub(super) fn validate_ledger(ledger: &ReservationLedger) -> Result<(), String> {
    if ledger.schema_version != RESERVATION_LEDGER_SCHEMA_VERSION {
        return Err(format!(
            "unsupported reservation ledger schema: {}",
            ledger.schema_version
        ));
    }
    if ledger.next_ordinal == 0
        || ledger.reservations.iter().any(|reservation| {
            reservation.issued_ordinal == 0 || reservation.issued_ordinal >= ledger.next_ordinal
        })
    {
        return Err("reservation ledger ordinal sequence is invalid".into());
    }
    let mut ordinals = ledger
        .reservations
        .iter()
        .map(|reservation| reservation.issued_ordinal)
        .collect::<Vec<_>>();
    ordinals.sort_unstable();
    if ordinals.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("reservation ledger contains duplicate issued ordinals".into());
    }
    Ok(())
}

/// Publish the first ledger document under an already-held authority lock.
///
/// DT-2: the authority directory self-initializes its components — the
/// transition owner and the quarantine are already created on first touch —
/// and the reservation table is one of them. Before this, nothing in
/// production ever created `reservations.json`, so the first admission on a
/// fresh boot failed `reservation ledger is unavailable` and a CRITICAL
/// `freeze_discardable` reported the same string when the honest answer was
/// "this boot has never issued a reservation". The exclusive flock is already
/// held, so a concurrent publisher cannot interleave and this is idempotent
///. A malformed, wrong-schema or identity-changed document is
/// still refused by `load_ledger_locked`; only the absent case initializes.
pub(super) fn initialize_ledger_locked(lock: &LedgerLock) -> Result<ReservationLedger, String> {
    let ledger = ReservationLedger::empty();
    store_ledger(lock, &ledger)?;
    Ok(ledger)
}

pub(super) fn load_ledger_locked(lock: &LedgerLock) -> Result<ReservationLedger, String> {
    lock.revalidate()?;
    let opened = match openat(
        &lock.root_file,
        OsStr::new("reservations.json"),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(opened) => opened,
        Err(rustix::io::Errno::NOENT) => return initialize_ledger_locked(lock),
        Err(error) => return Err(format!("read reservation ledger: {error}")),
    };
    let mut file = File::from(opened);
    let identity = validate_regular(&file, "reservation ledger")?;
    if named_identity(
        &lock.root_file,
        OsStr::new("reservations.json"),
        FileType::RegularFile,
        "reservation ledger",
    )? != identity
    {
        return Err("reservation ledger identity changed before read".into());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_LEDGER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read reservation ledger: {error}"))?;
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err("reservation ledger exceeds its bounded size".into());
    }
    if validate_regular(&file, "reservation ledger")? != identity
        || named_identity(
            &lock.root_file,
            OsStr::new("reservations.json"),
            FileType::RegularFile,
            "reservation ledger",
        )? != identity
    {
        return Err("reservation ledger identity changed during read".into());
    }
    lock.revalidate()?;
    let ledger: ReservationLedger = serde_json::from_slice(&bytes)
        .map_err(|_| "reservation ledger is malformed".to_string())?;
    validate_ledger(&ledger)?;
    Ok(ledger)
}

pub(crate) fn read_reservation_ledger(path: &Path) -> Result<Vec<Reservation>, String> {
    if path.file_name() != Some(OsStr::new("reservations.json")) {
        return Err("reservation ledger path is not canonical".into());
    }
    let root = path
        .parent()
        .ok_or("reservation ledger parent is missing")?;
    let paths = LedgerPaths::new(root);
    let owner = OwnerIdentity::current()?;
    let lock = acquire_lock(&paths, &owner, &SystemOwnerProbe)?;
    Ok(load_ledger_locked(&lock)?.reservations)
}

pub(super) fn store_named_locked(
    lock: &LedgerLock,
    name: &str,
    contents: &[u8],
) -> Result<(), String> {
    lock.revalidate()?;
    let temporary = format!(".{name}-{}.tmp", unique_nonce());
    let opened = openat(
        &lock.root_file,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| format!("create durable {name} temporary: {error}"))?;
    let mut file = File::from(opened);
    validate_regular(&file, "durable temporary file")?;
    let result = (|| {
        file.write_all(contents)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        lock.revalidate()?;
        renameat(&lock.root_file, temporary.as_str(), &lock.root_file, name)
            .map_err(|error| error.to_string())?;
        fsync(&lock.root_file).map_err(|error| error.to_string())?;
        let named = named_identity(
            &lock.root_file,
            OsStr::new(name),
            FileType::RegularFile,
            "durable publication",
        )?;
        if validate_regular(&file, "durable publication")? != named {
            return Err("durable publication identity changed after rename".into());
        }
        lock.revalidate()
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&lock.root_file, temporary.as_str(), AtFlags::empty());
    }
    result
}

pub(super) fn store_ledger(lock: &LedgerLock, ledger: &ReservationLedger) -> Result<(), String> {
    validate_ledger(ledger)?;
    let mut bytes = serde_json::to_vec(ledger).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err("reservation ledger exceeds its bounded size".into());
    }
    store_named_locked(lock, "reservations.json", &bytes)
}

#[cfg(test)]
pub(super) fn load_ledger(path: &Path) -> Result<ReservationLedger, String> {
    if path.file_name() != Some(OsStr::new("reservations.json")) {
        return Err("reservation ledger path is not canonical".into());
    }
    let root = path
        .parent()
        .ok_or("reservation ledger parent is missing")?;
    let paths = LedgerPaths::new(root);
    let owner = OwnerIdentity::current()?;
    let lock = acquire_lock(&paths, &owner, &SystemOwnerProbe)?;
    load_ledger_locked(&lock)
}

/// The unit half of the verified-dead recovery proof.
///
/// A row may only be recovered when its exact unit holds no processes and
/// will not start another. The name is checked against the one canonical
/// scope predicate first: a name the supervisor would refuse as a victim
/// must not be treated as a known-vacant unit here either.
pub(super) fn reservation_unit_is_vacant(
    status: &dyn ScopeStatusAdapter,
    unit: &str,
) -> Result<bool, String> {
    if !valid_scope_unit(unit) {
        return Err("reservation unit is not a canonical scope name".into());
    }
    status.query(unit)?.is_process_vacant()
}

/// Recover rows that cannot represent any memory any more.
///
/// A row is removed only when **both** halves of the proof hold: its owner is
/// not alive, and its exact unit is vacant. Either half alone is not enough.
/// Owner death alone would free budget for a still-running scope; a vacant
/// unit alone may belong to an owner that is between release and store. An
/// unknown owner state keeps the row, and an unknown unit state keeps the row
/// — an unknown observation is never read as proof of absence.
///
/// Recovered rows are preserved in the quarantine before the table is
/// rewritten, so a recovery cannot destroy evidence of what was admitted.
/// Recovery is persisted even when the caller's admission is later refused:
/// correcting the table is not a favour the budget does for free.
pub(super) fn recover_verified_dead_reservations(
    lock: &mut LedgerLock,
    ledger: &mut ReservationLedger,
    probe: &dyn OwnerProbe,
    status: &dyn ScopeStatusAdapter,
) -> Result<(), String> {
    let mut retained = Vec::with_capacity(ledger.reservations.len());
    let mut recovered = false;
    for item in std::mem::take(&mut ledger.reservations) {
        if probe.is_alive(&item.owner) {
            retained.push(item);
            continue;
        }
        match reservation_unit_is_vacant(status, &item.unit) {
            Ok(true) => {
                let evidence = serde_json::to_vec(&item)
                    .map_err(|error| format!("encode recovered reservation: {error}"))?;
                quarantine_snapshot(lock, "reservation", &evidence, "reservation")?;
                recovered = true;
            }
            Ok(false) => retained.push(item),
            Err(_) => retained.push(item),
        }
    }
    ledger.reservations = retained;
    if recovered {
        store_ledger(lock, ledger)?;
    }
    Ok(())
}

/// The one place that decides what a new reservation costs and what it is
/// called. Every row the table still holds is charged — owner death is not
/// release — and `next_ordinal` is monotonic for the life of the document.
pub(super) fn push_admitted_reservation(
    lock: &LedgerLock,
    ledger: &mut ReservationLedger,
    owner: OwnerIdentity,
    class: WorkloadClass,
    memory_bytes: u64,
    aggregate_max: u64,
) -> Result<Reservation, String> {
    let reserved = ledger
        .reservations
        .iter()
        .try_fold(0_u64, |sum, item| sum.checked_add(item.memory_bytes))
        .ok_or("reservation sum overflow")?;
    if memory_bytes == 0 || reserved.saturating_add(memory_bytes) > aggregate_max {
        return Err(format!(
            "aggregate workload budget exceeded: reserved={} requested={} max={aggregate_max}",
            reserved, memory_bytes
        ));
    }
    let id = unique_nonce();
    let issued_ordinal = ledger.next_ordinal;
    ledger.next_ordinal = ledger
        .next_ordinal
        .checked_add(1)
        .ok_or("reservation ordinal sequence exhausted")?;
    let reservation = Reservation {
        id: id.clone(),
        owner,
        class,
        memory_bytes,
        unit: format!("ramshared-{}-{}.scope", class.as_str(), id),
        invocation_id: None,
        issued_ordinal,
    };
    ledger.reservations.push(reservation.clone());
    store_ledger(lock, ledger)?;
    Ok(reservation)
}

/// The ledger-only admission surface, for tests that assert table semantics.
///
/// Production admission goes through `admit_reservation_at`, which also binds
/// supervisor identity and the boot-ephemeral host resume lease. Those gates
/// are deliberately absent here so a table test does not have to manufacture a
/// host authorization it is not about. Nothing in production may call this.
///
/// The parameter list mirrors `admit_reservation_at` so the two seams can be
/// read side by side and the missing gates stay visible as gaps rather than as
/// a shorter signature that hides them. `resume_lease` is one of those gaps.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn acquire_reservation_at(
    root: &Path,
    safe_mode: &Path,
    resume_lease: &Path,
    owner: OwnerIdentity,
    class: WorkloadClass,
    memory_bytes: u64,
    aggregate_max: u64,
    probe: &dyn OwnerProbe,
    status: &dyn ScopeStatusAdapter,
) -> Result<Reservation, String> {
    // This seam does not gate on the host resume lease; `admit_reservation_at`
    // does. The parameter exists to keep the two signatures comparable.
    let _ = resume_lease;
    if path_is_present(safe_mode)? {
        return Err("safe mode closes new workload admission until recover --resume".into());
    }
    let paths = LedgerPaths::new(root);
    let mut lock = acquire_lock(&paths, &owner, probe)?;
    let mut ledger = load_ledger_locked(&lock)?;
    recover_verified_dead_reservations(&mut lock, &mut ledger, probe, status)?;
    push_admitted_reservation(
        &lock,
        &mut ledger,
        owner,
        class,
        memory_bytes,
        aggregate_max,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn admit_reservation_at(
    root: &Path,
    safe_mode: &Path,
    resume_lease: &Path,
    supervisor_state: &Path,
    owner: OwnerIdentity,
    class: WorkloadClass,
    memory_bytes: u64,
    aggregate_max: u64,
    probe: &dyn OwnerProbe,
    status: &dyn ScopeStatusAdapter,
) -> Result<(Reservation, LedgerLock), String> {
    let paths = LedgerPaths::new(root);
    let mut lock = acquire_lock(&paths, &owner, probe)?;
    if path_is_present(safe_mode)? {
        return Err("safe mode closes new workload admission until recover --resume".into());
    }
    let now_ms = unix_time_ms()?;
    let supervisor = read_current_supervisor_status_at(supervisor_state, &owner, now_ms, 0, probe)?;
    admission_state_allows_at(&paths.admission_state, &owner, &supervisor, now_ms, probe)?;
    // DT-11: heavy guest units require the boot-ephemeral host authorization.
    // Supervisor identity is already bound above; this gate answers only
    // "did the host authorize heavy units on this boot?".
    read_host_resume_lease_if_present(resume_lease, &owner.boot_id)?
        .ok_or("boot-ephemeral resume lease is required for workload admission")?;
    let mut ledger = load_ledger_locked(&lock)?;
    recover_verified_dead_reservations(&mut lock, &mut ledger, probe, status)?;
    let reservation = push_admitted_reservation(
        &lock,
        &mut ledger,
        owner,
        class,
        memory_bytes,
        aggregate_max,
    )?;
    Ok((reservation, lock))
}

pub(super) fn release_reservation_at(
    root: &Path,
    reservation: &Reservation,
    probe: &dyn OwnerProbe,
) -> Result<bool, String> {
    let paths = LedgerPaths::new(root);
    let lock = acquire_lock(&paths, &reservation.owner, probe)?;
    let mut ledger = load_ledger_locked(&lock)?;
    let Some(index) = ledger
        .reservations
        .iter()
        .position(|item| item.id == reservation.id)
    else {
        return Ok(false);
    };
    if ledger.reservations[index].owner != reservation.owner {
        return Err("reservation owner identity mismatch".into());
    }
    ledger.reservations.remove(index);
    store_ledger(&lock, &ledger)?;
    Ok(true)
}

pub(super) fn bind_reservation_invocation_while_locked(
    lock: &LedgerLock,
    reservation: &Reservation,
    invocation_id: &str,
) -> Result<Reservation, String> {
    if !valid_systemd_invocation_id(invocation_id) {
        return Err("managed scope InvocationID is not canonical".into());
    }
    let mut ledger = load_ledger_locked(lock)?;
    let entry = ledger
        .reservations
        .iter_mut()
        .find(|item| item.id == reservation.id)
        .ok_or("managed scope reservation disappeared before identity binding")?;
    if entry.owner != reservation.owner || entry.unit != reservation.unit {
        return Err("managed scope reservation identity changed before binding".into());
    }
    match entry.invocation_id.as_deref() {
        Some(existing) if existing != invocation_id => {
            return Err("managed scope reservation already has another InvocationID".into());
        }
        Some(_) => {}
        None => entry.invocation_id = Some(invocation_id.to_string()),
    }
    let bound = entry.clone();
    store_ledger(lock, &ledger)?;
    Ok(bound)
}
