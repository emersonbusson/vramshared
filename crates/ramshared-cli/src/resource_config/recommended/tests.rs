// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/recommended/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::symlink;

use super::*;

#[test]
fn recommend_zram_scales_with_guest_ram_and_preset() {
    assert_eq!(recommend_zram_bytes(None, Preset::Recommended), None);
    assert_eq!(
        recommend_zram_bytes(Some(128 * MIB), Preset::Recommended),
        None
    );
    assert_eq!(
        recommend_zram_bytes(Some(16 * GIB), Preset::Recommended),
        Some(4 * GIB)
    );
    assert_eq!(
        recommend_zram_bytes(Some(16 * GIB), Preset::Conservative),
        Some(2 * GIB)
    );
    assert_eq!(
        recommend_zram_bytes(Some(64 * GIB), Preset::Recommended),
        Some(8 * GIB)
    );
    assert_eq!(
        recommend_zram_bytes(Some(64 * GIB), Preset::Conservative),
        Some(4 * GIB)
    );
    assert_eq!(
        recommend_zram_bytes(Some(1024 * MIB), Preset::Recommended),
        Some(256 * MIB)
    );
}

#[test]
fn recommend_storage_pair_enforces_reserve_floor_and_honors_preferred_volume() {
    // No volumes or all below 10 GiB reserve -> None.
    assert_eq!(
        recommend_storage_pair(Some(4 * GIB), &[], None, Preset::Recommended),
        StoragePairRecommendation::default()
    );
    assert_eq!(
        recommend_storage_pair(Some(4 * GIB), &[10 * GIB], None, Preset::Recommended),
        StoragePairRecommendation::default()
    );

    // Single volume with 30 GiB free -> 20 GiB usable -> 4 GiB origin + 4 GiB fallback.
    let s1 = recommend_storage_pair(Some(4 * GIB), &[30 * GIB], None, Preset::Recommended);
    assert_eq!(
        s1.origin,
        Some(StorageChoice {
            volume_index: 0,
            bytes: 4 * GIB
        })
    );
    assert_eq!(
        s1.fallback,
        Some(StorageChoice {
            volume_index: 0,
            bytes: 4 * GIB
        })
    );

    // Tight single volume (14 GiB free -> 4 GiB usable): splits cleanly without exceeding reserve.
    let s2 = recommend_storage_pair(Some(4 * GIB), &[14 * GIB], None, Preset::Recommended);
    assert_eq!(
        s2.origin,
        Some(StorageChoice {
            volume_index: 0,
            bytes: 2 * GIB
        })
    );
    assert_eq!(
        s2.fallback,
        Some(StorageChoice {
            volume_index: 0,
            bytes: 2 * GIB
        })
    );

    // Preferred volume index (e.g. benchmark winner) wins over larger volume.
    let s3 = recommend_storage_pair(
        Some(4 * GIB),
        &[100 * GIB, 25 * GIB],
        Some(1),
        Preset::Conservative,
    );
    assert_eq!(
        s3.origin,
        Some(StorageChoice {
            volume_index: 1,
            bytes: 2 * GIB
        })
    );
    assert_eq!(
        s3.fallback,
        Some(StorageChoice {
            volume_index: 1,
            bytes: 2 * GIB
        })
    );

    // Multi-volume spillover: if preferred volume only fits origin, fallback moves to second volume.
    let s4 = recommend_storage_pair(
        Some(4 * GIB),
        &[14 * GIB, 50 * GIB],
        Some(0),
        Preset::Recommended,
    );
    assert_eq!(
        s4.origin,
        Some(StorageChoice {
            volume_index: 0,
            bytes: 4 * GIB
        })
    );
    assert_eq!(
        s4.fallback,
        Some(StorageChoice {
            volume_index: 1,
            bytes: 4 * GIB
        })
    );
}

#[test]
fn recommend_three_tiers_populates_all_tiers_from_snapshot_and_renders_summary() {
    let mut snapshot = crate::resource_config::tests::fixture_snapshot();
    let now = crate::resource_config::unix_millis();
    snapshot.gpu_inventory = gpu_inventory::parse_inventory("GPU-rtx2060, 6144, 6144").unwrap();
    snapshot.gpu_observed_unix_ms = now;

    let rec = recommend_three_tiers(&snapshot, Preset::Recommended, now);
    assert_eq!(rec.zram_bytes, Some(4 * GIB));
    assert_eq!(rec.gpus.len(), 1);
    assert_eq!(rec.gpus[0].cap_bytes, 3840 * MIB);
    assert_eq!(rec.origin_bytes, Some(4 * GIB));
    assert_eq!(rec.fallback_bytes, Some(4 * GIB));
    assert_eq!(rec.cascade_zram_mib, 4096);
    assert_eq!(rec.cascade_vram_mib, 4096);
    assert_eq!(rec.min_vram_headroom_mib, 2048);

    let summary = render_recommendation_summary(&rec);
    assert!(summary.contains("Tier 1 (ZRAM)"));
    assert!(summary.contains("GPU-rtx2060"));
    assert!(summary.contains("Tier 2 (SSD origin)"));
    assert!(summary.contains("Tier 3 (Fallback)"));

    // No-GPU fallback summary also renders cleanly.
    snapshot.gpu_inventory.clear();
    let no_gpu = recommend_three_tiers(&snapshot, Preset::Conservative, now);
    assert!(render_recommendation_summary(&no_gpu).contains("no GPU detected"));

    // JSON inventory output includes recommended_three_tiers matching text summary.
    let json_text = crate::resource_config::render_json(&snapshot).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_text).unwrap();
    assert_eq!(
        parsed["recommended_three_tiers"]["cascade_zram_mib"],
        serde_json::json!(4096)
    );
    assert_eq!(
        parsed["recommended_three_tiers"]["cascade_vram_mib"],
        serde_json::json!(4096)
    );
    assert_eq!(
        parsed["recommended_three_tiers"]["min_vram_headroom_mib"],
        serde_json::json!(2048)
    );
}

#[test]
fn cascade_conf_atomic_writer_validates_preserves_existing_and_refuses_symlinks() {
    let dir =
        std::env::temp_dir().join(format!("ramshared-recommended-conf-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("cascade.conf");

    let snapshot = crate::resource_config::tests::fixture_snapshot();
    let now = crate::resource_config::unix_millis();

    // Absent -> writes hardware-detected 3-tier config.
    let first = ensure_recommended_cascade_conf_at(&conf, &snapshot, now).unwrap();
    assert!(first.is_some());
    let written = fs::read_to_string(&conf).unwrap();
    assert!(written.contains("ZRAM_MIB=4096"));
    assert!(written.contains("VRAM_MIB=4096"));
    assert!(written.contains("MIN_VRAM_HEADROOM_MIB=2048"));

    // Existing file -> preserved untouched when overwrite=false.
    fs::write(
        &conf,
        "VRAM_MIB=2048\nZRAM_MIB=1024\nMIN_VRAM_HEADROOM_MIB=2048\n",
    )
    .unwrap();
    let second = ensure_recommended_cascade_conf_at(&conf, &snapshot, now).unwrap();
    assert!(second.is_none());
    assert!(fs::read_to_string(&conf).unwrap().contains("ZRAM_MIB=1024"));

    // Explicit overwrite=true updates atomically.
    let updated = render_cascade_conf(4096, 4096, 2048, "operator apply").unwrap();
    assert_eq!(
        write_cascade_conf_atomic(&conf, &updated, true).unwrap(),
        CascadeConfWrite::Written
    );
    assert!(fs::read_to_string(&conf).unwrap().contains("ZRAM_MIB=4096"));

    // Invalid cascade.conf content is rejected before touching disk.
    assert!(render_cascade_conf(0, 4096, 2048, "bad").is_err());
    assert!(write_cascade_conf_atomic(&conf, "VRAM_MIB=notanumber\n", true).is_err());

    // Symlink target is refused fail-closed.
    let link = dir.join("link.conf");
    symlink(&conf, &link).unwrap();
    assert!(write_cascade_conf_atomic(&link, &updated, true).is_err());
    assert!(ensure_recommended_cascade_conf_at(&link, &snapshot, now).is_err());

    let _ = fs::remove_dir_all(&dir);
}
