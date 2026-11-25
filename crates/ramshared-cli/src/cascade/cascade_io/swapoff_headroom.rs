// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/swapoff_headroom.rs
//! Pure managed-swapoff headroom policy.
//!
//! The total swap in `/proc/swaps` includes fallback devices that `down` does
//! not touch. The decision therefore consumes the teardown plan: only devices
//! with a planned `Swapoff` contribute `managed`. Fallback free space is
//! reported for the operator but is deliberately not credited as an absorber:
//! `swapoff` first needs resident memory for the pages it decompresses, so a
//! free fallback must never turn an otherwise unsafe decision into SAFE.

use std::collections::HashSet;
use std::fs;
use std::io::Read;

use serde::Serialize;

pub(super) const MIN_RESERVE_BYTES: u64 = ramshared_tier::MIN_DRAIN_RESERVE_BYTES;
pub(super) const MARGIN_BYTES: u64 = ramshared_tier::DRAIN_MARGIN_BYTES;
const MEMINFO_MAX_BYTES: usize = ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES;

#[cfg(test)]
thread_local! {
    static TEST_PLATFORM: std::cell::Cell<Option<ramshared_block::platform::PlatformKind>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) struct NativePlatformFixture(Option<ramshared_block::platform::PlatformKind>);

#[cfg(test)]
impl NativePlatformFixture {
    pub(super) fn new() -> Self {
        Self(
            TEST_PLATFORM.with(|slot| {
                slot.replace(Some(ramshared_block::platform::PlatformKind::NativeLinux))
            }),
        )
    }
}

#[cfg(test)]
impl Drop for NativePlatformFixture {
    fn drop(&mut self) {
        TEST_PLATFORM.with(|slot| slot.set(self.0));
    }
}

pub(super) fn observed_platform() -> Option<ramshared_block::platform::PlatformKind> {
    #[cfg(test)]
    if let Some(platform) = TEST_PLATFORM.with(std::cell::Cell::get) {
        return Some(platform);
    }
    ramshared_block::platform::probe_platform().platform
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MemInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SwapAmount {
    pub path: String,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum HeadroomVerdict {
    Safe,
    Blocked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct HeadroomDecision {
    pub managed_bytes: u64,
    pub available_bytes: u64,
    pub emergency_bytes: u64,
    pub remaining_free_bytes: u64,
    pub reserve_bytes: u64,
    pub margin_bytes: u64,
    pub required_bytes: u64,
    pub deficit_bytes: u64,
    pub verdict: HeadroomVerdict,
    pub reason: &'static str,
}

impl HeadroomDecision {
    pub(super) fn is_safe(&self) -> bool {
        self.verdict == HeadroomVerdict::Safe
    }

    pub(super) fn absorber_can_help(&self) -> bool {
        self.reason == "managed_swap_exceeds_headroom"
    }

    pub(super) fn refusal_message(&self) -> String {
        format!(
            "swapoff headroom is not authorized: managed={} B available={} B emergency={} B remaining_free={} B (reported, not credited) reserve={} B margin={} B required={} B deficit={} B reason={}",
            self.managed_bytes,
            self.available_bytes,
            self.emergency_bytes,
            self.remaining_free_bytes,
            self.reserve_bytes,
            self.margin_bytes,
            self.required_bytes,
            self.deficit_bytes,
            self.reason
        )
    }
}

/// Parse only the two counters this decision owns. A missing, duplicate,
/// malformed or inconsistent pair is absence, never a fabricated zero.
pub(super) fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let memory = ramshared_tier::DrainMemory::parse(text)?;
    Some(MemInfo {
        total_bytes: memory.total_bytes,
        available_bytes: memory.available_bytes,
    })
}

pub(super) fn parse_meminfo_with_free(text: &str) -> Option<(MemInfo, Option<u64>)> {
    let mem = parse_meminfo(text)?;
    let free_bytes = ramshared_tier::parse_free_bytes(text);
    if free_bytes.is_some_and(|free| free > mem.total_bytes) {
        return None;
    }
    Some((mem, free_bytes))
}

pub(super) fn read_meminfo_with_free() -> Result<(MemInfo, Option<u64>), &'static str> {
    let mut text = String::new();
    fs::File::open("/proc/meminfo")
        .and_then(|file| {
            file.take((MEMINFO_MAX_BYTES + 1) as u64)
                .read_to_string(&mut text)
        })
        .map_err(|_| "meminfo_unavailable")?;
    parse_meminfo_with_free(&text).ok_or("meminfo_malformed")
}

pub(super) fn read_meminfo() -> Result<MemInfo, &'static str> {
    read_meminfo_with_free().map(|(mem, _)| mem)
}

/// Partition active swap into the part the plan will remove and the part that
/// remains available as an absorber. Paths are the canonical `/dev/...` names
/// already used by the teardown plan.
pub(super) fn summarize_swaps(planned: &[String], swaps: &[SwapAmount]) -> (u64, u64) {
    let planned: HashSet<&str> = planned.iter().map(String::as_str).collect();
    let mut managed = 0u64;
    let mut remaining_free = 0u64;
    for swap in swaps {
        if planned.contains(swap.path.as_str()) {
            managed = managed.saturating_add(swap.used_bytes);
        } else {
            // Reported only. It is not available as a memory absorber before
            // swapoff; see `evaluate`.
            remaining_free = remaining_free.saturating_add(swap.free_bytes);
        }
    }
    (managed, remaining_free)
}

pub(super) fn evaluate(
    managed_bytes: u64,
    remaining_free_bytes: u64,
    mem: MemInfo,
) -> HeadroomDecision {
    evaluate_with_absorber(managed_bytes, remaining_free_bytes, 0, mem)
}

/// The same policy with a transient emergency-swap absorber (SPEC
/// `swap-drain-safety` RF-2/RF-5). Only that absorber is credited; fallback
/// free space is still reported and never counted as RAM.
pub(super) fn evaluate_with_absorber(
    managed_bytes: u64,
    remaining_free_bytes: u64,
    emergency_bytes: u64,
    mem: MemInfo,
) -> HeadroomDecision {
    let decision = ramshared_tier::DrainBudget {
        total_bytes: mem.total_bytes,
        available_bytes: mem.available_bytes,
        managed_bytes,
        emergency_bytes,
    }
    .evaluate();
    HeadroomDecision {
        managed_bytes,
        available_bytes: mem.available_bytes,
        emergency_bytes,
        remaining_free_bytes,
        reserve_bytes: decision
            .map(|d| d.reserve_bytes)
            .unwrap_or(MIN_RESERVE_BYTES),
        margin_bytes: MARGIN_BYTES,
        required_bytes: decision.map(|d| d.required_bytes).unwrap_or(u64::MAX),
        deficit_bytes: decision.map(|d| d.deficit_bytes).unwrap_or(u64::MAX),
        verdict: if decision.is_some_and(|d| d.is_safe()) {
            HeadroomVerdict::Safe
        } else {
            HeadroomVerdict::Blocked
        },
        reason: decision
            .map(|d| d.reason)
            .unwrap_or("drain_observation_invalid"),
    }
}

pub(super) fn read_leased_cache_status(
    status_path: &std::path::Path,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> Option<serde_json::Value> {
    let status = crate::cascade::read_status_document(status_path)?;
    if !crate::cascade::cache_status_is_current_daemon(&status, daemon_instance_id, now_unix_ms) {
        return None;
    }
    if status
        .get("control_plane_state")
        .and_then(serde_json::Value::as_str)
        != Some("vsock_leased")
    {
        return None;
    }
    let deadline = status
        .get("control_plane_lease_deadline_ms")
        .and_then(serde_json::Value::as_u64)?;
    (deadline > now_unix_ms).then_some(status)
}

/// Read fresh authenticated host-memory telemetry for the live bound daemon.
pub(super) fn verified_live_host_memory(
    status_path: &std::path::Path,
    daemon_instance_id: &str,
    now_unix_ms: Option<u64>,
) -> Option<ramshared_tier::HostDrainMemory> {
    let host = super::host_memory_admission::sample_from_lease(
        status_path,
        daemon_instance_id,
        now_unix_ms,
    )?;
    Some(ramshared_tier::HostDrainMemory {
        total_bytes: host.physical.total_bytes,
        available_bytes: host.physical.available_bytes,
        commit_total_bytes: host.commit_total_bytes,
        commit_limit_bytes: host.commit_limit_bytes,
    })
}

#[cfg(test)]
use super::host_memory_admission::sample_from_lease as verified_live_host_memory_sample;

/// Read fresh authenticated Windows host NTFS free bytes matching `root_vhd`
/// (with the 1 GiB reserve subtracted) for WSL2 emergency swap allocation.
pub(super) fn verified_live_host_storage_cap(
    status_path: &std::path::Path,
    daemon_instance_id: &str,
    now_unix_ms: Option<u64>,
    root_vhd: &ramshared_block::vhd_identity::RootVhdObservation,
) -> Option<u64> {
    let now_unix_ms = now_unix_ms?;
    if root_vhd.identifier == [0; 16]
        || !std::time::Instant::now()
            .checked_duration_since(root_vhd.observed_at)
            .is_some_and(|age| age < std::time::Duration::from_secs(5))
    {
        return None;
    }
    let status = read_leased_cache_status(status_path, daemon_instance_id, now_unix_ms)?;
    let host: ramshared_ipc::storage_proof::HostStorageTelemetry =
        serde_json::from_value(status.get("host_storage")?.clone()).ok()?;
    let (boot_id, boot_ns) = ramshared_ipc::storage_proof::storage_boot_clock()?;
    if !host.deadline.is_current(&boot_id, boot_ns) {
        return None;
    }
    let disk_id = ramshared_ipc::storage_proof::parse_disk_id_hex(&host.disk_id).ok()?;
    let written_at = status.get("written_at_unix_ms")?.as_u64()?;
    let elapsed = now_unix_ms.checked_sub(written_at)?;
    // Round conservatively for timestamps truncated to milliseconds.
    let observed_age = host.age_ms.checked_add(elapsed)?.checked_add(1)?;
    if disk_id != root_vhd.identifier
        || observed_age >= ramshared_ipc::storage_proof::MAX_STORAGE_AGE_MS
    {
        return None;
    }
    Some(
        host.available_bytes
            .saturating_sub(ramshared_ipc::storage_proof::HOST_STORAGE_RESERVE_BYTES),
    )
}

/// Evaluate cross-layer WSL drain headroom when a planned swapoff exists.
#[allow(clippy::too_many_arguments)]
pub(super) fn evaluate_for_platform(
    managed_bytes: u64,
    remaining_free_bytes: u64,
    emergency_bytes: u64,
    mem: MemInfo,
    guest_free_bytes: Option<u64>,
    platform: Option<ramshared_block::platform::PlatformKind>,
    has_swapoff: bool,
    host: Option<ramshared_tier::HostDrainMemory>,
) -> HeadroomDecision {
    use ramshared_block::platform::PlatformKind;

    if !has_swapoff || ramshared_block::swap_absorber::guest_only_capacity_is_admissible(platform) {
        return evaluate_with_absorber(managed_bytes, remaining_free_bytes, emergency_bytes, mem);
    }
    let wsl_host = match platform {
        Some(PlatformKind::Wsl1 | PlatformKind::Wsl2) => host,
        _ => None,
    };
    let decision = ramshared_tier::DrainBudget {
        total_bytes: mem.total_bytes,
        available_bytes: mem.available_bytes,
        managed_bytes,
        emergency_bytes,
    }
    .evaluate_wsl(guest_free_bytes, wsl_host);
    HeadroomDecision {
        managed_bytes,
        available_bytes: mem.available_bytes,
        emergency_bytes,
        remaining_free_bytes,
        reserve_bytes: decision
            .map(|d| d.reserve_bytes)
            .unwrap_or(MIN_RESERVE_BYTES),
        margin_bytes: MARGIN_BYTES,
        required_bytes: decision.map(|d| d.required_bytes).unwrap_or(u64::MAX),
        deficit_bytes: decision.map(|d| d.deficit_bytes).unwrap_or(u64::MAX),
        verdict: if decision.is_some_and(|d| d.is_safe()) {
            HeadroomVerdict::Safe
        } else {
            HeadroomVerdict::Blocked
        },
        reason: decision
            .map(|d| d.reason)
            .unwrap_or("drain_observation_invalid"),
    }
}

/// Guest-only measurements cannot authorize a WSL drain without verified host telemetry.
#[cfg(test)]
pub(super) fn require_native_or_empty_plan(
    mut decision: HeadroomDecision,
    platform: Option<ramshared_block::platform::PlatformKind>,
    has_swapoff: bool,
) -> HeadroomDecision {
    if has_swapoff && !ramshared_block::swap_absorber::guest_only_capacity_is_admissible(platform) {
        decision.verdict = HeadroomVerdict::Blocked;
        decision.reason = "host_memory_unverified";
    }
    decision
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn guest_only_headroom_cannot_authorize_wsl_or_unknown_swapoff() {
        use ramshared_block::platform::PlatformKind;

        let mem = MemInfo {
            total_bytes: 16 * 1024 * 1024 * 1024,
            available_bytes: 8 * 1024 * 1024 * 1024,
        };
        let decision = evaluate(1024 * 1024 * 1024, 0, mem);
        assert!(decision.is_safe());
        for platform in [None, Some(PlatformKind::Wsl1), Some(PlatformKind::Wsl2)] {
            let blocked = require_native_or_empty_plan(decision.clone(), platform, true);
            assert!(!blocked.is_safe());
            assert_eq!(blocked.reason, "host_memory_unverified");
            assert!(!blocked.absorber_can_help());
        }
        assert!(
            require_native_or_empty_plan(decision.clone(), Some(PlatformKind::NativeLinux), true)
                .is_safe()
        );
        assert!(require_native_or_empty_plan(decision, None, false).is_safe());
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    fn mem(total: u64, available: u64) -> MemInfo {
        MemInfo {
            total_bytes: total,
            available_bytes: available,
        }
    }

    fn current_shape() -> (u64, u64, MemInfo) {
        (
            2 * GIB + 50 * 1024 * 1024,
            5 * GIB + 700 * 1024 * 1024,
            mem(16 * GIB, 5 * GIB),
        )
    }

    #[test]
    fn current_managed_zram_is_safe_only_with_enough_available_ram() {
        let (managed, remaining, mem) = current_shape();
        let decision = evaluate(managed, remaining, mem);
        assert!(decision.is_safe(), "{decision:?}");
        assert_eq!(decision.deficit_bytes, 0);
    }

    #[test]
    fn fallback_free_does_not_substitute_for_available_ram() {
        let decision = evaluate(2 * GIB, 10 * GIB, mem(16 * GIB, 3 * GIB));
        assert!(!decision.is_safe(), "{decision:?}");
        assert_eq!(decision.reason, "managed_swap_exceeds_headroom");
        assert!(decision.deficit_bytes > 0);
    }

    // SPEC swap-drain-safety T12 (RF-11/DT-10): only the emergency absorber is
    // credited; fallback free is reported but never substitutes for it.
    #[test]
    fn attended_headroom_credits_only_the_emergency_absorber() {
        let blocked = evaluate(2 * GIB, 10 * GIB, mem(16 * GIB, 3 * GIB));
        assert!(!blocked.is_safe(), "{blocked:?}");
        assert_eq!(blocked.remaining_free_bytes, 10 * GIB);
        assert_eq!(blocked.emergency_bytes, 0);

        let covered = evaluate_with_absorber(
            2 * GIB,
            10 * GIB,
            blocked.deficit_bytes,
            mem(16 * GIB, 3 * GIB),
        );
        assert!(covered.is_safe(), "{covered:?}");
        assert_eq!(covered.emergency_bytes, blocked.deficit_bytes);
        assert_eq!(covered.deficit_bytes, 0);

        let short = evaluate_with_absorber(
            2 * GIB,
            10 * GIB,
            blocked.deficit_bytes - 1,
            mem(16 * GIB, 3 * GIB),
        );
        assert!(!short.is_safe(), "{short:?}");
        assert_eq!(short.deficit_bytes, 1);
    }

    #[test]
    fn emergency_swap_cannot_replace_the_resident_ram_reserve() {
        let decision = evaluate_with_absorber(GIB, 16 * GIB, 8 * GIB, mem(16 * GIB, GIB));
        assert!(!decision.is_safe());
        assert_eq!(decision.reason, "mem_available_below_reserve");
    }

    #[test]
    fn incident_preflight_with_large_fallback_free_is_blocked() {
        let decision = evaluate(
            2_145_615_872,
            5_627_236_352,
            mem(16_778_395_648, 4_039_417_856),
        );
        assert!(!decision.is_safe(), "{decision:?}");
        assert_eq!(decision.reason, "managed_swap_exceeds_headroom");
        assert_eq!(decision.required_bytes, 4_360_326_349);
        assert_eq!(decision.deficit_bytes, 320_908_493);
    }

    #[test]
    fn managed_swap_exceeding_headroom_is_blocked() {
        let decision = evaluate(7 * GIB + 900 * 1024 * 1024, 0, mem(16 * GIB, 3 * GIB));
        assert!(!decision.is_safe(), "{decision:?}");
        assert_eq!(decision.reason, "managed_swap_exceeds_headroom");
        assert!(decision.deficit_bytes > 0);
    }

    #[test]
    fn missing_or_malformed_samples_are_blocked() {
        assert!(parse_meminfo("").is_none());
        assert!(parse_meminfo("MemTotal: 16384 kB\n").is_none());
        assert!(
            parse_meminfo("MemTotal: 16384 kB\nMemAvailable: 8192 kB\nMemAvailable: 1 kB\n")
                .is_none()
        );
        assert!(parse_meminfo("MemTotal: 16384 kB\nMemAvailable: 20000 kB\n").is_none());
        assert!(parse_meminfo("MemTotal: nope kB\nMemAvailable: 8192 kB\n").is_none());
    }

    #[test]
    fn memory_counters_require_exact_units_and_no_trailing_fields() {
        for total in ["16384", "16384 MB", "16384 kB extra"] {
            assert!(
                parse_meminfo(&format!("MemTotal: {total}\nMemAvailable: 8192 kB\n")).is_none()
            );
        }
        for available in ["8192", "8192 bytes", "8192 kB extra"] {
            assert!(
                parse_meminfo(&format!("MemTotal: 16384 kB\nMemAvailable: {available}\n"))
                    .is_none()
            );
        }
        assert_eq!(
            parse_meminfo("MemTotal: 16384 kB\nMemAvailable: 8192 kB\nHugePages_Total: 0\n"),
            Some(mem(16384 * 1024, 8192 * 1024))
        );
    }

    #[test]
    fn oversized_memory_observations_are_refused() {
        let mut text = "MemTotal: 16384 kB\nMemAvailable: 8192 kB\n".to_string();
        text.push_str(&"UnusedCounter: 1 kB\n".repeat(4096));
        assert!(text.len() > 64 * 1024);
        assert!(parse_meminfo(&text).is_none());
    }

    #[test]
    fn reserve_and_margin_boundaries() {
        let decision = evaluate(1, 0, mem(16 * GIB, GIB));
        assert!(!decision.is_safe(), "1 GiB is below the 1.6 GiB reserve");
        let decision = evaluate(1, 0, mem(16 * GIB, 3 * GIB));
        assert!(decision.is_safe(), "{decision:?}");
        assert_eq!(decision.reserve_bytes, 16 * GIB / 10 + 1);
        assert_eq!(decision.margin_bytes, MARGIN_BYTES);
    }

    #[test]
    fn nbd_outside_swap_is_not_counted() {
        let planned = vec!["/dev/zram0".to_string()];
        let swaps = vec![
            SwapAmount {
                path: "/dev/zram0".to_string(),
                used_bytes: 2 * GIB,
                free_bytes: 0,
            },
            SwapAmount {
                path: "/dev/nbd0".to_string(),
                used_bytes: 9 * GIB,
                free_bytes: 0,
            },
            SwapAmount {
                path: "/dev/sdc".to_string(),
                used_bytes: 6 * GIB,
                free_bytes: 2 * GIB,
            },
        ];
        let (managed, remaining) = summarize_swaps(&planned, &swaps);
        assert_eq!(managed, 2 * GIB);
        assert_eq!(remaining, 2 * GIB);
    }

    #[test]
    fn swap_cached_is_not_credited() {
        let parsed = parse_meminfo(
            "MemTotal: 16384000 kB\nMemAvailable: 1000000 kB\nSwapCached: 9999999 kB\n",
        )
        .expect("valid meminfo");
        assert_eq!(parsed.available_bytes, 1000000 * 1024);
    }

    #[test]
    fn empty_managed_plan_is_safe_without_swapoff() {
        let (managed, remaining) = summarize_swaps(&[], &[]);
        let decision = evaluate(managed, remaining, mem(16 * GIB, 2 * GIB));
        assert!(decision.is_safe(), "{decision:?}");
        assert_eq!(decision.managed_bytes, 0);
    }

    #[test]
    fn wsl_teardown_accepts_fresh_leased_host_memory_and_refuses_stale_or_unleased_status() {
        use ramshared_block::platform::PlatformKind;

        let dir = std::env::temp_dir().join(format!(
            "ramshared-wsl-drain-status-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let status_path = dir.join("cache-status.json");
        let now_ms = 1_800_000_000_000u64;
        let write_doc = |leased: &str, deadline: u64, sampled_at: u64, avail: u64, daemon: &str| {
            let doc = serde_json::json!({
                "schema_version": 1,
                "daemon_instance_id": daemon,
                "written_at_unix_ms": now_ms,
                "ok": true,
                "origin_state": "READY",
                "cache_state": "ACTIVE",
                "cache_unavailable_reason": null,
                "logical_capacity_kib": 4_194_304,
                "vram_cached_kib": 0,
                "gpu_headroom_kib": null,
                "gpu_budget": null,
                "gpu_utilization": null,
                "gpu_cache": null,
                "host_memory": {
                    "sampled_at_unix_ms": sampled_at,
                    "total_bytes": 32 * GIB,
                    "available_bytes": avail,
                    "commit_total_bytes": 40 * GIB,
                    "commit_limit_bytes": 56 * GIB,
                    "page_size_bytes": 4096
                },
                "control_plane_state": leased,
                "control_plane_lease_deadline_ms": deadline,
                "ssd_origin_written_kib": 0,
                "cache_hits": 0,
                "cache_fallback_reads": 0,
                "cache_invalidations": 0,
                "cache_releases": 0,
                "origin_cache_io": {
                    "origin_syncs": 0,
                    "batched_writes": 0,
                    "cache_read_bytes": 0,
                    "cache_read_failures": 0,
                    "cache_write_failures": 0,
                    "promotion_refusals": 0,
                    "allocation_failures": 0,
                    "valid_blocks": 0
                },
                "cache_target_kib": 0,
                "serve_threads": 4,
                "serve_requests_total": 0
            });
            fs::write(&status_path, serde_json::to_vec(&doc).unwrap()).unwrap();
        };

        write_doc(
            "vsock_leased",
            now_ms + 10_000,
            now_ms - 500,
            4 * GIB,
            "42-99",
        );
        let sample = verified_live_host_memory_sample(&status_path, "42-99", Some(now_ms)).unwrap();
        assert_eq!(sample.physical.sampled_at_unix_ms, now_ms - 500);
        assert_eq!(sample.physical.available_bytes, 4 * GIB);
        assert_eq!(sample.commit_limit_bytes, 56 * GIB);
        let verified = verified_live_host_memory(&status_path, "42-99", Some(now_ms)).unwrap();
        let safe = evaluate_for_platform(
            2 * GIB,
            0,
            0,
            mem(16 * GIB, 8 * GIB),
            Some(GIB),
            Some(PlatformKind::Wsl2),
            true,
            Some(verified),
        );
        assert!(safe.is_safe(), "{safe:?}");

        let unknown_platform = evaluate_for_platform(
            2 * GIB,
            0,
            0,
            mem(16 * GIB, 8 * GIB),
            Some(GIB),
            None,
            true,
            Some(verified),
        );
        assert!(!unknown_platform.is_safe());
        assert_eq!(unknown_platform.reason, "host_memory_unverified");

        write_doc(
            "origin_only",
            now_ms + 10_000,
            now_ms - 500,
            4 * GIB,
            "42-99",
        );
        assert!(verified_live_host_memory(&status_path, "42-99", Some(now_ms)).is_none());
        assert!(verified_live_host_memory_sample(&status_path, "42-99", Some(now_ms)).is_none());

        write_doc("vsock_leased", now_ms, now_ms - 500, 4 * GIB, "42-99");
        assert!(verified_live_host_memory(&status_path, "42-99", Some(now_ms)).is_none());

        write_doc(
            "vsock_leased",
            now_ms + 10_000,
            now_ms - 5_000,
            4 * GIB,
            "42-99",
        );
        assert!(verified_live_host_memory(&status_path, "42-99", Some(now_ms)).is_none());

        write_doc(
            "vsock_leased",
            now_ms + 10_000,
            now_ms - 500,
            4 * GIB,
            "other-99",
        );
        assert!(verified_live_host_memory(&status_path, "42-99", Some(now_ms)).is_none());

        write_doc(
            "vsock_leased",
            now_ms + 10_000,
            now_ms - 500,
            2 * GIB,
            "42-99",
        );
        let low = verified_live_host_memory(&status_path, "42-99", Some(now_ms)).unwrap();
        let blocked = evaluate_for_platform(
            2 * GIB,
            0,
            0,
            mem(16 * GIB, 8 * GIB),
            Some(GIB),
            Some(PlatformKind::Wsl2),
            true,
            Some(low),
        );
        assert!(!blocked.is_safe());
        assert_eq!(blocked.reason, "host_mem_available_below_reserve");
        let _ = fs::remove_dir_all(&dir);
    }
}
