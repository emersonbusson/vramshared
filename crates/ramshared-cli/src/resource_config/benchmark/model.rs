// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark/model.rs
//! Pure policy, geometry, scheduling, statistics, and recommendation logic for
//! `ramshared config benchmark` (`SPEC.md` DT-14..DT-16, DT-19, DT-21).

use std::collections::HashSet;
use std::time::Duration;

use ramshared_config::resource_profile::{
    DISK_RESERVE_FLOOR_BYTES, ResourcePlatform, ResourceProfile, StorageVolumeIdentity,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::linux_storage;
use super::super::{InventoryError, MountInfo, ResourceSnapshot, RuntimePlatform, sample_is_fresh};
use super::{BenchmarkRequest, BenchmarkUseCase};

/// Maximum eligible volumes allowed in a single consented benchmark run (DT-14).
pub(crate) const MAX_BENCHMARK_VOLUMES: usize = 8;
/// Rounds executed per run; every volume runs one sample per round (DT-14, DT-15).
pub(crate) const ROUNDS_PER_VOLUME: usize = 3;
/// Fixed sequential buffer size (`1 MiB`) used by the worker (DT-14, DT-21).
pub(crate) const SEQ_BUFFER_BYTES: usize = 1024 * 1024;
/// Sequential chunks written per sample (`31 MiB`).
pub(crate) const SEQ_CHUNKS_PER_SAMPLE: usize = 31;
/// Total sequential write payload per sample (`31 MiB`, DT-21).
pub(crate) const SEQ_WRITE_BYTES: u64 = (SEQ_CHUNKS_PER_SAMPLE as u64) * (SEQ_BUFFER_BYTES as u64);
/// Small durable write block size (`4 KiB`, DT-21).
pub(crate) const SMALL_WRITE_BYTES: usize = 4096;
/// Distinct small writes with individual `fdatasync` per sample (`256`, DT-21).
pub(crate) const SMALL_WRITES_PER_SAMPLE: usize = 256;
/// Stride coprime to `256` so the 256 4 KiB slots are visited in non-sequential order.
const SMALL_WRITE_PERM_STRIDE: usize = 97;
/// Total payload writes per sample (`32 MiB`, DT-14, DT-21).
pub(crate) const SAMPLE_PAYLOAD_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum payload writes per volume across three samples (`96 MiB`, DT-14, DT-19).
pub(crate) const PER_VOLUME_WRITE_CAP_BYTES: u64 =
    (ROUNDS_PER_VOLUME as u64) * SAMPLE_PAYLOAD_BYTES;
/// Maximum payload writes across an 8-volume run (`768 MiB`, DT-14).
pub(crate) const RUN_WRITE_CAP_BYTES: u64 =
    (MAX_BENCHMARK_VOLUMES as u64) * PER_VOLUME_WRITE_CAP_BYTES;
/// Total durable small-write observations per volume across 3 rounds (`768`, DT-21).
pub(crate) const SMALL_WRITE_OBSERVATIONS_PER_VOLUME: usize =
    ROUNDS_PER_VOLUME * SMALL_WRITES_PER_SAMPLE;
/// Required free bytes per volume: `96 MiB + 10 GiB` reserve floor (DT-19).
pub(crate) const BENCHMARK_REQUIRED_FREE_BYTES: u64 =
    PER_VOLUME_WRITE_CAP_BYTES + DISK_RESERVE_FLOOR_BYTES;
/// Per-volume sequential child-worker deadline (`120 s`, DT-21).
pub(crate) const VOLUME_WORKER_DEADLINE: Duration = Duration::from_secs(120);
/// Relative median advantage required to recommend a leader (`10%`, DT-15).
pub(crate) const LEADER_MARGIN_RATIO: f64 = 0.10;
/// Minimum available guest RAM required before running benchmark I/O (128 MiB).
const MIN_AVAILABLE_RAM_BYTES: u64 = 128 * 1024 * 1024;

/// Stable Linux volume candidate resolved to one current eligible mount.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct BenchmarkVolume {
    pub(crate) filesystem_uuid: String,
    pub(crate) device_identity: String,
    pub(crate) display_name: String,
    pub(crate) mountpoint: String,
    pub(crate) mount_id: u64,
    pub(crate) major_minor: String,
    pub(crate) filesystem: String,
    pub(crate) free_bytes: u64,
    pub(crate) required_free_bytes: u64,
    pub(crate) capacity_observed_unix_ms: u64,
}

impl BenchmarkVolume {
    /// Stable volume label suitable for logs and `--batch` selectors (no transient dev paths).
    pub(crate) fn stable_id(&self) -> String {
        format!("{}@{}", self.filesystem_uuid, self.device_identity)
    }
}

/// Reviewed benchmark preview bound by a SHA-256 `plan_hash` before any write occurs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct BenchmarkPlan {
    pub(crate) schema_version: u32,
    pub(crate) plan_hash: String,
    pub(crate) platform: &'static str,
    pub(crate) use_case: BenchmarkUseCase,
    pub(crate) observed_unix_ms: u64,
    pub(crate) rounds: usize,
    pub(crate) sample_payload_bytes: u64,
    pub(crate) per_volume_write_cap_bytes: u64,
    pub(crate) max_total_write_bytes: u64,
    pub(crate) per_volume_deadline_secs: u64,
    pub(crate) required_free_bytes_per_volume: u64,
    pub(crate) volumes: Vec<BenchmarkVolume>,
    pub(crate) round_schedule: Vec<Vec<String>>,
    pub(crate) writes_performed: bool,
}

/// Live system/health measurements evaluated at preflight and before each volume/sample.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeHealthSnapshot {
    pub(crate) ramshared_off: bool,
    pub(crate) psi_full_avg10: Option<f64>,
    pub(crate) telemetry_healthy: bool,
    pub(crate) telemetry_detail: Option<String>,
}

/// Per-sample telemetry returned by a verified child worker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkerSampleOutcome {
    pub(crate) bytes_written: u64,
    pub(crate) seq_bytes_written: u64,
    pub(crate) small_write_count: usize,
    pub(crate) seq_elapsed_ns: u64,
    pub(crate) small_write_latencies_ns: Vec<u64>,
    pub(crate) expected_sha256: String,
    pub(crate) observed_sha256: String,
    pub(crate) file_dev: u64,
    pub(crate) file_ino: u64,
    pub(crate) cleanup_verified: bool,
}

/// Per-volume aggregated summary across all three samples (`DT-15`, `DT-21`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct VolumeBenchmarkResult {
    pub(crate) filesystem_uuid: String,
    pub(crate) device_identity: String,
    pub(crate) stable_id: String,
    pub(crate) display_name: String,
    pub(crate) sequential_mibs_per_sec: Vec<f64>,
    pub(crate) sequential_median_mibs_per_sec: f64,
    pub(crate) sequential_stddev_mibs_per_sec: f64,
    pub(crate) small_write_sample_count: usize,
    pub(crate) small_write_median_us: f64,
    pub(crate) small_write_p99_us: f64,
    pub(crate) small_write_stddev_us: f64,
    pub(crate) total_bytes_written: u64,
    pub(crate) readback_hashes: Vec<String>,
    pub(crate) cleanup_verified: bool,
}

/// Use-case-scoped recommendation output (`DT-15`: never a universal fastest claim).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct BenchmarkRecommendation {
    pub(crate) use_case: BenchmarkUseCase,
    pub(crate) status: &'static str,
    pub(crate) recommended_stable_id: Option<String>,
    pub(crate) rationale: String,
}

/// Validate host RAM/PSI/RamShared status before starting or continuing benchmark writes (DT-16).
pub(crate) fn validate_runtime_health(
    snapshot: &ResourceSnapshot,
    health: &RuntimeHealthSnapshot,
) -> Result<(), InventoryError> {
    if !health.ramshared_off
        || snapshot
            .swaps
            .iter()
            .any(|swap| swap.filename.contains("ramshared"))
    {
        return Err(InventoryError(
            "benchmark refused while RamShared is active; deactivate tiers before storage testing"
                .into(),
        ));
    }
    if !health.telemetry_healthy {
        let reason = health
            .telemetry_detail
            .as_deref()
            .unwrap_or("control telemetry is unavailable or degraded");
        return Err(InventoryError(format!(
            "benchmark refused because telemetry is unhealthy: {reason}"
        )));
    }
    let psi = health.psi_full_avg10.ok_or_else(|| {
        InventoryError(
            "benchmark refused: /proc/pressure/memory PSI telemetry is unavailable or invalid"
                .into(),
        )
    })?;
    if !psi.is_finite() || psi < 0.0 {
        return Err(InventoryError(
            "benchmark refused: invalid non-finite PSI telemetry reading".into(),
        ));
    }
    if psi >= crate::supervisor::memory_policy::GUARDED_PSI_FULL_AVG10_PERCENT {
        return Err(InventoryError(format!(
            "benchmark refused: memory pressure PSI full avg10={psi:.2}% exceeds safe ceiling"
        )));
    }
    if !snapshot.guest_memory.required_counters_available {
        return Err(InventoryError(
            "benchmark refused: required guest memory counters are unavailable".into(),
        ));
    }
    let (Some(total), Some(available)) = (
        snapshot.guest_memory.total_bytes,
        snapshot.guest_memory.available_bytes,
    ) else {
        return Err(InventoryError(
            "benchmark refused: memory availability cannot be verified".into(),
        ));
    };
    let floor = (total / 10).max(MIN_AVAILABLE_RAM_BYTES);
    if available < floor {
        return Err(InventoryError(format!(
            "benchmark refused: available RAM ({available} bytes) is below safety floor ({floor} bytes)"
        )));
    }
    Ok(())
}

/// Build an immutable, capacity- and freshness-checked benchmark preview (`DT-14`, `DT-16`, `DT-19`).
pub(crate) fn build_benchmark_plan(
    snapshot: &ResourceSnapshot,
    request: &BenchmarkRequest,
    profile_text: Option<&str>,
    health: &RuntimeHealthSnapshot,
    now_unix_ms: u64,
) -> Result<BenchmarkPlan, InventoryError> {
    if snapshot.platform == RuntimePlatform::Wsl2 {
        return Err(InventoryError(
            "WSL2 host-volume benchmarking is not available yet; Windows host controller integration is required"
                .into(),
        ));
    }
    validate_runtime_health(snapshot, health)?;
    if !sample_is_fresh(Some(snapshot.observed_unix_ms), now_unix_ms) {
        return Err(InventoryError(
            "storage inventory snapshot is missing or stale; refresh before benchmarking".into(),
        ));
    }

    let eligible = discover_eligible_native_volumes(snapshot, now_unix_ms);
    if eligible.is_empty() {
        return Err(InventoryError(
            "no eligible native Linux ext4/XFS local volumes are available for benchmarking".into(),
        ));
    }

    let selected = select_volumes(&eligible, request, profile_text)?;
    for volume in &selected {
        validate_volume_reserve(volume)?;
    }

    let round_schedule = rotate_schedule(&selected);
    let max_total_write_bytes = (selected.len() as u64)
        .checked_mul(PER_VOLUME_WRITE_CAP_BYTES)
        .filter(|total| *total <= RUN_WRITE_CAP_BYTES)
        .ok_or_else(|| InventoryError("benchmark write ceiling overflow".into()))?;

    let plan_hash = compute_plan_hash(request.use_case, &selected, &round_schedule)?;
    Ok(BenchmarkPlan {
        schema_version: 1,
        plan_hash,
        platform: "native_linux",
        use_case: request.use_case,
        observed_unix_ms: snapshot.observed_unix_ms,
        rounds: ROUNDS_PER_VOLUME,
        sample_payload_bytes: SAMPLE_PAYLOAD_BYTES,
        per_volume_write_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
        max_total_write_bytes,
        per_volume_deadline_secs: VOLUME_WORKER_DEADLINE.as_secs(),
        required_free_bytes_per_volume: BENCHMARK_REQUIRED_FREE_BYTES,
        volumes: selected,
        round_schedule,
        writes_performed: false,
    })
}

/// Discover all unique, fresh, eligible native Linux volumes with stable identities.
pub(crate) fn discover_eligible_native_volumes(
    snapshot: &ResourceSnapshot,
    now_unix_ms: u64,
) -> Vec<BenchmarkVolume> {
    let mut volumes = Vec::new();
    let mut seen = HashSet::new();
    for device in &snapshot.block_devices {
        let (Some(filesystem_uuid), Some(device_identity)) = (
            device.uuid.as_deref(),
            linux_storage::backing_device_identity(device),
        ) else {
            continue;
        };
        let key = StorageVolumeIdentity::Linux {
            filesystem_uuid: filesystem_uuid.to_owned(),
            device_identity: device_identity.to_owned(),
        };
        let Ok((mount, obs)) =
            linux_storage::observe_volume(snapshot, filesystem_uuid, device_identity, now_unix_ms)
        else {
            continue;
        };
        if !seen.insert(key) {
            continue;
        }
        if let Some(volume) = build_volume_record(
            device,
            mount,
            obs.free_bytes,
            filesystem_uuid,
            device_identity,
        ) {
            volumes.push(volume);
        }
    }
    volumes.sort_by_cached_key(BenchmarkVolume::stable_id);
    volumes
}

fn build_volume_record(
    device: &ramshared_block::storage_inventory::BlockDevice,
    mount: &MountInfo,
    free_bytes: u64,
    filesystem_uuid: &str,
    device_identity: &str,
) -> Option<BenchmarkVolume> {
    let observed_unix_ms = mount.capacity_observed_unix_ms?;
    Some(BenchmarkVolume {
        filesystem_uuid: filesystem_uuid.to_owned(),
        device_identity: device_identity.to_owned(),
        display_name: format!(
            "{} at {}",
            device.model.as_deref().unwrap_or(&device.name),
            mount.mountpoint
        ),
        mountpoint: mount.mountpoint.clone(),
        mount_id: mount.mount_id,
        major_minor: mount.major_minor.clone(),
        filesystem: mount.filesystem.clone(),
        free_bytes,
        required_free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        capacity_observed_unix_ms: observed_unix_ms,
    })
}

fn select_volumes(
    eligible: &[BenchmarkVolume],
    request: &BenchmarkRequest,
    profile_text: Option<&str>,
) -> Result<Vec<BenchmarkVolume>, InventoryError> {
    if let Some(text) = profile_text {
        return select_from_profile(eligible, text);
    }
    if !request.batch.is_empty() {
        return select_from_batch(eligible, &request.batch);
    }
    if eligible.len() > MAX_BENCHMARK_VOLUMES {
        return Err(InventoryError(format!(
            "found {} eligible volumes (max {MAX_BENCHMARK_VOLUMES} per run); select at most {MAX_BENCHMARK_VOLUMES} via --batch or --profile",
            eligible.len()
        )));
    }
    Ok(eligible.to_vec())
}

fn select_from_profile(
    eligible: &[BenchmarkVolume],
    profile_text: &str,
) -> Result<Vec<BenchmarkVolume>, InventoryError> {
    let profile = ResourceProfile::parse(profile_text)
        .map_err(|error| InventoryError(format!("resource profile is invalid: {error}")))?;
    profile
        .validate_for(ResourcePlatform::NativeLinux)
        .map_err(|error| InventoryError(format!("resource profile is invalid here: {error}")))?;
    if profile.targets.is_empty() {
        return Err(InventoryError(
            "selected resource profile declares no storage targets to benchmark".into(),
        ));
    }
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for target in &profile.targets {
        let StorageVolumeIdentity::Linux {
            filesystem_uuid,
            device_identity,
        } = super::super::resource_volume_identity(target)
        else {
            return Err(InventoryError(
                "resource profile contains a non-Linux target".into(),
            ));
        };
        let key = (filesystem_uuid, device_identity);
        if !seen.insert(key.clone()) {
            continue;
        }
        let found = eligible
            .iter()
            .find(|v| v.filesystem_uuid == key.0 && v.device_identity == key.1)
            .ok_or_else(|| {
                InventoryError(format!(
                    "profile volume {}@{} is absent, ambiguous, stale, or ineligible",
                    key.0, key.1
                ))
            })?;
        selected.push(found.clone());
    }
    enforce_volume_count(&selected)?;
    Ok(selected)
}

fn select_from_batch(
    eligible: &[BenchmarkVolume],
    selectors: &[String],
) -> Result<Vec<BenchmarkVolume>, InventoryError> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for selector in selectors {
        let matches: Vec<&BenchmarkVolume> = eligible
            .iter()
            .filter(|v| v.stable_id() == *selector || v.filesystem_uuid == *selector)
            .collect();
        match matches.as_slice() {
            [] => {
                return Err(InventoryError(format!(
                    "selected volume {selector} is absent, stale, or ineligible"
                )));
            }
            [single] => {
                if !seen.insert(single.stable_id()) {
                    return Err(InventoryError(format!(
                        "duplicate volume selector {selector} in --batch"
                    )));
                }
                selected.push((*single).clone());
            }
            _ => {
                return Err(InventoryError(format!(
                    "volume selector {selector} matches multiple eligible volumes; use UUID@device_identity"
                )));
            }
        }
    }
    enforce_volume_count(&selected)?;
    Ok(selected)
}

fn enforce_volume_count(selected: &[BenchmarkVolume]) -> Result<(), InventoryError> {
    if selected.is_empty() {
        return Err(InventoryError("no volumes selected for benchmark".into()));
    }
    if selected.len() > MAX_BENCHMARK_VOLUMES {
        return Err(InventoryError(format!(
            "selected {} volumes, exceeding the maximum {MAX_BENCHMARK_VOLUMES} per run",
            selected.len()
        )));
    }
    Ok(())
}

/// Validate per-volume free capacity (`>= 96 MiB + 10 GiB`, DT-19).
pub(crate) fn validate_volume_reserve(volume: &BenchmarkVolume) -> Result<(), InventoryError> {
    if volume.free_bytes < BENCHMARK_REQUIRED_FREE_BYTES {
        return Err(InventoryError(format!(
            "volume {} has {} free bytes; benchmark requires at least {} bytes (96 MiB + 10 GiB reserve)",
            volume.stable_id(),
            volume.free_bytes,
            BENCHMARK_REQUIRED_FREE_BYTES
        )));
    }
    Ok(())
}

/// Revalidate one volume against a freshly captured snapshot before running its sample (DT-16).
pub(crate) fn revalidate_volume<'a>(
    snapshot: &'a ResourceSnapshot,
    expected: &BenchmarkVolume,
    health: &RuntimeHealthSnapshot,
    now_unix_ms: u64,
) -> Result<(&'a MountInfo, BenchmarkVolume), InventoryError> {
    if snapshot.platform != RuntimePlatform::NativeLinux {
        return Err(InventoryError("native Linux platform required".into()));
    }
    validate_runtime_health(snapshot, health)?;
    let (mount, obs) = linux_storage::observe_volume(
        snapshot,
        &expected.filesystem_uuid,
        &expected.device_identity,
        now_unix_ms,
    )
    .map_err(|(code, msg)| InventoryError(format!("volume recheck failed ({code}): {msg}")))?;
    if mount.mount_id != expected.mount_id
        || mount.major_minor != expected.major_minor
        || mount.mountpoint != expected.mountpoint
        || !mount.filesystem.eq_ignore_ascii_case(&expected.filesystem)
    {
        return Err(InventoryError(format!(
            "volume {} mount identity drifted during benchmark run",
            expected.stable_id()
        )));
    }
    let mut refreshed = expected.clone();
    refreshed.free_bytes = obs.free_bytes;
    refreshed.capacity_observed_unix_ms = mount.capacity_observed_unix_ms.unwrap_or(0);
    validate_volume_reserve(&refreshed)?;
    Ok((mount, refreshed))
}

/// Generate the 3-round rotated volume execution order (`DT-15`).
pub(crate) fn rotate_schedule(volumes: &[BenchmarkVolume]) -> Vec<Vec<String>> {
    let count = volumes.len();
    if count == 0 {
        return Vec::new();
    }
    (0..ROUNDS_PER_VOLUME)
        .map(|round| {
            (0..count)
                .map(|offset| volumes[(round + offset) % count].stable_id())
                .collect()
        })
        .collect()
}

/// Map logical 4 KiB slot index `0..256` from sequential write step `0..256` without overlap.
pub(crate) const fn small_write_slot(step: usize) -> usize {
    (step * SMALL_WRITE_PERM_STRIDE) % SMALL_WRITES_PER_SAMPLE
}

/// Deterministically fill a buffer slice for one 4 KiB page at `page_index` (`0..8192`).
pub(crate) fn fill_deterministic_page(sample_seed: u64, page_index: u32, buf: &mut [u8]) {
    let mut state = sample_seed ^ ((u64::from(page_index) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    for word in buf.as_chunks_mut::<8>().0 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        word.copy_from_slice(&z.to_le_bytes());
    }
}

/// Compute a deterministic 64-bit sample seed from `tx_id`, round, and volume identity.
pub(crate) fn sample_seed(tx_id: &str, round: usize, stable_id: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(tx_id.as_bytes());
    hasher.update(b"\0");
    hasher.update((round as u64).to_le_bytes());
    hasher.update(b"\0");
    hasher.update(stable_id.as_bytes());
    let digest = hasher.finalize();
    u64::from_le_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ])
}

/// Summarize one volume's 3 verified samples into DT-15 / DT-21 metrics.
pub(crate) fn summarize_volume(
    volume: &BenchmarkVolume,
    samples: &[WorkerSampleOutcome],
) -> Result<VolumeBenchmarkResult, InventoryError> {
    if samples.len() != ROUNDS_PER_VOLUME {
        return Err(InventoryError(format!(
            "volume {} produced {} samples instead of {ROUNDS_PER_VOLUME}",
            volume.stable_id(),
            samples.len()
        )));
    }
    let mut seq_speeds = Vec::with_capacity(ROUNDS_PER_VOLUME);
    let mut small_latencies_us = Vec::with_capacity(SMALL_WRITE_OBSERVATIONS_PER_VOLUME);
    let mut readback_hashes = Vec::with_capacity(ROUNDS_PER_VOLUME);
    let mut total_bytes_written = 0u64;

    for sample in samples {
        if sample.bytes_written != SAMPLE_PAYLOAD_BYTES
            || sample.seq_bytes_written != SEQ_WRITE_BYTES
            || sample.small_write_count != SMALL_WRITES_PER_SAMPLE
            || sample.small_write_latencies_ns.len() != SMALL_WRITES_PER_SAMPLE
            || !sample.cleanup_verified
            || sample.observed_sha256 != sample.expected_sha256
        {
            return Err(InventoryError(format!(
                "sample verification failed for volume {}",
                volume.stable_id()
            )));
        }
        if sample.seq_elapsed_ns == 0 || sample.small_write_latencies_ns.contains(&0) {
            return Err(InventoryError(format!(
                "unmeasurable zero-duration benchmark timing for volume {}",
                volume.stable_id()
            )));
        }
        total_bytes_written = total_bytes_written
            .checked_add(sample.bytes_written)
            .ok_or_else(|| InventoryError("benchmark written-byte counter overflow".into()))?;
        let secs = (sample.seq_elapsed_ns as f64) / 1_000_000_000.0;
        let mibs = (sample.seq_bytes_written as f64) / (1024.0 * 1024.0);
        seq_speeds.push(mibs / secs);
        for ns in &sample.small_write_latencies_ns {
            small_latencies_us.push((*ns as f64) / 1_000.0);
        }
        readback_hashes.push(sample.observed_sha256.clone());
    }
    if small_latencies_us.len() != SMALL_WRITE_OBSERVATIONS_PER_VOLUME {
        return Err(InventoryError(
            "unexpected small-write observation count".into(),
        ));
    }

    Ok(VolumeBenchmarkResult {
        filesystem_uuid: volume.filesystem_uuid.clone(),
        device_identity: volume.device_identity.clone(),
        stable_id: volume.stable_id(),
        display_name: volume.display_name.clone(),
        sequential_median_mibs_per_sec: median(&seq_speeds),
        sequential_stddev_mibs_per_sec: stddev(&seq_speeds),
        sequential_mibs_per_sec: seq_speeds,
        small_write_sample_count: small_latencies_us.len(),
        small_write_median_us: median(&small_latencies_us),
        small_write_p99_us: nearest_rank_percentile(&small_latencies_us, 0.99),
        small_write_stddev_us: stddev(&small_latencies_us),
        total_bytes_written,
        readback_hashes,
        cleanup_verified: true,
    })
}

/// Evaluate DT-15 leader recommendation (`>= 10%` median difference and no metric conflict).
pub(crate) fn recommend_leader(
    use_case: BenchmarkUseCase,
    results: &[VolumeBenchmarkResult],
) -> BenchmarkRecommendation {
    if results.len() <= 1 {
        return BenchmarkRecommendation {
            use_case,
            status: "single_volume",
            recommended_stable_id: results.first().map(|r| r.stable_id.clone()),
            rationale: "single-volume baseline recorded; compare two or more volumes for ranking"
                .into(),
        };
    }

    let leader = pick_leader(use_case, results);
    for peer in results {
        if peer.stable_id == leader.stable_id {
            continue;
        }
        if let Some(tie_reason) = compare_pair(use_case, leader, peer) {
            return BenchmarkRecommendation {
                use_case,
                status: "tie",
                recommended_stable_id: None,
                rationale: tie_reason,
            };
        }
    }

    BenchmarkRecommendation {
        use_case,
        status: "recommended",
        recommended_stable_id: Some(leader.stable_id.clone()),
        rationale: format!(
            "{} leads for {:?} use case by at least 10% median margin without metric conflict",
            leader.stable_id, use_case
        ),
    }
}

fn pick_leader(
    use_case: BenchmarkUseCase,
    results: &[VolumeBenchmarkResult],
) -> &VolumeBenchmarkResult {
    results
        .iter()
        .min_by(|a, b| match use_case {
            BenchmarkUseCase::Swap => a.small_write_median_us.total_cmp(&b.small_write_median_us),
            BenchmarkUseCase::Origin => b
                .sequential_median_mibs_per_sec
                .total_cmp(&a.sequential_median_mibs_per_sec),
        })
        .unwrap_or(&results[0])
}

fn compare_pair(
    use_case: BenchmarkUseCase,
    leader: &VolumeBenchmarkResult,
    peer: &VolumeBenchmarkResult,
) -> Option<String> {
    match use_case {
        BenchmarkUseCase::Swap => {
            if peer.small_write_median_us <= 0.0 {
                return Some("tie: invalid non-positive latency baseline".into());
            }
            let diff_ratio = (peer.small_write_median_us - leader.small_write_median_us)
                / peer.small_write_median_us;
            if diff_ratio < LEADER_MARGIN_RATIO {
                return Some(format!(
                    "tie: small-write latency difference vs {} is {:.1}% (< 10% threshold)",
                    peer.stable_id,
                    diff_ratio * 100.0
                ));
            }
            if peer.sequential_median_mibs_per_sec > leader.sequential_median_mibs_per_sec {
                return Some(format!(
                    "tie: {} wins durable 4 KiB latency while {} leads sequential throughput",
                    leader.stable_id, peer.stable_id
                ));
            }
        }
        BenchmarkUseCase::Origin => {
            if leader.sequential_median_mibs_per_sec <= 0.0 {
                return Some("tie: invalid non-positive throughput baseline".into());
            }
            let diff_ratio = (leader.sequential_median_mibs_per_sec
                - peer.sequential_median_mibs_per_sec)
                / leader.sequential_median_mibs_per_sec;
            if diff_ratio < LEADER_MARGIN_RATIO {
                return Some(format!(
                    "tie: sequential throughput difference vs {} is {:.1}% (< 10% threshold)",
                    peer.stable_id,
                    diff_ratio * 100.0
                ));
            }
            if peer.small_write_median_us < leader.small_write_median_us {
                return Some(format!(
                    "tie: {} wins sequential throughput while {} leads durable 4 KiB latency",
                    leader.stable_id, peer.stable_id
                ));
            }
        }
    }
    None
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    }
}

fn nearest_rank_percentile(values: &[f64], percentile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((sorted.len() as f64) * percentile).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len().saturating_sub(1));
    sorted[idx]
}

fn stddev(values: &[f64]) -> f64 {
    if values.len() <= 1 {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values
        .iter()
        .map(|v| {
            let d = *v - mean;
            d * d
        })
        .sum::<f64>()
        / n;
    variance.sqrt()
}

fn compute_plan_hash(
    use_case: BenchmarkUseCase,
    volumes: &[BenchmarkVolume],
    schedule: &[Vec<String>],
) -> Result<String, InventoryError> {
    #[derive(Serialize)]
    struct PlanHashInput<'a> {
        schema_version: u32,
        use_case: BenchmarkUseCase,
        per_volume_write_cap_bytes: u64,
        required_free_bytes_per_volume: u64,
        volumes: Vec<(&'a str, &'a str, u64, &'a str)>,
        schedule: &'a [Vec<String>],
    }
    let encoded = serde_json::to_vec(&PlanHashInput {
        schema_version: 1,
        use_case,
        per_volume_write_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
        required_free_bytes_per_volume: BENCHMARK_REQUIRED_FREE_BYTES,
        volumes: volumes
            .iter()
            .map(|v| {
                (
                    v.filesystem_uuid.as_str(),
                    v.device_identity.as_str(),
                    v.mount_id,
                    v.major_minor.as_str(),
                )
            })
            .collect(),
        schedule,
    })
    .map_err(|error| InventoryError(format!("cannot encode benchmark plan hash: {error}")))?;
    Ok(Sha256::digest(&encoded)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
