// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/reserve.rs
//! VRAM reserve-floor derivation and measurement gap predicates.

use super::*;

/// The published MiB face of a reserve **floor** — a quantity whose whole job
/// is to say how much must stay free.
///
/// `div_ceil`, never floor. An **allocation bound** ("you may take at most
/// this") floors, because rounding down never grants more than the exact
/// figure. A reserve floor has to round the other way: understating it is what
/// authorizes more allocation than the policy allows. These are the two faces
/// of the same two-view rule and they point in opposite directions, which is
/// why one "round to MiB" helper must not serve both callers.
///
/// A whole-MiB input is unchanged — the conversion is the identity there — so
/// the sealed minimum and the runtime headroom publish as their exact `2048`
/// and `640`. The enforced floor is the one that is generally fractional:
/// `configured_reserve_bytes` floors `capacity * percent / 100`, and
/// `enforced_free_floor_from_configured` takes `capacity.div_ceil(5)`. Neither
/// is whole MiB once a capacity-derived term beats the sealed minimum, which
/// starts around a 10 GiB capacity. On a 24 GiB adapter the enforced floor is
/// `5 825 049 396` bytes — 5555 MiB plus 209 716 bytes — and a floor division
/// publishes `5555`, below the amount that must stay free.
///
/// A **view**. The policy computes in bytes, the unit the authority states and
/// the wire carries; this only renders one.
pub(super) fn reserve_floor_mib(bytes: u64) -> u64 {
    bytes.div_ceil(1024 * 1024)
}

/// Builds the DT-10 reserve block from one budget observation.
///
/// The monitor is a reader: it reports the sealed authority and the formula it
/// can recompute. It never claims `lab-override-raise`, because an override
/// lives in the daemon process and is not observable from here.
///
/// The three figures leave in MiB because that is what the field names carry,
/// but the policy is run in bytes end to end. The runtime headroom is
/// `ramshared_vram::RUNTIME_FREE_BUFFER_BYTES` on both sides of that
/// conversion — the literal lives with the reserve authority, so a local `640`
/// would be an unvalidated mirror of the value the worker enforces — and is
/// never rounded to MiB and back before the policy sees it.
pub(super) fn vram_reserve_fields(
    total_bytes: Option<u64>,
    budget_bytes: u64,
) -> (
    u64,    // enforced MiB
    String, // source
    u64,    // sealed min MiB
    u64,    // sealed percent
    u64,    // runtime buffer MiB
) {
    use ramshared_vram::{
        RUNTIME_FREE_BUFFER_BYTES, ReserveFloorPolicy, enforced_free_floor_from_configured,
    };
    let policy = ReserveFloorPolicy::sealed();
    let capacity = ReserveFloorPolicy::helper_capacity(total_bytes, budget_bytes);
    let enforced = enforced_free_floor_from_configured(
        policy.configured_reserve_bytes(capacity),
        capacity,
        RUNTIME_FREE_BUFFER_BYTES,
    );
    (
        reserve_floor_mib(enforced),
        "sealed-manifest".to_string(),
        reserve_floor_mib(policy.min_floor_bytes),
        policy.sealed_percent,
        reserve_floor_mib(RUNTIME_FREE_BUFFER_BYTES),
    )
}

/// Whether this guest carries the WSL interop surface.
///
/// The binfmt entry is the strong signal. The environment variable is the
/// fallback for a guest where `binfmt_misc` is not mounted — the `||` must be
/// evaluated when the path is absent. Both inputs are parameters so the probe
/// is pure and both arms are reachable in a unit test without mutating the
/// process environment.
pub(super) fn wsl_interop_present(
    binfmt_wsl_interop: &Path,
    wsl_interop_env: Option<&OsStr>,
) -> bool {
    binfmt_wsl_interop.exists() || wsl_interop_env.is_some()
}

/// Names the measurement gaps one observation carries.
///
/// Each absence is its own fact: missing PSI is not missing RAM telemetry, and
/// missing RAM telemetry is not missing swap telemetry. A reader that
/// collapses them cannot tell an unpressured machine from one whose pressure
/// it never saw, nor a host with no swap from one whose swap counter never
/// arrived. The two memory faces are derived from the pairs themselves, so a
/// gap is named exactly when its figures publish `null` — never when a
/// measured zero sits in the fields.
pub(super) fn measurement_errors(
    memory_psi_available: bool,
    mem: &MemoryObservation,
) -> Vec<String> {
    let mut errors = Vec::new();
    if !memory_psi_available {
        errors.push("memory_psi_unavailable".to_string());
    }
    if mem.ram_pair().is_none() {
        errors.push("memory_telemetry_unavailable".to_string());
    }
    if mem.swap_pair().is_none() {
        errors.push("swap_telemetry_unavailable".to_string());
    }
    errors
}
