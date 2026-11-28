// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/teardown/emergency_cleanup.rs
//! Retire a temporary swap only with live release evidence.

use super::*;

pub(super) fn retire_emergency_swap<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    binding: &LifecycleBinding,
    swap: &EmergencySwap,
) {
    retire_emergency_swap_with_budget(runner, paths, binding, swap, |targets, entries, host| {
        headroom_for_targets(targets, entries, 0, host).is_ok_and(|decision| decision.is_safe())
    });
}

pub(super) fn retire_emergency_swap_with_budget<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    binding: &LifecycleBinding,
    swap: &EmergencySwap,
    budget: impl FnOnce(&[String], &[SwapEntry], Option<ramshared_tier::HostDrainMemory>) -> bool,
) {
    let release_allowed = read_swaps().is_ok_and(|entries| {
        if swap.free_bytes(&entries).is_err() {
            return false;
        }
        let Ok(host) = authorize_peer_for_cleanup(paths, binding) else {
            return false;
        };
        let Some(path) = swap.path.to_str() else {
            return false;
        };
        let canonical = crate::cascade::canonicalize_swap_path(path);
        budget(&[canonical], &entries, host)
    });
    if release_allowed {
        if let Err(error) = swap.record_active_observation(paths) {
            eprintln!(
                "[down] emergency swap retained at {}: {error}",
                swap.path.display()
            );
            return;
        }
        if swap.remove(runner)
            && let Err(error) = read_swaps().and_then(|entries| retained(paths, &entries, true))
        {
            eprintln!("[down] emergency swap record retained after release: {error}");
        }
    } else {
        eprintln!(
            "[down] emergency swap retained at {}: occupied capacity or release headroom is unverified",
            swap.path.display()
        );
    }
}

pub(super) fn authorize_peer_for_cleanup(
    paths: &RuntimePaths,
    binding: &LifecycleBinding,
) -> Result<Option<ramshared_tier::HostDrainMemory>, CascadeError> {
    crate::cascade::cascade_io::authorization::revalidate_binding_identity(binding)?;
    crate::cascade::cascade_io::authorization::revalidate_peer_liveness(binding, paths)?;
    Ok(live_host_memory(paths, binding, PeerState::Live))
}
