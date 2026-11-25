// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

pub(super) use std::cell::Cell;

pub(super) use std::fs;

pub(super) use std::io::{self, IsTerminal};

pub(super) use std::os::unix::fs::{MetadataExt, PermissionsExt};

pub(super) use std::path::PathBuf;

pub(super) use std::rc::Rc;

pub(super) use std::time::{SystemTime, UNIX_EPOCH};

pub(super) use super::draft::*;

pub(super) use super::inventory::*;

pub(super) use super::render_ui::*;

pub(super) use super::*;

pub(super) use ramshared_block::storage_inventory::HostBacking;

pub(super) use ratatui::Terminal;

pub(super) use ratatui::backend::TestBackend;

pub(super) use ratatui::crossterm::event::KeyCode;

pub(in crate::resource_config) fn draft_test_directory(label: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time follows Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "ramshared-config-draft-{label}-{}-{timestamp}",
        std::process::id()
    ));
    fs::create_dir(&path).expect("temporary draft directory creates");
    path
}

pub(in crate::resource_config) fn fixture_mount_info() -> MountInfo {
    MountInfo {
        mount_id: 41,
        parent_mount_id: 32,
        major_minor: "259:1".into(),
        root: "/".into(),
        mountpoint: "/data".into(),
        mount_options: vec!["rw".into()],
        filesystem: "ext4".into(),
        source: "/dev/nvme0n1p1".into(),
        super_options: vec!["rw".into()],
        read_only: false,
        total_bytes: Some(300 * 1024 * 1024 * 1024),
        available_bytes: Some(200 * 1024 * 1024 * 1024),
        capacity_observed_unix_ms: Some(unix_millis()),
        host_backing: None,
    }
}

pub(in crate::resource_config) fn fixture_block_device() -> BlockDevice {
    BlockDevice {
        name: "nvme0n1p1".into(),
        path: "/dev/nvme0n1p1".into(),
        kind: "part".into(),
        size_bytes: Some(300 * 1024 * 1024 * 1024),
        filesystem: Some("ext4".into()),
        uuid: Some("fs-uuid".into()),
        mountpoints: vec!["/data".into()],
        parent: Some("nvme0n1".into()),
        major_minor: Some("259:1".into()),
        partition_uuid: Some("part-uuid".into()),
        hardware_identity: None,
        parent_hardware_identity: Some("wwn:wwn-123".into()),
        mounts: vec![fixture_mount_info()],
        read_only: Some(false),
        removable: Some(false),
        rotational: Some(false),
        transport: Some("nvme".into()),
        model: Some("Test NVMe".into()),
        eligible_for_file_storage: false,
        eligibility_reason: String::new(),
    }
}

pub(in crate::resource_config) fn fixture_snapshot() -> ResourceSnapshot {
    ResourceSnapshot {
        platform: RuntimePlatform::Wsl2,
        observed_unix_ms: unix_millis(),
        guest_observed_unix_ms: unix_millis(),
        gpu_observed_unix_ms: unix_millis(),
        guest_memory: MemorySnapshot {
            total_bytes: Some(16 * 1024 * 1024 * 1024),
            available_bytes: Some(8 * 1024 * 1024 * 1024),
            swap_total_bytes: Some(4 * 1024 * 1024 * 1024),
            swap_free_bytes: Some(2 * 1024 * 1024 * 1024),
            required_counters_available: true,
        },
        swaps: Vec::new(),
        windows: Some(WindowsSnapshot {
            observed_utc: "2026-09-27T22:00:00Z".into(),
            observed_unix_ms: Some(unix_millis()),
            wsl: None,
            host_memory: HostMemorySnapshot {
                total_bytes: Some(32 * 1024 * 1024 * 1024),
                free_bytes: Some(16 * 1024 * 1024 * 1024),
                committed_bytes: Some(24 * 1024 * 1024 * 1024),
                commit_limit_bytes: Some(56 * 1024 * 1024 * 1024),
            },
            volumes: vec![
                WindowsVolume {
                    drive_letter: Some("C".into()),
                    label: Some("System".into()),
                    file_system: Some("NTFS".into()),
                    drive_type: "Fixed".into(),
                    size_bytes: Some(500_000_000_000),
                    free_bytes: Some(200_000_000_000),
                    volume_id: Some("vol-c".into()),
                },
                WindowsVolume {
                    drive_letter: Some("I".into()),
                    label: Some("Data".into()),
                    file_system: Some("NTFS".into()),
                    drive_type: "Fixed".into(),
                    size_bytes: Some(1_000_000_000_000),
                    free_bytes: Some(600_000_000_000),
                    volume_id: Some("vol-i".into()),
                },
            ],
        }),
        block_devices: Vec::new(),
        gpu_inventory: Vec::new(),
        gpu_budget_status: "not sampled by storage inventory".into(),
        warnings: Vec::new(),
        // An explicit isolated receipt observation; never this host's files.
        prepared_native_origin: prepared_origin::Observation::Absent,
    }
}

#[path = "tests/profile_plan_and_policy.rs"]
mod profile_plan_and_policy;
#[path = "tests/storage_discovery_and_draft.rs"]
mod storage_discovery_and_draft;
#[path = "tests/wizard_and_validation.rs"]
mod wizard_and_validation;
