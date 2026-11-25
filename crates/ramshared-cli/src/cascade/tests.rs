// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub(super) use super::*;

pub(super) use crate::release_coherence::ReleaseCoherence;

pub(super) use std::path::PathBuf;

/// The sealed pair a guardian fixture is written for (AUD-40).
pub(super) fn test_identity() -> sealed_identity::SealedIdentity {
    sealed_identity::SealedIdentity {
        distro: "Ubuntu-24.04".to_string(),
        user_sid: "S-1-5-21-1-2-3-1001".to_string(),
        task_name: sealed_identity::GUARDIAN_TASK_NAME.to_string(),
    }
}

/// Minimal health proof for the given sealed pair. Both halves are present:
/// a proof that names only one of them is ambiguous and must be refused.
pub(super) fn health_json(distro: &str, user_sid: &str, state: &str) -> String {
    format!(
        r#"{{"schema_version":3,"distro":"{distro}","user_sid":"{user_sid}","state":"{state}"}}"#
    )
}

pub(super) fn test_health_body(state: &str) -> String {
    let identity = test_identity();
    health_json(&identity.distro, &identity.user_sid, state)
}

/// Empty host-sealed config for a temp tree, plus its state directory.
pub(super) fn write_sealed_config(root: &Path, distro: &str, user_sid: &str) -> (PathBuf, PathBuf) {
    let config = root.join("guardian-config.json");
    let state_dir = root.join("guardian-state");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
            &config,
            format!(
                r#"{{"schema_version":2,"distro":"{distro}","user_sid":"{user_sid}","task_name":"RamSharedWslGuardian.v1","guardian_policy":{{}}}}"#
            ),
        )
        .unwrap();
    (config, state_dir)
}

/// Minimal snapshot for rendering tests. Every field that `cache_line` does
/// not read is a healthy default, so a failure points at the field the test
/// is about.
pub(super) fn snapshot_for_cache_line(
    cache_state: CacheState,
    vram_cached_kib: Option<u64>,
    gpu_headroom_kib: Option<u64>,
) -> CascadeSnapshot {
    CascadeSnapshot {
        origin_cache_mode: Some(false),
        zram: TierSample::default(),
        vram: TierSample::default(),
        disk: TierSample::default(),
        ghost: false,
        order_ok: true,
        daemon_alive: true,
        daemon_pid: Some(1),
        capacity_guaranteed: Some(true),
        disk_baseline_kib: Some(0),
        demote: DemoteSnapshot::default(),
        active_kib: 1024,
        control_state: ControlState::Healthy,
        origin_state: OriginState::Ready,
        cache_state,
        guardian_state: GuardianState::Healthy,
        cache_target_kib: Some(33_792),
        logical_capacity_kib: Some(1024),
        vram_cached_kib,
        gpu_headroom_kib,
        gpu_budget: None,
        gpu_utilization: None,
        gpu_cache: None,
        origin_cache_io: None,
        cache_fallback_reads: None,
        cache_hits: None,
        host_memory: None,
        host_storage: None,
        cache_invalidations: None,
        cache_releases: None,
        zram_compression: None,
        zram_tier_state: crate::monitor::ZramTierState::Inactive,
        ssd_origin_written_kib: Some(0),
        fallback_swap_used_kib: Some(0),
        release_coherence: ReleaseCoherence::Coherent,
        measurement_errors: Vec::new(),
        topology: TopologyFacts::default(),
        cache_unavailable_reason: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
    }
}

/// A cascade snapshot that every pressure gate accepts, for tests that
/// then degrade exactly one witness.
pub(super) fn ready_snapshot() -> CascadeSnapshot {
    let tier = TierSample {
        present: true,
        prio: Some(200),
        size_kib: 1024,
        used_kib: 0,
    };
    CascadeSnapshot {
        origin_cache_mode: Some(false),
        zram: tier.clone(),
        vram: TierSample {
            prio: Some(100),
            ..tier.clone()
        },
        disk: TierSample {
            prio: Some(-2),
            ..tier
        },
        ghost: false,
        order_ok: true,
        daemon_alive: true,
        daemon_pid: Some(1),
        capacity_guaranteed: Some(true),
        disk_baseline_kib: Some(0),
        demote: DemoteSnapshot::default(),
        active_kib: 1024,
        control_state: ControlState::Healthy,
        origin_state: OriginState::Ready,
        cache_state: CacheState::Active,
        guardian_state: GuardianState::Healthy,
        cache_target_kib: Some(33_792),
        logical_capacity_kib: Some(1024),
        vram_cached_kib: Some(256),
        gpu_headroom_kib: Some(768),
        gpu_budget: None,
        gpu_utilization: None,
        gpu_cache: None,
        origin_cache_io: None,
        cache_fallback_reads: None,
        cache_hits: None,
        host_memory: None,
        host_storage: None,
        cache_invalidations: None,
        cache_releases: None,
        zram_compression: None,
        zram_tier_state: crate::monitor::ZramTierState::Inactive,
        ssd_origin_written_kib: Some(0),
        fallback_swap_used_kib: Some(0),
        release_coherence: ReleaseCoherence::Coherent,
        measurement_errors: Vec::new(),
        topology: TopologyFacts::default(),
        cache_unavailable_reason: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
    }
}

pub(super) fn parse_proc_swaps(text: &str) -> Vec<SwapEntry> {
    super::parse_proc_swaps(text).expect("strict /proc/swaps fixture")
}

pub(super) fn parse(args: &[&str]) -> Result<UpArgs, CascadeError> {
    let args = args.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    parse_up_args_from(&args, "ramsharedd".to_string())
}

pub(super) fn valid_supervisor_status_v3() -> serde_json::Value {
    serde_json::json!({
        "schema_version": 3,
        "control_state": "CRITICAL",
        "healthy_samples": 0,
        "action_results": [
            {"action": "close_admission", "status": "succeeded", "error": null},
            {"action": "reduce_vram_cache", "status": "failed", "error": "fixture refusal"},
            {"action": "freeze_discardable", "status": "succeeded", "error": null},
            {"action": "request_reclaim", "status": "succeeded", "error": null},
        ],
        "daemon_instance_id": "daemon-1",
        "supervisor_identity": {
            "boot_id": "fixture-boot",
            "pid": 1,
            "start_time": 1,
            "nonce": "fixture-nonce",
        },
        "written_at_unix_ms": 1_000,
    })
}

/// The named predicate tokens a pressure-gate refusal reports.
///
/// Splits the `refused: …` list out of the error text so an assertion can
/// match a whole token. A bare substring check is ambiguous here: the
/// refusal token `daemon_dead` is a prefix of the lifecycle reason
/// `daemon_dead_hot_vram`.
pub(super) fn refused_tokens(err: &str) -> Vec<&str> {
    err.split_once("refused: ")
        .and_then(|(_, rest)| rest.split(';').next())
        .map(|list| list.split(", ").collect())
        .unwrap_or_default()
}

pub(super) fn clear_sh_script() {
    SH_SCRIPT.with(|q| q.borrow_mut().clear());
}

pub(super) fn push_sh(pat: &str, res: Result<&str, &str>) {
    SH_SCRIPT.with(|q| {
        q.borrow_mut().push_back((
            pat.to_string(),
            res.map(str::to_string).map_err(str::to_string),
        ));
    });
}

pub(super) fn set_test_swaps(s: Option<&str>) {
    TEST_SWAPS.with(|c| *c.borrow_mut() = s.map(str::to_string));
}

pub(super) fn set_test_mem(n: Option<u64>) {
    TEST_MEM_AVAILABLE.with(|c| *c.borrow_mut() = n);
}

pub(super) fn set_test_mb(v: Option<(&str, u64)>) {
    TEST_ENV_MB.with(|c| *c.borrow_mut() = v.map(|(k, n)| (k.to_string(), n)));
}

pub(super) fn set_test_cascade_conf(v: Option<Result<String, String>>) {
    TEST_CASCADE_CONF.with(|c| *c.borrow_mut() = v);
}

pub(super) fn clear_test_seams() {
    clear_sh_script();
    set_test_swaps(None);
    TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().clear());
    TEST_SWAPS_ERROR.with(|cell| *cell.borrow_mut() = None);
    set_test_cascade_conf(None);
    set_test_mem(None);
    set_test_mb(None);
}

pub(super) fn sealed_origin_fixture_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-sealed-origin-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[path = "tests/health_and_snapshot.rs"]
mod health_and_snapshot;
#[path = "tests/lifecycle_extra.rs"]
mod lifecycle_extra;
#[path = "tests/relay_and_status.rs"]
mod relay_and_status;
#[path = "tests/sealed_origin_and_seams.rs"]
mod sealed_origin_and_seams;
#[path = "tests/swaps_and_cli_parsing.rs"]
mod swaps_and_cli_parsing;
