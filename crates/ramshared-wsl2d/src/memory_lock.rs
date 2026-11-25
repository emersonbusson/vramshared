// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/memory_lock.rs
//! Anti-deadlock residency locking (`mlockall`, `oom_score_adj`, `PR_SET_IO_FLUSHER`).

use core::ffi::{c_int, c_ulong};

// Discipline 3 (anti-deadlock): the daemon serves swap, so it cannot be swapped out.
unsafe extern "C" {
    fn mlockall(flags: c_int) -> c_int;
    pub(crate) fn prctl(
        option: c_int,
        arg2: c_ulong,
        arg3: c_ulong,
        arg4: c_ulong,
        arg5: c_ulong,
    ) -> c_int;
}
const PR_SET_IO_FLUSHER: c_int = 57;
const MCL_CURRENT: c_int = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemoryLockStatus {
    Protected,
    ForcedDegraded { locked: bool, oom_ok: bool },
}

/// Pure fail-closed decision after the two process-local protection attempts.
/// `--force` is the sole explicit opt-in to a degraded anti-deadlock posture.
pub(crate) fn memory_lock_status(
    locked: bool,
    oom_ok: bool,
    force: bool,
) -> Result<MemoryLockStatus, &'static str> {
    if !locked && !force {
        return Err("mlockall failed; run as root or use --force");
    }
    if !oom_ok && !force {
        return Err("could not set oom_score_adj=-1000; run as root or use --force");
    }
    if locked && oom_ok {
        Ok(MemoryLockStatus::Protected)
    } else {
        Ok(MemoryLockStatus::ForcedDegraded { locked, oom_ok })
    }
}

/// Requests the calling thread's IO-flusher state via `PR_SET_IO_FLUSHER`.
/// Success alone does not prove that the complete daemon or its storage path
/// is free of reclaim deadlocks.
pub(crate) fn enable_anti_deadlock_io_flusher() -> bool {
    // SAFETY: prctl with PR_SET_IO_FLUSHER only affects memory allocation flags of the calling thread.
    unsafe { prctl(PR_SET_IO_FLUSHER, 1, 0, 0, 0) == 0 }
}

/// Re-locks whatever is resident now.
///
/// `mlockall(MCL_CURRENT)` locks only the pages that are resident at the
/// instant it runs. Pages the daemon allocates afterwards are not covered,
/// and any of them can be swapped to the very device this process serves —
/// which is how it can fault on its own memory and die.
///
/// Re-applying it on the daemon's existing heartbeat cadence shrinks that
/// window: each pass locks what has become resident since the last one. It
/// is deliberately `MCL_CURRENT` and not `MCL_FUTURE`, which
/// [`lock_memory`] rejects because inheriting the lock into CUDA, DXG and
/// thread-stack mappings can turn memory pressure into a host stall.
///
/// Returns whether the re-lock succeeded. A failure is reported, not
/// hidden: the caller decides what a daemon that cannot hold its own pages
/// should do.
pub(crate) fn relock_resident() -> bool {
    // SAFETY: mlockall is a syscall with no unsafe memory side effects.
    let result = unsafe { mlockall(MCL_CURRENT) };
    result == 0
}

/// Locks memory (`mlockall`) + protects from OOM killer (`oom_score_adj=-1000`) BEFORE
/// serving swap (Discipline 3, anti-deadlock). `--force` continues without protection, warning.
///
/// `MCL_FUTURE` is deliberately rejected. CUDA, DXG, cache growth, thread
/// stacks, and runtime libraries can all create mappings after this frontier;
/// inheriting the lock into those mappings can turn memory pressure into a
/// process or host stall.
pub(crate) fn lock_memory(
    force: bool,
    lock_future: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if lock_future {
        return Err("MCL_FUTURE is forbidden before runtime GPU/DXG mappings".into());
    }
    // SAFETY: mlockall is a syscall with no unsafe memory side effects.
    let locked = unsafe { mlockall(MCL_CURRENT) } == 0;
    let oom_ok = std::fs::write("/proc/self/oom_score_adj", "-1000").is_ok();

    if enable_anti_deadlock_io_flusher() {
        eprintln!("[ramsharedd] anti-deadlock: PR_SET_IO_FLUSHER active (PF_MEMALLOC_NOIO)");
    }

    match memory_lock_status(locked, oom_ok, force)? {
        MemoryLockStatus::Protected => {
            eprintln!("[ramsharedd] memory locked (mlockall) + oom_score_adj=-1000");
        }
        MemoryLockStatus::ForcedDegraded { locked, oom_ok } => {
            eprintln!(
                "[ramsharedd] WARNING --force: mlockall={} oom_score_adj={} (anti-deadlock NOT guaranteed)",
                if locked { "ok" } else { "FAILED" },
                if oom_ok { "ok" } else { "FAILED" }
            );
        }
    }
    Ok(())
}
