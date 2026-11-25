// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/storage_benchmark.rs
//! Bounded Windows fixed-volume storage speed diagnostic and recommendation (`DT-27`).
//!
//! Enumerates local fixed NTFS volumes (`GetLogicalDrives` + `DRIVE_FIXED`),
//! enforces the shared `10 GiB + 96 MiB` reserve floor, verifies volume GUID
//! correspondence before writing, runs 3 rotated sequential + 4 KiB
//! `FlushFileBuffers` (`sync_all`) probe rounds with full SHA-256 read-back
//! verification, and cleans every probe artifact deterministically.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

pub const MAX_BENCHMARK_VOLUMES: usize = 8;
pub const ROUNDS_PER_VOLUME: usize = 3;
pub const SEQ_BUFFER_BYTES: usize = 1024 * 1024;
pub const SEQ_CHUNKS_PER_SAMPLE: usize = 31;
pub const SMALL_WRITE_BYTES: usize = 4096;
pub const SMALL_WRITES_PER_SAMPLE: usize = 256;
const SMALL_WRITE_PERM_STRIDE: usize = 97;
pub const SAMPLE_PAYLOAD_BYTES: u64 = 32 * 1024 * 1024;
pub const PER_VOLUME_WRITE_CAP_BYTES: u64 = (ROUNDS_PER_VOLUME as u64) * SAMPLE_PAYLOAD_BYTES;
pub const DISK_RESERVE_FLOOR_BYTES: u64 = 10 * 1024 * 1024 * 1024;
pub const BENCHMARK_REQUIRED_FREE_BYTES: u64 =
    PER_VOLUME_WRITE_CAP_BYTES + DISK_RESERVE_FLOOR_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeGeometry {
    pub seq_buffer_bytes: usize,
    pub seq_chunks: usize,
    pub small_write_bytes: usize,
    pub small_writes: usize,
}

impl ProbeGeometry {
    pub const PRODUCTION: Self = Self {
        seq_buffer_bytes: SEQ_BUFFER_BYTES,
        seq_chunks: SEQ_CHUNKS_PER_SAMPLE,
        small_write_bytes: SMALL_WRITE_BYTES,
        small_writes: SMALL_WRITES_PER_SAMPLE,
    };

    pub const fn seq_bytes(self) -> u64 {
        (self.seq_buffer_bytes as u64) * (self.seq_chunks as u64)
    }

    pub const fn small_bytes(self) -> u64 {
        (self.small_write_bytes as u64) * (self.small_writes as u64)
    }

    pub const fn total_bytes(self) -> u64 {
        self.seq_bytes() + self.small_bytes()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkUseCase {
    Swap,
    Origin,
}

impl BenchmarkUseCase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Swap => "swap",
            Self::Origin => "origin",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WindowsVolumeCandidate {
    pub drive_letter: String,
    pub label: Option<String>,
    pub filesystem: String,
    pub drive_type: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub volume_id: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IneligibleVolumeNote {
    pub drive_letter: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WindowsSampleOutcome {
    pub round: usize,
    pub seq_elapsed_ns: u64,
    pub small_write_latencies_ns: Vec<u64>,
    pub bytes_written: u64,
    pub seq_bytes_written: u64,
    pub small_write_count: usize,
    pub expected_sha256: String,
    pub observed_sha256: String,
    pub cleanup_verified: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowsVolumeBenchmarkResult {
    pub drive_letter: String,
    pub label: Option<String>,
    pub filesystem: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub volume_id: String,
    pub sequential_mibs_per_sec: Vec<f64>,
    pub sequential_median_mibs_per_sec: f64,
    pub small_write_median_us: f64,
    pub small_write_p99_us: f64,
    pub total_bytes_written: u64,
    pub cleanup_verified: bool,
    pub samples: Vec<WindowsSampleOutcome>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowsBenchmarkReport {
    pub schema_version: u32,
    pub tx_id: String,
    pub plan_hash: String,
    pub use_case: BenchmarkUseCase,
    pub rounds: usize,
    pub sample_payload_bytes: u64,
    pub per_volume_write_cap_bytes: u64,
    pub required_free_bytes_per_volume: u64,
    pub writes_performed: bool,
    pub cleanup_verified: bool,
    pub status: String,
    pub recommended_drive: Option<String>,
    pub recommended_volume_id: Option<String>,
    pub rationale: String,
    pub eligible_volumes: Vec<WindowsVolumeCandidate>,
    pub skipped_volumes: Vec<IneligibleVolumeNote>,
    pub volumes: Vec<WindowsVolumeBenchmarkResult>,
}

pub fn filter_eligible_volumes(
    discovered: Vec<WindowsVolumeCandidate>,
    requested_drives: &[String],
) -> Result<(Vec<WindowsVolumeCandidate>, Vec<IneligibleVolumeNote>), String> {
    let filter_set: HashSet<String> = requested_drives
        .iter()
        .map(|s| s.trim_end_matches(':').to_ascii_uppercase())
        .filter(|s| !s.is_empty())
        .collect();

    let mut eligible = Vec::new();
    let mut skipped = Vec::new();
    let mut seen_ids = HashSet::new();

    for vol in discovered {
        let drive_upper = vol.drive_letter.to_ascii_uppercase();
        if !filter_set.is_empty() && !filter_set.contains(&drive_upper) {
            continue;
        }
        if !vol.drive_type.eq_ignore_ascii_case("Fixed") {
            skipped.push(IneligibleVolumeNote {
                drive_letter: drive_upper,
                reason: format!("unsupported drive type {}", vol.drive_type),
            });
            continue;
        }
        if !vol.filesystem.eq_ignore_ascii_case("NTFS") {
            skipped.push(IneligibleVolumeNote {
                drive_letter: drive_upper,
                reason: format!("filesystem {} is not NTFS", vol.filesystem),
            });
            continue;
        }
        if vol.read_only {
            skipped.push(IneligibleVolumeNote {
                drive_letter: drive_upper,
                reason: "volume is read-only".into(),
            });
            continue;
        }
        if vol.free_bytes < BENCHMARK_REQUIRED_FREE_BYTES {
            skipped.push(IneligibleVolumeNote {
                drive_letter: drive_upper,
                reason: format!(
                    "free capacity {:.2} GiB is below 10.09 GiB reserve floor",
                    (vol.free_bytes as f64) / (1024.0 * 1024.0 * 1024.0)
                ),
            });
            continue;
        }
        if vol.volume_id.trim().is_empty() || !seen_ids.insert(vol.volume_id.to_ascii_lowercase()) {
            skipped.push(IneligibleVolumeNote {
                drive_letter: drive_upper,
                reason: "missing or duplicate volume GUID identity".into(),
            });
            continue;
        }
        eligible.push(vol);
    }

    if !filter_set.is_empty() {
        for req in &filter_set {
            if !eligible
                .iter()
                .any(|v| v.drive_letter.eq_ignore_ascii_case(req))
            {
                let reason = skipped
                    .iter()
                    .find(|s| s.drive_letter.eq_ignore_ascii_case(req))
                    .map(|s| s.reason.clone())
                    .unwrap_or_else(|| "drive not present or not fixed".into());
                return Err(format!("requested volume {req}: is ineligible ({reason})"));
            }
        }
    }

    if eligible.is_empty() {
        return Err(
            "no eligible fixed NTFS volumes with >= 10 GiB + 96 MiB free space found".into(),
        );
    }
    if eligible.len() > MAX_BENCHMARK_VOLUMES {
        return Err(format!(
            "found {} eligible volumes (max {MAX_BENCHMARK_VOLUMES}); filter with --volumes",
            eligible.len()
        ));
    }

    eligible.sort_by(|a, b| a.drive_letter.cmp(&b.drive_letter));
    Ok((eligible, skipped))
}

pub fn compute_plan_hash(use_case: BenchmarkUseCase, volumes: &[WindowsVolumeCandidate]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(use_case.as_str().as_bytes());
    for vol in volumes {
        hasher.update(b"\0");
        hasher.update(vol.drive_letter.as_bytes());
        hasher.update(b":");
        hasher.update(vol.volume_id.as_bytes());
    }
    hex_digest(&hasher.finalize())
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[(b >> 4) as usize]));
        out.push(char::from(HEX[(b & 0x0f) as usize]));
    }
    out
}

fn fill_deterministic_page(sample_seed: u64, page_index: u32, buf: &mut [u8]) {
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

pub fn run_sample_in_dir(
    staging_dir: &Path,
    leaf_name: &str,
    sample_seed: u64,
    round: usize,
    geometry: ProbeGeometry,
) -> Result<WindowsSampleOutcome, String> {
    let probe_path = staging_dir.join(leaf_name);
    let result = (|| -> Result<WindowsSampleOutcome, String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&probe_path)
            .map_err(|err| format!("open probe file {}: {err}", probe_path.display()))?;

        let mut expected_hasher = Sha256::new();
        let mut seq_buf = vec![0u8; geometry.seq_buffer_bytes];
        let mut page_idx: u32 = 0;

        let seq_start = Instant::now();
        for _ in 0..geometry.seq_chunks {
            for chunk in seq_buf.chunks_mut(geometry.small_write_bytes) {
                fill_deterministic_page(sample_seed, page_idx, chunk);
                expected_hasher.update(chunk);
                page_idx = page_idx.wrapping_add(1);
            }
            file.write_all(&seq_buf)
                .map_err(|err| format!("sequential probe write failed: {err}"))?;
        }
        file.sync_all()
            .map_err(|err| format!("sequential probe flush failed: {err}"))?;
        let seq_elapsed_ns = seq_start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        if seq_elapsed_ns == 0 {
            return Err("unmeasurable zero-duration sequential benchmark timing".into());
        }

        // Pre-size tail region then execute distinct small durable writes using
        // a coprime permutation matching DT-21.
        let small_base_offset = geometry.seq_bytes();
        let zero_small = vec![0u8; geometry.small_write_bytes];
        for _ in 0..geometry.small_writes {
            file.write_all(&zero_small)
                .map_err(|err| format!("small-region prefill failed: {err}"))?;
        }

        let mut small_payloads = vec![vec![0u8; geometry.small_write_bytes]; geometry.small_writes];
        for (slot, payload) in small_payloads.iter_mut().enumerate() {
            fill_deterministic_page(sample_seed, page_idx + slot as u32, payload);
            expected_hasher.update(payload);
        }
        let expected_sha256 = hex_digest(&expected_hasher.finalize());

        let mut latencies_ns = Vec::with_capacity(geometry.small_writes);
        let stride = if geometry.small_writes == SMALL_WRITES_PER_SAMPLE {
            SMALL_WRITE_PERM_STRIDE
        } else {
            1
        };
        for step in 0..geometry.small_writes {
            let slot = (step * stride) % geometry.small_writes;
            let offset = small_base_offset + (slot as u64) * (geometry.small_write_bytes as u64);
            let t0 = Instant::now();
            file.seek(SeekFrom::Start(offset))
                .map_err(|err| format!("seek failed during small-write probe: {err}"))?;
            file.write_all(&small_payloads[slot])
                .map_err(|err| format!("small-write probe failed: {err}"))?;
            file.sync_all()
                .map_err(|err| format!("small-write flush failed: {err}"))?;
            let ns = t0.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            if ns == 0 {
                return Err("unmeasurable zero-duration small-write timing".into());
            }
            latencies_ns.push(ns);
        }

        // Verify complete file length and read-back SHA-256 checksum.
        file.seek(SeekFrom::Start(0))
            .map_err(|err| format!("seek before readback failed: {err}"))?;
        let mut read_hasher = Sha256::new();
        let mut read_buf = vec![0u8; geometry.seq_buffer_bytes];
        let mut total_read: u64 = 0;
        loop {
            let n = file
                .read(&mut read_buf)
                .map_err(|err| format!("readback verification failed: {err}"))?;
            if n == 0 {
                break;
            }
            if let Some(slice) = read_buf.get(..n) {
                read_hasher.update(slice);
            }
            total_read = total_read.saturating_add(n as u64);
        }
        let observed_sha256 = hex_digest(&read_hasher.finalize());
        let expected_bytes = geometry.total_bytes();
        if total_read != expected_bytes || observed_sha256 != expected_sha256 {
            return Err(format!(
                "readback integrity mismatch (read {total_read}/{expected_bytes} bytes)"
            ));
        }
        drop(file);
        Ok(WindowsSampleOutcome {
            round,
            seq_elapsed_ns,
            small_write_latencies_ns: latencies_ns,
            bytes_written: expected_bytes,
            seq_bytes_written: geometry.seq_bytes(),
            small_write_count: geometry.small_writes,
            expected_sha256,
            observed_sha256,
            cleanup_verified: false,
        })
    })();

    let remove_res = fs::remove_file(&probe_path);
    let cleanup_verified = remove_res.is_ok() && !probe_path.exists();
    let mut outcome = result?;
    if !cleanup_verified {
        return Err(format!(
            "manual_recovery_required: failed to verify removal of probe file {}",
            probe_path.display()
        ));
    }
    outcome.cleanup_verified = true;
    Ok(outcome)
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    }
}

fn nearest_rank_p99(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((sorted.len() as f64) * 0.99).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

pub fn recommend_windows_winner(
    use_case: BenchmarkUseCase,
    results: &[WindowsVolumeBenchmarkResult],
) -> (String, Option<String>, Option<String>, String) {
    if results.len() <= 1 {
        let drive = results.first().map(|r| r.drive_letter.clone());
        let id = results.first().map(|r| r.volume_id.clone());
        return (
            "single_volume".into(),
            drive,
            id,
            "single fixed NTFS volume measured".into(),
        );
    }

    let mut ranked: Vec<&WindowsVolumeBenchmarkResult> = results.iter().collect();
    ranked.sort_by(|a, b| match use_case {
        BenchmarkUseCase::Origin => b
            .sequential_median_mibs_per_sec
            .total_cmp(&a.sequential_median_mibs_per_sec)
            .then_with(|| a.small_write_median_us.total_cmp(&b.small_write_median_us)),
        BenchmarkUseCase::Swap => a
            .small_write_median_us
            .total_cmp(&b.small_write_median_us)
            .then_with(|| {
                b.sequential_median_mibs_per_sec
                    .total_cmp(&a.sequential_median_mibs_per_sec)
            }),
    });
    let Some(leader) = ranked.first().copied() else {
        return (
            "no_volumes".into(),
            None,
            None,
            "no volumes measured".into(),
        );
    };

    for peer in &ranked[1..] {
        let seq_tie = leader.sequential_median_mibs_per_sec
            < peer.sequential_median_mibs_per_sec * 1.10
            && matches!(use_case, BenchmarkUseCase::Origin);
        let lat_tie = peer.small_write_median_us < leader.small_write_median_us * 1.10
            && matches!(use_case, BenchmarkUseCase::Swap);
        let seq_conflict =
            leader.sequential_median_mibs_per_sec < peer.sequential_median_mibs_per_sec;
        let lat_conflict = leader.small_write_median_us > peer.small_write_median_us;
        if seq_tie || lat_tie || seq_conflict || lat_conflict {
            return (
                "tie".into(),
                None,
                None,
                format!(
                    "{}: and {}: are within 10% margin or split across sequential/small-write metrics",
                    leader.drive_letter, peer.drive_letter
                ),
            );
        }
    }

    (
        "recommended".into(),
        Some(leader.drive_letter.clone()),
        Some(leader.volume_id.clone()),
        format!(
            "{}: leads for {:?} use case ({:.1} MiB/s seq median, {:.1} us 4KiB flush median)",
            leader.drive_letter,
            use_case.as_str(),
            leader.sequential_median_mibs_per_sec,
            leader.small_write_median_us
        ),
    )
}

pub fn summarize_windows_results(
    vol: &WindowsVolumeCandidate,
    samples: Vec<WindowsSampleOutcome>,
) -> Result<WindowsVolumeBenchmarkResult, String> {
    if samples.len() != ROUNDS_PER_VOLUME {
        return Err(format!(
            "volume {}: produced {} samples instead of {ROUNDS_PER_VOLUME}",
            vol.drive_letter,
            samples.len()
        ));
    }
    let mut seq_speeds = Vec::with_capacity(samples.len());
    let mut small_us = Vec::with_capacity(samples.len() * SMALL_WRITES_PER_SAMPLE);
    let mut total_written = 0u64;
    for s in &samples {
        if !s.cleanup_verified || s.observed_sha256 != s.expected_sha256 {
            return Err(format!(
                "integrity/cleanup check failed on {}:",
                vol.drive_letter
            ));
        }
        let secs = (s.seq_elapsed_ns as f64) / 1_000_000_000.0;
        let mib = (s.seq_bytes_written as f64) / (1024.0 * 1024.0);
        seq_speeds.push(mib / secs);
        for &ns in &s.small_write_latencies_ns {
            small_us.push((ns as f64) / 1_000.0);
        }
        total_written = total_written.saturating_add(s.bytes_written);
    }

    Ok(WindowsVolumeBenchmarkResult {
        drive_letter: vol.drive_letter.clone(),
        label: vol.label.clone(),
        filesystem: vol.filesystem.clone(),
        total_bytes: vol.total_bytes,
        free_bytes: vol.free_bytes,
        volume_id: vol.volume_id.clone(),
        sequential_median_mibs_per_sec: median(&seq_speeds),
        sequential_mibs_per_sec: seq_speeds,
        small_write_median_us: median(&small_us),
        small_write_p99_us: nearest_rank_p99(&small_us),
        total_bytes_written: total_written,
        cleanup_verified: true,
        samples,
    })
}

pub fn render_windows_report_text(report: &WindowsBenchmarkReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "RamShared Windows volume benchmark ({}, tx={}, writes_performed={}):",
        report.use_case.as_str(),
        report.tx_id,
        report.writes_performed
    );
    if !report.writes_performed {
        for v in &report.eligible_volumes {
            let _ = writeln!(
                out,
                "  {}: ({}) {} — free {:.2} GiB of {:.2} GiB [eligible]",
                v.drive_letter,
                v.label.as_deref().unwrap_or("unlabeled"),
                v.volume_id,
                (v.free_bytes as f64) / (1024.0 * 1024.0 * 1024.0),
                (v.total_bytes as f64) / (1024.0 * 1024.0 * 1024.0),
            );
        }
    } else {
        for v in &report.volumes {
            let _ = writeln!(
                out,
                "  {}: ({}) — seq median {:.1} MiB/s | 4KiB flush median {:.1} us, p99 {:.1} us | written {} MiB | cleanup={}",
                v.drive_letter,
                v.label.as_deref().unwrap_or("unlabeled"),
                v.sequential_median_mibs_per_sec,
                v.small_write_median_us,
                v.small_write_p99_us,
                v.total_bytes_written / (1024 * 1024),
                v.cleanup_verified
            );
        }
    }
    for note in &report.skipped_volumes {
        let _ = writeln!(out, "  skipped {}: {}", note.drive_letter, note.reason);
    }
    let _ = writeln!(
        out,
        "Recommendation ({}): {} — {}",
        report.status,
        report
            .recommended_drive
            .as_deref()
            .map(|d| format!("{d}:"))
            .unwrap_or_else(|| "none".into()),
        report.rationale
    );
    out
}

#[cfg(windows)]
mod win;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BenchmarkVolumesArgs {
    pub confirm: bool,
    pub use_case: BenchmarkUseCase,
    pub volumes: Vec<String>,
    pub json: bool,
}

pub fn parse_benchmark_volumes_args(args: &[String]) -> Result<BenchmarkVolumesArgs, String> {
    let mut confirm: Option<bool> = None;
    let mut use_case: Option<BenchmarkUseCase> = None;
    let mut volumes: Option<Vec<String>> = None;
    let mut json = false;
    let mut idx = 0;

    while idx < args.len() {
        match args[idx].as_str() {
            "--plan" if confirm.is_none() => confirm = Some(false),
            "--confirm" if confirm.is_none() => confirm = Some(true),
            "--json" if !json => json = true,
            "--use-case" if use_case.is_none() && idx + 1 < args.len() => {
                idx += 1;
                use_case = Some(match args[idx].as_str() {
                    "swap" => BenchmarkUseCase::Swap,
                    "origin" => BenchmarkUseCase::Origin,
                    other => return Err(format!("invalid --use-case value: {other}")),
                });
            }
            "--volumes" if volumes.is_none() && idx + 1 < args.len() => {
                idx += 1;
                let mut list = Vec::new();
                for part in args[idx].split(',') {
                    let trimmed = part.trim().trim_end_matches(':');
                    if trimmed.len() != 1 || !trimmed.bytes().all(|b| b.is_ascii_alphabetic()) {
                        return Err(format!("invalid Windows drive selector: {part}"));
                    }
                    list.push(trimmed.to_ascii_uppercase());
                }
                if list.is_empty() {
                    return Err("--volumes requires at least one drive letter".into());
                }
                volumes = Some(list);
            }
            other => return Err(format!("unsupported benchmark-volumes argument: {other}")),
        }
        idx += 1;
    }

    Ok(BenchmarkVolumesArgs {
        confirm: confirm.unwrap_or(false),
        use_case: use_case.unwrap_or(BenchmarkUseCase::Swap),
        volumes: volumes.unwrap_or_default(),
        json,
    })
}

pub fn run_benchmark_volumes_command(
    confirm: bool,
    use_case: BenchmarkUseCase,
    volumes_filter: &[String],
    json: bool,
) -> i32 {
    #[cfg(not(windows))]
    {
        let _ = (confirm, use_case, volumes_filter, json);
        eprintln!("benchmark-volumes requires Windows host execution");
        2
    }
    #[cfg(windows)]
    {
        match win::execute_windows_benchmark(confirm, use_case, volumes_filter) {
            Ok(report) => {
                if json {
                    match serde_json::to_string(&report) {
                        Ok(s) => println!("{s}"),
                        Err(e) => {
                            eprintln!("cannot serialize benchmark report: {e}");
                            return 1;
                        }
                    }
                } else {
                    print!("{}", render_windows_report_text(&report));
                }
                0
            }
            Err(err) => {
                eprintln!("benchmark-volumes failed: {err}");
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct LocalTempDir(PathBuf);
    impl LocalTempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(1);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ramshared-winbench-{tag}-{}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for LocalTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn vol(
        letter: &str,
        fs_name: &str,
        drive_type: &str,
        free_bytes: u64,
        id: &str,
    ) -> WindowsVolumeCandidate {
        WindowsVolumeCandidate {
            drive_letter: letter.into(),
            label: Some(format!("DISK_{letter}")),
            filesystem: fs_name.into(),
            drive_type: drive_type.into(),
            total_bytes: free_bytes.saturating_mul(2),
            free_bytes,
            volume_id: id.into(),
            read_only: false,
        }
    }

    #[test]
    fn filter_enforces_ntfs_fixed_floor_and_unique_volume_guids() {
        let min_ok = BENCHMARK_REQUIRED_FREE_BYTES + 1024;
        let candidates = vec![
            vol("C", "NTFS", "Fixed", min_ok, r"\\?\Volume{1111}\"),
            vol("D", "FAT32", "Fixed", min_ok, r"\\?\Volume{2222}\"),
            vol("E", "NTFS", "Removable", min_ok, r"\\?\Volume{3333}\"),
            vol(
                "F",
                "NTFS",
                "Fixed",
                BENCHMARK_REQUIRED_FREE_BYTES - 1,
                r"\\?\Volume{4444}\",
            ),
            vol("I", "NTFS", "Fixed", min_ok * 2, r"\\?\Volume{5555}\"),
        ];
        let (eligible, skipped) = filter_eligible_volumes(candidates, &[]).unwrap();
        assert_eq!(eligible.len(), 2);
        assert_eq!(eligible[0].drive_letter, "C");
        assert_eq!(eligible[1].drive_letter, "I");
        assert_eq!(skipped.len(), 3);
    }

    #[test]
    fn run_sample_in_dir_verifies_sha256_timings_and_cleanup() {
        let tmp = LocalTempDir::new("sample");
        let geom = ProbeGeometry {
            seq_buffer_bytes: 4096,
            seq_chunks: 4,
            small_write_bytes: 4096,
            small_writes: 8,
        };
        let mut outcomes = Vec::new();
        for round in 0..ROUNDS_PER_VOLUME {
            let s = run_sample_in_dir(
                &tmp.0,
                &format!("p{round}.bin"),
                42 + round as u64,
                round,
                geom,
            )
            .expect("sample succeeds");
            assert_eq!(s.expected_sha256, s.observed_sha256);
            assert!(s.cleanup_verified);
            assert_eq!(s.bytes_written, geom.total_bytes());
            assert!(s.seq_elapsed_ns > 0);
            assert!(!tmp.0.join(format!("p{round}.bin")).exists());
            outcomes.push(s);
        }
        let candidate = vol(
            "I",
            "NTFS",
            "Fixed",
            BENCHMARK_REQUIRED_FREE_BYTES + 4096,
            r"\\?\Volume{5555}\",
        );
        let summary = summarize_windows_results(&candidate, outcomes).unwrap();
        assert!(summary.sequential_median_mibs_per_sec > 0.0);
        assert!(summary.small_write_median_us > 0.0);
    }

    #[test]
    fn recommendation_distinguishes_clear_leader_from_tie() {
        let fast = WindowsVolumeBenchmarkResult {
            drive_letter: "I".into(),
            label: Some("NVME".into()),
            filesystem: "NTFS".into(),
            total_bytes: 100,
            free_bytes: 80,
            volume_id: "id-i".into(),
            sequential_mibs_per_sec: vec![3000.0, 3100.0, 3050.0],
            sequential_median_mibs_per_sec: 3050.0,
            small_write_median_us: 120.0,
            small_write_p99_us: 250.0,
            total_bytes_written: PER_VOLUME_WRITE_CAP_BYTES,
            cleanup_verified: true,
            samples: Vec::new(),
        };
        let slow = WindowsVolumeBenchmarkResult {
            drive_letter: "E".into(),
            label: Some("SATA".into()),
            filesystem: "NTFS".into(),
            total_bytes: 100,
            free_bytes: 80,
            volume_id: "id-e".into(),
            sequential_mibs_per_sec: vec![500.0, 510.0, 505.0],
            sequential_median_mibs_per_sec: 505.0,
            small_write_median_us: 800.0,
            small_write_p99_us: 1500.0,
            total_bytes_written: PER_VOLUME_WRITE_CAP_BYTES,
            cleanup_verified: true,
            samples: Vec::new(),
        };
        let (status, drive, id, _) =
            recommend_windows_winner(BenchmarkUseCase::Swap, &[slow.clone(), fast.clone()]);
        assert_eq!(status, "recommended");
        assert_eq!(drive.as_deref(), Some("I"));
        assert_eq!(id.as_deref(), Some("id-i"));

        let near_tie = WindowsVolumeBenchmarkResult {
            drive_letter: "C".into(),
            volume_id: "id-c".into(),
            sequential_median_mibs_per_sec: 2950.0,
            small_write_median_us: 125.0,
            ..fast.clone()
        };
        let (tie_status, tie_drive, _, _) =
            recommend_windows_winner(BenchmarkUseCase::Origin, &[fast, near_tie]);
        assert_eq!(tie_status, "tie");
        assert!(tie_drive.is_none());
    }

    #[test]
    fn cli_parser_enforces_mode_use_case_and_drive_letters() {
        let parsed = parse_benchmark_volumes_args(&[
            "--confirm".into(),
            "--use-case".into(),
            "origin".into(),
            "--volumes".into(),
            "c:,e,I".into(),
            "--json".into(),
        ])
        .unwrap();
        assert!(parsed.confirm);
        assert_eq!(parsed.use_case, BenchmarkUseCase::Origin);
        assert_eq!(parsed.volumes, vec!["C", "E", "I"]);
        assert!(parsed.json);

        assert!(parse_benchmark_volumes_args(&["--plan".into(), "--confirm".into()]).is_err());
        assert!(parse_benchmark_volumes_args(&["--volumes".into(), "C:\\".into()]).is_err());
        assert!(parse_benchmark_volumes_args(&["--bogus".into()]).is_err());
    }

    #[test]
    fn report_rendering_covers_plan_and_measured_text_and_filter_errors() {
        let min_ok = BENCHMARK_REQUIRED_FREE_BYTES + 1024;
        let c = vol("C", "NTFS", "Fixed", min_ok, r"\\?\Volume{1111}\");
        let i = vol("I", "NTFS", "Fixed", min_ok * 2, r"\\?\Volume{5555}\");
        assert!(filter_eligible_volumes(vec![c.clone()], &["Z".into()]).is_err());
        let h1 = compute_plan_hash(BenchmarkUseCase::Swap, &[c.clone(), i.clone()]);
        assert_eq!(h1.len(), 64);

        let plan_rep = WindowsBenchmarkReport {
            schema_version: 1,
            tx_id: "tx-plan".into(),
            plan_hash: h1,
            use_case: BenchmarkUseCase::Swap,
            rounds: ROUNDS_PER_VOLUME,
            sample_payload_bytes: SAMPLE_PAYLOAD_BYTES,
            per_volume_write_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
            required_free_bytes_per_volume: BENCHMARK_REQUIRED_FREE_BYTES,
            writes_performed: false,
            cleanup_verified: true,
            status: "plan_only".into(),
            recommended_drive: None,
            recommended_volume_id: None,
            rationale: "preview".into(),
            eligible_volumes: vec![c, i],
            skipped_volumes: vec![IneligibleVolumeNote {
                drive_letter: "D".into(),
                reason: "not NTFS".into(),
            }],
            volumes: Vec::new(),
        };
        let txt = render_windows_report_text(&plan_rep);
        assert!(txt.contains("writes_performed=false"));
        assert!(txt.contains("[eligible]"));
        assert!(txt.contains("skipped D: not NTFS"));

        // Exercise verified write report formatting, single-volume & Origin winner,
        // read-only/duplicate-ID/overflow filters, and CLI parser error branches.
        let mut ro = vol("R", "NTFS", "Fixed", min_ok, r"\\?\Volume{6666}\");
        ro.read_only = true;
        let dup = vol("X", "NTFS", "Fixed", min_ok, r"\\?\Volume{1111}\");
        let (_, extra_skipped) = filter_eligible_volumes(
            vec![
                vol("C", "NTFS", "Fixed", min_ok, r"\\?\Volume{1111}\"),
                ro,
                dup,
            ],
            &[],
        )
        .unwrap();
        assert_eq!(extra_skipped.len(), 2);
        assert!(filter_eligible_volumes(vec![], &[]).is_err());
        let too_many: Vec<_> = (b'A'..=b'I')
            .enumerate()
            .map(|(n, ch)| {
                vol(
                    &char::from(ch).to_string(),
                    "NTFS",
                    "Fixed",
                    min_ok,
                    &format!("id-{n}"),
                )
            })
            .collect();
        assert!(filter_eligible_volumes(too_many, &[]).is_err());

        let v_fast = WindowsVolumeBenchmarkResult {
            drive_letter: "I".into(),
            label: Some("NVME".into()),
            filesystem: "NTFS".into(),
            total_bytes: 100,
            free_bytes: 80,
            volume_id: "id-i".into(),
            sequential_mibs_per_sec: vec![3000.0, 3100.0, 3050.0],
            sequential_median_mibs_per_sec: 3050.0,
            small_write_median_us: 120.0,
            small_write_p99_us: 250.0,
            total_bytes_written: PER_VOLUME_WRITE_CAP_BYTES,
            cleanup_verified: true,
            samples: Vec::new(),
        };
        let v_slow = WindowsVolumeBenchmarkResult {
            drive_letter: "E".into(),
            label: Some("SATA".into()),
            volume_id: "id-e".into(),
            sequential_median_mibs_per_sec: 400.0,
            small_write_median_us: 900.0,
            ..v_fast.clone()
        };
        let (s1, d1, _, _) =
            recommend_windows_winner(BenchmarkUseCase::Swap, std::slice::from_ref(&v_fast));
        assert_eq!(s1, "single_volume");
        assert_eq!(d1.as_deref(), Some("I"));
        let (so, do_drive, _, _) =
            recommend_windows_winner(BenchmarkUseCase::Origin, &[v_slow.clone(), v_fast.clone()]);
        assert_eq!(so, "recommended");
        assert_eq!(do_drive.as_deref(), Some("I"));

        let measured_rep = WindowsBenchmarkReport {
            writes_performed: true,
            status: "recommended".into(),
            recommended_drive: Some("I".into()),
            volumes: vec![v_fast, v_slow],
            ..plan_rep
        };
        let measured_txt = render_windows_report_text(&measured_rep);
        assert!(measured_txt.contains("writes_performed=true"));
        assert!(measured_txt.contains("4KiB flush median"));
        assert!(measured_txt.contains("I: (NVME)"));

        let p =
            parse_benchmark_volumes_args(&["--plan".into(), "--use-case".into(), "swap".into()])
                .unwrap();
        assert!(!p.confirm);
        assert!(parse_benchmark_volumes_args(&["--use-case".into(), "invalid".into()]).is_err());
        assert!(parse_benchmark_volumes_args(&["--volumes".into(), "".into()]).is_err());
        assert_eq!(
            run_benchmark_volumes_command(false, BenchmarkUseCase::Swap, &["!".into()], true),
            if cfg!(windows) { 1 } else { 2 }
        );
        #[cfg(not(windows))]
        assert_eq!(
            run_benchmark_volumes_command(false, BenchmarkUseCase::Swap, &[], false),
            2
        );
    }
}
