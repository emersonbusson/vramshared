// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/assistant/tests.rs
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::resource_config::kernel_capabilities::{
    AdaptiveModePlan, CapabilityState, KernelCapabilities, KernelGap, MglruState, UblkState,
};

fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ramshared-assistant-{tag}-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sample_caps(optimal: bool) -> KernelCapabilities {
    if optimal {
        KernelCapabilities {
            platform: RuntimePlatform::Wsl2,
            kernel_release: Some("6.18.64.1-microsoft-standard-WSL2+".into()),
            zram_writeback: CapabilityState::Supported,
            zram_actime: CapabilityState::Supported,
            zram_algorithms: vec!["lz4".into(), "lzo-rle".into(), "zstd".into()],
            zram_active_algorithm: Some("lz4".into()),
            zram_multi_comp: CapabilityState::Supported,
            mglru: MglruState::Enabled(7),
            psi: CapabilityState::Supported,
            ublk: UblkState::PresentLabGatedWsl2,
            nbd_present: true,
            vmbus_v3_active: CapabilityState::Supported,
            effective_transport: "nbd",
            kernel_gaps: Vec::new(),
            adaptive_plan: AdaptiveModePlan {
                primary_zram_algo: "lz4".into(),
                secondary_recomp_algo: Some("zstd".into()),
                spill_aging_mode: "native_actime",
                tier2_transport: "nbd",
                transport_note: "NBD active (ublk module present in kernel but lab-gated on WSL2)",
            },
        }
    } else {
        KernelCapabilities {
            platform: RuntimePlatform::Wsl2,
            kernel_release: Some("6.18.64.1-microsoft-standard-WSL2+".into()),
            zram_writeback: CapabilityState::Supported,
            zram_actime: CapabilityState::Unsupported,
            zram_algorithms: vec!["lzo-rle".into(), "lzo".into()],
            zram_active_algorithm: Some("lzo-rle".into()),
            zram_multi_comp: CapabilityState::Unsupported,
            mglru: MglruState::Absent,
            psi: CapabilityState::Supported,
            ublk: UblkState::PresentLabGatedWsl2,
            nbd_present: true,
            vmbus_v3_active: CapabilityState::Supported,
            effective_transport: "nbd",
            kernel_gaps: vec![
                KernelGap {
                    config_symbol: "CONFIG_ZRAM_TRACK_ENTRY_ACTIME",
                    reason: "enables single-step cold-page aging",
                },
                KernelGap {
                    config_symbol: "CONFIG_LRU_GEN",
                    reason: "improves reclaim accuracy",
                },
            ],
            adaptive_plan: AdaptiveModePlan {
                primary_zram_algo: "lzo-rle".into(),
                secondary_recomp_algo: None,
                spill_aging_mode: "two_phase_adaptive",
                tier2_transport: "nbd",
                transport_note: "NBD active (ublk module present in kernel but lab-gated on WSL2)",
            },
        }
    }
}

#[test]
fn checklist_classifies_optimal_partial_and_degraded_hosts() {
    let optimal_obs = AssistantObservation {
        installed_release: Some("v0.14.1-1448-gabcdef01".into()),
        cascade_conf_present: true,
        cascade_conf_summary: Some(
            "ZRAM=4096 MiB · Origin/VRAM=4096 MiB · Host reserve=2048 MiB".into(),
        ),
        gpu_count: 1,
        gpu_cache_active: Some(true),
        control_plane_state: Some("vsock_leased".into()),
        cascade_service_enabled: Some(true),
        auto_update_timer_enabled: Some(true),
        auto_update_conf: AutoUpdateFileState {
            present: true,
            enabled: Some(true),
            lower_sink: Some("/mnt/c/ProgramData/RamShared/nbd-lower-sink".into()),
            bundle_root: Some("/opt/ramshared/bundles".into()),
            release_keyring: Some("/etc/ramshared/release-keyring.gpg".into()),
        },
        capabilities: sample_caps(true),
    };
    let optimal = build_checklist(&optimal_obs);
    assert_eq!(optimal.items.len(), 6);
    assert_eq!(optimal.ok_count, 6);
    assert_eq!(optimal.pending_count, 0);

    let degraded_obs = AssistantObservation {
        installed_release: None,
        cascade_conf_present: false,
        cascade_conf_summary: None,
        gpu_count: 1,
        gpu_cache_active: Some(false),
        control_plane_state: Some("disconnected".into()),
        cascade_service_enabled: Some(false),
        auto_update_timer_enabled: Some(false),
        auto_update_conf: AutoUpdateFileState::default(),
        capabilities: sample_caps(false),
    };
    let degraded = build_checklist(&degraded_obs);
    assert_eq!(degraded.ok_count, 1); // NBD transport still present
    assert_eq!(degraded.pending_count, 5);
    assert_eq!(degraded.items[0].status, ItemStatus::Pending);
    assert_eq!(degraded.items[1].status, ItemStatus::Pending);
    assert_eq!(degraded.items[2].status, ItemStatus::Optional);
    assert_eq!(degraded.items[4].status, ItemStatus::Attention);
    assert_eq!(degraded.items[5].status, ItemStatus::Pending);

    // No-GPU + no-writeback path
    let mut no_wb = sample_caps(false);
    no_wb.zram_writeback = CapabilityState::Unsupported;
    no_wb.nbd_present = false;
    no_wb.ublk = UblkState::Absent;
    let no_gpu_obs = AssistantObservation {
        gpu_count: 0,
        capabilities: no_wb,
        ..degraded_obs
    };
    let no_gpu = build_checklist(&no_gpu_obs);
    assert_eq!(no_gpu.items[2].status, ItemStatus::Attention);
    assert_eq!(no_gpu.items[3].status, ItemStatus::Attention);
    assert_eq!(no_gpu.items[4].status, ItemStatus::Optional);

    // Unobserved kernel surface reports Unavailable honestly.
    let mut unobserved = sample_caps(false);
    unobserved.kernel_release = None;
    unobserved.zram_writeback = CapabilityState::Unknown;
    let unobserved_obs = AssistantObservation {
        capabilities: unobserved,
        ..no_gpu_obs
    };
    let unobserved_list = build_checklist(&unobserved_obs);
    assert_eq!(unobserved_list.items[2].status, ItemStatus::Unavailable);
    assert_eq!(unobserved_list.items[2].status.badge(), "[UNAVAILABLE]");
}

#[test]
fn auto_update_write_preserves_keys_is_idempotent_and_refuses_symlinks() {
    let dir = temp_dir("autoupdate");
    let conf = dir.join("auto-update.conf");
    std::fs::write(
        &conf,
        "enabled=0\nlower_sink=/mnt/c/ProgramData/RamShared/nbd-lower-sink\nbundle_root=/opt/ramshared/bundles\nrelease_keyring=/etc/ramshared/release-keyring.gpg\n",
    )
    .unwrap();

    let first = write_auto_update_conf_atomic(&conf, true).unwrap();
    let content_first = std::fs::read_to_string(&conf).unwrap();
    let second = write_auto_update_conf_atomic(&conf, true).unwrap();
    let content_second = std::fs::read_to_string(&conf).unwrap();
    assert_eq!(first, second);
    assert_eq!(content_first, content_second);
    assert_eq!(first.enabled, Some(true));
    assert_eq!(
        first.lower_sink.as_deref(),
        Some("/mnt/c/ProgramData/RamShared/nbd-lower-sink")
    );

    // Symlink target must be refused without touching outside file.
    let outside = dir.join("outside.conf");
    std::fs::write(&outside, "untouched").unwrap();
    let link = dir.join("link-auto-update.conf");
    symlink(&outside, &link).unwrap();
    let err = write_auto_update_conf_atomic(&link, true).unwrap_err();
    assert!(err.0.contains("symlink"));
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn post_install_summary_names_single_wizard_command_when_pending() {
    let obs = AssistantObservation {
        installed_release: Some("v0.14.1-1448-gabcdef01".into()),
        cascade_conf_present: true,
        cascade_conf_summary: Some("ZRAM=4096 MiB".into()),
        gpu_count: 1,
        gpu_cache_active: Some(true),
        control_plane_state: Some("vsock_leased".into()),
        cascade_service_enabled: Some(true),
        auto_update_timer_enabled: Some(true),
        auto_update_conf: AutoUpdateFileState {
            present: true,
            enabled: Some(true),
            ..AutoUpdateFileState::default()
        },
        capabilities: sample_caps(false),
    };
    let checklist = build_checklist(&obs);
    let summary = render_post_install_summary(&checklist);
    assert!(summary.contains("NBD_INSTALL_ASSISTANT_OK=5 NBD_INSTALL_ASSISTANT_PENDING=1"));
    assert!(summary.contains("ramshared config wizard"));
    let guidance = kernel_upgrade_guidance_text(&obs.capabilities);
    assert!(guidance.contains("CONFIG_ZRAM_TRACK_ENTRY_ACTIME"));
    assert!(guidance.contains("build-wsl-kernel.sh"));
    assert!(guidance.contains("never triggers `wsl --shutdown`"));
}

#[test]
fn renders_stable_text_and_json_without_tty() {
    let obs = AssistantObservation {
        installed_release: Some("v0.14.1-1448-gabcdef01".into()),
        cascade_conf_present: true,
        cascade_conf_summary: Some(summarize_cascade_conf(
            "# comment\nZRAM_MIB=4096\nVRAM_MIB=4096\nMIN_VRAM_HEADROOM_MIB=2048\n",
        )),
        gpu_count: 1,
        gpu_cache_active: None,
        control_plane_state: None,
        cascade_service_enabled: Some(true),
        auto_update_timer_enabled: Some(false),
        auto_update_conf: AutoUpdateFileState {
            present: true,
            enabled: Some(true),
            ..AutoUpdateFileState::default()
        },
        capabilities: sample_caps(false),
    };
    let checklist = build_checklist(&obs);
    let text = render_checklist_text(&checklist);
    assert!(text.contains("RamShared — Unified Setup & Configuration Assistant"));
    assert!(text.contains("ZRAM=4096 MiB"));
    assert!(text.contains("ramshared config wizard"));
    let json = render_checklist_json(&checklist).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["items"].as_array().unwrap().len(), 6);
    assert_eq!(parsed["recommended_command"], "ramshared config wizard");
    assert_eq!(parsed["capabilities"]["effective_transport"], "nbd");
}
