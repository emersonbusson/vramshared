// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/storage_benchmark/win.rs
//! Windows-specific fixed NTFS volume enumeration, Win32 GUID verification,
//! and bounded benchmark orchestration (`DT-27`).

use super::*;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW,
};
use windows_sys::Win32::System::SystemServices::FILE_READ_ONLY_VOLUME;

const DRIVE_FIXED: u32 = 3;

fn wide_null(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn utf16_cstr(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[allow(unsafe_code)]
fn volume_guid_for_mount(mount_with_slash: &str) -> Option<String> {
    let wide = wide_null(mount_with_slash);
    let mut guid_buf = [0u16; 128];
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(
            wide.as_ptr(),
            guid_buf.as_mut_ptr(),
            guid_buf.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let s = utf16_cstr(&guid_buf);
    if s.is_empty() { None } else { Some(s) }
}

#[allow(unsafe_code)]
pub(super) fn discover_fixed_volumes() -> Vec<WindowsVolumeCandidate> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for bit in 0..26u8 {
        if (mask & (1u32 << bit)) == 0 {
            continue;
        }
        let letter = char::from(b'A' + bit);
        let root = format!("{letter}:\\");
        let root_w = wide_null(&root);
        let dtype = unsafe { GetDriveTypeW(root_w.as_ptr()) };
        if dtype != DRIVE_FIXED {
            continue;
        }

        let mut label_buf = [0u16; 260];
        let mut fs_buf = [0u16; 64];
        let mut flags: u32 = 0;
        let info_ok = unsafe {
            GetVolumeInformationW(
                root_w.as_ptr(),
                label_buf.as_mut_ptr(),
                label_buf.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                fs_buf.as_mut_ptr(),
                fs_buf.len() as u32,
            )
        };
        if info_ok == 0 {
            continue;
        }
        let label = utf16_cstr(&label_buf);
        let filesystem = utf16_cstr(&fs_buf);
        let read_only = (flags & FILE_READ_ONLY_VOLUME) != 0;

        let mut free_avail: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        let space_ok = unsafe {
            GetDiskFreeSpaceExW(
                root_w.as_ptr(),
                &mut free_avail,
                &mut total,
                &mut total_free,
            )
        };
        if space_ok == 0 {
            continue;
        }
        let volume_id = volume_guid_for_mount(&root).unwrap_or_default();
        out.push(WindowsVolumeCandidate {
            drive_letter: letter.to_string(),
            label: if label.is_empty() { None } else { Some(label) },
            filesystem,
            drive_type: "Fixed".into(),
            total_bytes: total,
            free_bytes: free_avail,
            volume_id,
            read_only,
        });
    }
    out
}

fn choose_verified_staging_dir(
    vol: &WindowsVolumeCandidate,
    tx_short: &str,
) -> Result<PathBuf, String> {
    let sys_drive = std::env::var("SystemDrive")
        .ok()
        .map(|d| d.trim_end_matches(':').to_ascii_uppercase());
    let candidates: Vec<PathBuf> = if sys_drive.as_deref() == Some(vol.drive_letter.as_str()) {
        let mut dirs = Vec::new();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local)
                    .join("Temp")
                    .join(format!("RamSharedBench.{tx_short}")),
            );
        }
        if let Ok(pd) = std::env::var("ProgramData") {
            dirs.push(
                PathBuf::from(pd)
                    .join("RamShared")
                    .join(format!("BenchTmp.{tx_short}")),
            );
        }
        dirs.push(
            PathBuf::from(format!("{}:\\", vol.drive_letter))
                .join(format!(".ramshared-bench-{tx_short}")),
        );
        dirs
    } else {
        vec![
            PathBuf::from(format!("{}:\\", vol.drive_letter))
                .join(format!(".ramshared-bench-{tx_short}")),
        ]
    };

    for dir in candidates {
        if fs::create_dir_all(&dir).is_err() {
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(&dir)
            && meta.file_type().is_symlink()
        {
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        let root_mount = format!("{}:\\", vol.drive_letter);
        let matches_root =
            volume_guid_for_mount(&root_mount).as_deref() == Some(vol.volume_id.as_str());
        // Check ancestor components do not reparse off the target volume.
        let mut cur: Option<&Path> = Some(dir.as_path());
        let mut any_symlink = false;
        while let Some(ancestor) = cur {
            if let Ok(meta) = fs::symlink_metadata(ancestor)
                && meta.file_type().is_symlink()
            {
                any_symlink = true;
                break;
            }
            cur = ancestor.parent().filter(|p| !p.as_os_str().is_empty());
        }
        if matches_root && !any_symlink {
            return Ok(dir);
        }
        let _ = fs::remove_dir_all(&dir);
    }
    Err(format!(
        "cannot establish verified non-symlink staging directory on {}:",
        vol.drive_letter
    ))
}

pub(super) fn execute_windows_benchmark(
    confirm: bool,
    use_case: BenchmarkUseCase,
    volumes_filter: &[String],
) -> Result<WindowsBenchmarkReport, String> {
    let discovered = discover_fixed_volumes();
    let (eligible, skipped) = filter_eligible_volumes(discovered, volumes_filter)?;
    let plan_hash = compute_plan_hash(use_case, &eligible);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0);
    let tx_id = format!("win-{now_ms}-{}", std::process::id());
    let tx_short = tx_id.get(..14).unwrap_or(&tx_id);

    if !confirm {
        return Ok(WindowsBenchmarkReport {
            schema_version: 1,
            tx_id,
            plan_hash,
            use_case,
            rounds: ROUNDS_PER_VOLUME,
            sample_payload_bytes: SAMPLE_PAYLOAD_BYTES,
            per_volume_write_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
            required_free_bytes_per_volume: BENCHMARK_REQUIRED_FREE_BYTES,
            writes_performed: false,
            cleanup_verified: true,
            status: "plan_only".into(),
            recommended_drive: None,
            recommended_volume_id: None,
            rationale: "Read-only preview; pass --confirm to measure volume speeds.".into(),
            eligible_volumes: eligible,
            skipped_volumes: skipped,
            volumes: Vec::new(),
        });
    }

    let mut samples_by_vol: Vec<Vec<WindowsSampleOutcome>> = vec![Vec::new(); eligible.len()];
    let mut staging_dirs: Vec<PathBuf> = Vec::with_capacity(eligible.len());
    for vol in &eligible {
        staging_dirs.push(choose_verified_staging_dir(vol, tx_short)?);
    }

    let run_res = (|| -> Result<(), String> {
        for round in 0..ROUNDS_PER_VOLUME {
            for offset in 0..eligible.len() {
                let vol_idx = (round + offset) % eligible.len();
                let seed = now_ms
                    ^ ((round as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15))
                    ^ ((vol_idx as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9));
                let leaf = format!("bench-r{round}-v{vol_idx}.bin");
                let sample = run_sample_in_dir(
                    &staging_dirs[vol_idx],
                    &leaf,
                    seed,
                    round,
                    ProbeGeometry::PRODUCTION,
                )?;
                samples_by_vol[vol_idx].push(sample);
            }
        }
        Ok(())
    })();

    for dir in &staging_dirs {
        let _ = fs::remove_dir_all(dir);
    }
    run_res?;

    let mut results = Vec::with_capacity(eligible.len());
    for (vol, samples) in eligible.iter().zip(samples_by_vol) {
        results.push(summarize_windows_results(vol, samples)?);
    }
    let (status, recommended_drive, recommended_volume_id, rationale) =
        recommend_windows_winner(use_case, &results);

    Ok(WindowsBenchmarkReport {
        schema_version: 1,
        tx_id,
        plan_hash,
        use_case,
        rounds: ROUNDS_PER_VOLUME,
        sample_payload_bytes: SAMPLE_PAYLOAD_BYTES,
        per_volume_write_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
        required_free_bytes_per_volume: BENCHMARK_REQUIRED_FREE_BYTES,
        writes_performed: true,
        cleanup_verified: results.iter().all(|r| r.cleanup_verified),
        status,
        recommended_drive,
        recommended_volume_id,
        rationale,
        eligible_volumes: eligible,
        skipped_volumes: skipped,
        volumes: results,
    })
}
