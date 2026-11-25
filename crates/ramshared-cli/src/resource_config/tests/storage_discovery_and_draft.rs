// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/tests/storage_discovery_and_draft.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn platform_detection_distinguishes_native_linux_from_wsl2() {
    assert!(inventory_platform(crate::readiness::probes::Observation::Unknown).is_err());
    assert!(
        inventory_platform(crate::readiness::probes::Observation::Measured(
            crate::readiness::probes::PlatformKind::Wsl1
        ))
        .is_err()
    );
    assert_eq!(
        inventory_platform(crate::readiness::probes::Observation::Measured(
            crate::readiness::probes::PlatformKind::Wsl2
        ))
        .unwrap(),
        RuntimePlatform::Wsl2
    );
    assert_eq!(
        inventory_platform(crate::readiness::probes::Observation::Measured(
            crate::readiness::probes::PlatformKind::NativeLinux
        ))
        .unwrap(),
        RuntimePlatform::NativeLinux
    );
}

#[test]
fn wsl2_guest_storage_requires_host_volume_identity_and_capacity_binding() {
    let mut candidate = fixture_block_device();
    candidate.eligible_for_file_storage = true;
    candidate.eligibility_reason = "mount is otherwise eligible".into();
    let mut read_only_mount = fixture_mount_info();
    read_only_mount.mountpoint = "/mnt/wslg/distro".into();
    read_only_mount.read_only = true;
    candidate.mounts.push(read_only_mount);

    apply_platform_storage_policy(std::slice::from_mut(&mut candidate), RuntimePlatform::Wsl2);
    assert!(!candidate.eligible_for_file_storage);
    assert!(candidate.eligibility_reason.contains("host-volume"));
    assert!(
        candidate.mounts[0].total_bytes.is_none()
            && candidate.mounts[0].available_bytes.is_none()
            && candidate.mounts[0].capacity_observed_unix_ms.is_none(),
        "guest-only capacity must not be published as host capacity"
    );
    assert!(
        candidate.mounts[1].available_bytes.is_none(),
        "read-only guest mounts have the same host-capacity ambiguity"
    );

    let mut native = fixture_block_device();
    native.eligible_for_file_storage = true;
    native.eligibility_reason = "native filesystem candidate".into();
    apply_platform_storage_policy(
        std::slice::from_mut(&mut native),
        RuntimePlatform::NativeLinux,
    );
    assert!(native.eligible_for_file_storage);
    assert_eq!(native.eligibility_reason, "native filesystem candidate");
    assert!(native.mounts[0].available_bytes.is_some());
}

#[test]
fn wsl_host_backing_is_reported_on_the_root_mount() {
    let mut device = fixture_block_device();
    device.mounts[0].mountpoint = "/".into();
    device.mounts[0].total_bytes = None;
    device.mounts[0].available_bytes = None;
    device.mounts[0].capacity_observed_unix_ms = None;

    let mut windows = fixture_snapshot().windows.expect("windows fixture");
    windows.wsl = Some(WslInventory {
        distro: Some("Ubuntu-24.04".into()),
        vhdx_path: Some(r"I:\wsl2\Ubuntu-24.04\ext4.vhdx".into()),
        drive_letter: Some("I".into()),
    });
    windows.volumes = vec![WindowsVolume {
        drive_letter: Some("I".into()),
        label: Some("WSL2-NVME".into()),
        file_system: Some("NTFS".into()),
        drive_type: "Fixed".into(),
        size_bytes: Some(256_043_380_736),
        free_bytes: Some(65_352_171_520),
        volume_id: None,
    }];

    bind_wsl_host_capacity(std::slice::from_mut(&mut device), &windows);
    assert_eq!(
        device.mounts[0].host_backing,
        Some(HostBacking {
            drive_letter: "I".into(),
            total_bytes: 256_043_380_736,
            free_bytes: 65_352_171_520,
        })
    );
    assert!(device.mounts[0].available_bytes.is_none());
    assert!(device.eligibility_reason.contains("host-volume backing I"));
}

#[test]
fn wsl_host_backing_rejects_ambiguous_or_inconsistent_volume_evidence() {
    let mut windows = fixture_snapshot().windows.expect("windows fixture");
    windows.wsl = Some(WslInventory {
        distro: Some("Ubuntu".into()),
        vhdx_path: Some(r"C:\wsl\ext4.vhdx".into()),
        drive_letter: Some("I".into()),
    });
    windows.volumes = vec![WindowsVolume {
        drive_letter: Some("I".into()),
        label: None,
        file_system: Some("NTFS".into()),
        drive_type: "Fixed".into(),
        size_bytes: Some(100),
        free_bytes: Some(50),
        volume_id: None,
    }];
    let mut device = fixture_block_device();
    device.mounts[0].mountpoint = "/".into();
    bind_wsl_host_capacity(std::slice::from_mut(&mut device), &windows);
    assert!(
        device.mounts[0].host_backing.is_none(),
        "VHDX drive must match the reported host volume"
    );

    windows.wsl.as_mut().unwrap().vhdx_path = Some(r"I:\wsl\ext4.vhdx".into());
    windows.volumes[0].free_bytes = Some(101);
    bind_wsl_host_capacity(std::slice::from_mut(&mut device), &windows);
    assert!(
        device.mounts[0].host_backing.is_none(),
        "free capacity cannot exceed total capacity"
    );

    windows.volumes[0].free_bytes = Some(50);
    windows.volumes.push(windows.volumes[0].clone());
    bind_wsl_host_capacity(std::slice::from_mut(&mut device), &windows);
    assert!(
        device.mounts[0].host_backing.is_none(),
        "duplicate volume identities are ambiguous"
    );
}

#[test]
fn meminfo_accepts_user_sized_ram_and_swap_without_product_minima() {
    // Deliberately varied parser fixtures; these are not product defaults.
    let small = parse_meminfo(
        "MemTotal: 262144 kB\nMemAvailable: 196608 kB\nSwapTotal: 131072 kB\nSwapFree: 65536 kB\n",
    );
    let large = parse_meminfo(
        "MemTotal: 50331648 kB\nMemAvailable: 40265318 kB\nSwapTotal: 20971520 kB\nSwapFree: 15728640 kB\n",
    );

    assert!(small.required_counters_available);
    assert!(large.required_counters_available);
    assert_eq!(small.total_bytes, Some(262_144 * 1024));
    assert_eq!(large.swap_total_bytes, Some(20_971_520 * 1024));
}

#[test]
fn meminfo_missing_or_inconsistent_counters_are_unavailable() {
    let missing = parse_meminfo("MemTotal: 4096 kB\n");
    let inconsistent = parse_meminfo(
        "MemTotal: 4096 kB\nMemAvailable: 8192 kB\nSwapTotal: 4096 kB\nSwapFree: 8192 kB\n",
    );

    assert!(!missing.required_counters_available);
    assert!(!inconsistent.required_counters_available);
    assert_eq!(inconsistent.total_bytes, None);
    assert_eq!(inconsistent.swap_total_bytes, None);
}

#[test]
fn swap_parser_keeps_all_devices_and_variable_capacities() {
    let swaps = parse_swap_table(
        "Filename Type Size Used Priority\n/dev/zram0 partition 524288 32768 100\n/mnt/c/swap\\040file file 12582912 0 -2\n",
    )
    .expect("valid swap inventory");

    assert_eq!(swaps.len(), 2);
    assert_eq!(swaps[0].size_kib, 524_288);
    assert_eq!(swaps[1].filename, "/mnt/c/swap\\040file");
    assert_eq!(swaps[1].size_kib, 12_582_912);
}

#[test]
fn linux_block_inventory_preserves_mounted_and_unmounted_devices() {
    let devices = parse_lsblk_json(
        r#"{"blockdevices":[{"name":"nvme0n1","path":"/dev/nvme0n1","type":"disk","size":2000000000000,"maj:min":"259:0","wwn":"wwn-123","ro":false,"rm":false,"tran":"nvme","mountpoints":[null],"children":[{"name":"nvme0n1p1","path":"/dev/nvme0n1p1","type":"part","size":1000000000000,"maj:min":"259:1","partuuid":"part-uuid","ro":false,"rm":false,"fstype":"ext4","uuid":"fs-uuid","mountpoints":["/data"],"pkname":"nvme0n1"}]},{"name":"sda","path":"/dev/sda","type":"disk","size":500000000000,"maj:min":"8:0","ro":false,"mountpoints":[null]}]}"#,
    )
    .expect("valid lsblk response");

    assert_eq!(devices.len(), 3);
    assert!(devices.iter().any(|device| device.name == "nvme0n1"));
    let unmounted = devices
        .iter()
        .find(|device| device.name == "sda")
        .expect("unmounted disk is visible");
    assert!(unmounted.mountpoints.is_empty());
    let mounted = devices
        .iter()
        .find(|device| device.name == "nvme0n1p1")
        .expect("mounted filesystem is visible");
    assert_eq!(mounted.filesystem.as_deref(), Some("ext4"));
    assert_eq!(mounted.mountpoints, vec!["/data"]);
    assert!(!mounted.eligible_for_file_storage);
    assert_eq!(mounted.major_minor.as_deref(), Some("259:1"));
    assert_eq!(mounted.partition_uuid.as_deref(), Some("part-uuid"));
    assert_eq!(
        mounted.parent_hardware_identity.as_deref(),
        Some("wwn:wwn-123")
    );
    assert_eq!(mounted.removable, Some(false));
    assert_eq!(mounted.transport.as_deref(), Some("nvme"));
}

#[test]
fn mountinfo_parser_decodes_paths_and_records_mount_identity_and_access() {
    let mounts = parse_mountinfo(
        "41 32 259:1 / /mnt/data\\040disk rw,nosuid shared:7 - ext4 /dev/nvme0n1p1 rw,errors=remount-ro\n42 32 8:1 / /mnt/readonly ro - xfs /dev/sda1 ro\n",
    )
    .expect("valid mountinfo");

    assert_eq!(mounts.len(), 2);
    assert_eq!(mounts[0].mount_id, 41);
    assert_eq!(mounts[0].parent_mount_id, 32);
    assert_eq!(mounts[0].major_minor, "259:1");
    assert_eq!(mounts[0].mountpoint, "/mnt/data disk");
    assert_eq!(mounts[0].filesystem, "ext4");
    assert!(!mounts[0].read_only);
    assert!(mounts[1].read_only);
}

#[test]
fn storage_candidate_requires_current_writable_mount_capacity_and_stable_identity() {
    let writable = MountInfo {
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
    };
    let candidate = BlockDevice {
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
        mounts: vec![writable.clone()],
        read_only: Some(false),
        removable: Some(false),
        rotational: Some(false),
        transport: Some("nvme".into()),
        model: Some("Test NVMe".into()),
        eligible_for_file_storage: false,
        eligibility_reason: String::new(),
    };

    assert!(storage_eligibility(&candidate, Some(&writable)).0);
    assert!(!storage_eligibility(&candidate, None).0);
    let mut missing_uuid = candidate.clone();
    missing_uuid.uuid = None;
    assert!(!storage_eligibility(&missing_uuid, Some(&writable)).0);
    let mut missing_parent_identity = candidate.clone();
    missing_parent_identity.parent_hardware_identity = None;
    assert!(!storage_eligibility(&missing_parent_identity, Some(&writable)).0);
    let mut unknown_removable = candidate.clone();
    unknown_removable.removable = None;
    assert!(!storage_eligibility(&unknown_removable, Some(&writable)).0);
    let mut removable = candidate.clone();
    removable.removable = Some(true);
    assert!(!storage_eligibility(&removable, Some(&writable)).0);
    let mut usb = candidate.clone();
    usb.transport = Some("usb".into());
    assert!(!storage_eligibility(&usb, Some(&writable)).0);

    let mut read_only = writable.clone();
    read_only.read_only = true;
    assert!(!storage_eligibility(&candidate, Some(&read_only)).0);
    let mut unknown_capacity = writable.clone();
    unknown_capacity.available_bytes = None;
    assert!(!storage_eligibility(&candidate, Some(&unknown_capacity)).0);
    let mut unsupported = writable;
    unsupported.filesystem = "btrfs".into();
    assert!(!storage_eligibility(&candidate, Some(&unsupported)).0);

    let mut wrong_device = fixture_mount_info();
    wrong_device.major_minor = "8:1".into();
    assert!(!storage_eligibility(&candidate, Some(&wrong_device)).0);
    let mut missing_total = fixture_mount_info();
    missing_total.total_bytes = None;
    assert!(!storage_eligibility(&candidate, Some(&missing_total)).0);
    let mut unmounted_device = candidate.clone();
    unmounted_device.kind = "disk".into();
    assert!(!storage_eligibility(&unmounted_device, Some(&fixture_mount_info())).0);
    let mut read_only_device = candidate.clone();
    read_only_device.read_only = Some(true);
    assert!(!storage_eligibility(&read_only_device, Some(&fixture_mount_info())).0);
    let mut xfs_mount = fixture_mount_info();
    xfs_mount.filesystem = "xfs".into();
    assert!(storage_eligibility(&candidate, Some(&xfs_mount)).0);
}

#[test]
fn storage_candidate_rejects_filesystem_subtree_mounts() {
    let device = fixture_block_device();
    let mut mount = fixture_mount_info();
    mount.root = "/mounted-subtree".into();

    let (eligible, reason) = storage_eligibility(&device, Some(&mount));

    assert!(!eligible);
    assert!(reason.contains("filesystem root"));
}

#[test]
fn network_backed_block_devices_are_ineligible_and_multiple_local_disks_remain_eligible() {
    let nvme = fixture_block_device();
    let nvme_mount = fixture_mount_info();

    let mut sata = fixture_block_device();
    sata.name = "sdb1".into();
    sata.path = "/dev/sdb1".into();
    sata.major_minor = Some("8:17".into());
    sata.parent = Some("sdb".into());
    sata.parent_hardware_identity = Some("serial:local-sata-2".into());
    sata.transport = Some("sata".into());
    let sata_mount = MountInfo {
        major_minor: "8:17".into(),
        mountpoint: "/mnt/second".into(),
        source: "/dev/sdb1".into(),
        filesystem: "xfs".into(),
        ..fixture_mount_info()
    };

    assert!(storage_eligibility(&nvme, Some(&nvme_mount)).0);
    assert!(storage_eligibility(&sata, Some(&sata_mount)).0);

    for transport in ["iscsi", "nbd", "rbd", "fcoe", "nvme-of"] {
        let mut remote = nvme.clone();
        remote.transport = Some(transport.into());
        let (eligible, reason) = storage_eligibility(&remote, Some(&nvme_mount));
        assert!(!eligible, "{transport} must not be a writable local target");
        assert!(reason.contains("network"), "unexpected reason: {reason}");
    }

    for transport in [None, Some("unclassified".into())] {
        let mut ambiguous = nvme.clone();
        ambiguous.transport = transport;
        let (eligible, reason) = storage_eligibility(&ambiguous, Some(&nvme_mount));
        assert!(!eligible, "unproven local transport must be refused");
        assert!(reason.contains("transport"), "unexpected reason: {reason}");
    }
}

#[test]
fn mounted_whole_disk_filesystem_uses_its_own_stable_identity() {
    let mut disk = fixture_block_device();
    disk.name = "sdc".into();
    disk.path = "/dev/sdc".into();
    disk.kind = "disk".into();
    disk.size_bytes = Some(1024 * 1024 * 1024 * 1024);
    disk.uuid = Some("root-fs-uuid".into());
    disk.parent = None;
    disk.major_minor = Some("8:32".into());
    disk.partition_uuid = None;
    disk.hardware_identity = Some("wwn:virtual-disk-123".into());
    disk.parent_hardware_identity = None;

    let mount = MountInfo {
        major_minor: "8:32".into(),
        source: "/dev/sdc".into(),
        ..fixture_mount_info()
    };
    assert!(storage_eligibility(&disk, Some(&mount)).0);

    disk.hardware_identity = None;
    let (eligible, reason) = storage_eligibility(&disk, Some(&mount));
    assert!(!eligible);
    assert!(reason.contains("stable"));
}

#[test]
fn windows_snapshot_lists_multiple_host_volumes_and_live_memory() {
    let snapshot = parse_windows_snapshot(
        r#"{"observed_utc":"2026-09-27T22:00:00Z","host_memory":{"total_bytes":34359738368,"free_bytes":17179869184,"committed_bytes":25769803776,"commit_limit_bytes":60129542144},"volumes":[{"drive_letter":"C","label":"System","file_system":"NTFS","drive_type":"Fixed","size_bytes":500000000000,"free_bytes":200000000000,"volume_id":"vol-c"},{"drive_letter":"I","label":"Data","file_system":"NTFS","drive_type":"Fixed","size_bytes":1000000000000,"free_bytes":600000000000,"volume_id":"vol-i"}],"wsl":{"distro":"Ubuntu-24.04","vhdx_path":"I:\\wsl2\\Ubuntu-24.04\\ext4.vhdx","drive_letter":"I"}}"#,
    )
    .expect("valid Windows snapshot");

    assert_eq!(snapshot.host_memory.free_bytes, Some(17_179_869_184));
    assert_eq!(snapshot.volumes.len(), 2);
    assert_eq!(snapshot.volumes[0].drive_letter.as_deref(), Some("C"));
    assert_eq!(snapshot.volumes[1].drive_letter.as_deref(), Some("I"));
    assert_eq!(
        snapshot
            .wsl
            .as_ref()
            .and_then(|wsl| wsl.drive_letter.as_deref()),
        Some("I")
    );
}

#[test]
fn windows_inventory_lists_every_volume_and_explains_ineligible_targets() {
    let mut snapshot = fixture_snapshot();
    let windows = snapshot.windows.as_mut().expect("Windows snapshot fixture");
    windows.volumes.extend([
        WindowsVolume {
            drive_letter: Some("F".into()),
            label: Some("Removable".into()),
            file_system: Some("exFAT".into()),
            drive_type: "Removable".into(),
            size_bytes: Some(64 * 1024 * 1024 * 1024),
            free_bytes: Some(32 * 1024 * 1024 * 1024),
            volume_id: Some("volume-f".into()),
        },
        WindowsVolume {
            drive_letter: Some("G".into()),
            label: Some("Unsupported".into()),
            file_system: Some("FAT32".into()),
            drive_type: "Fixed".into(),
            size_bytes: Some(64 * 1024 * 1024 * 1024),
            free_bytes: Some(32 * 1024 * 1024 * 1024),
            volume_id: Some("volume-g".into()),
        },
        WindowsVolume {
            drive_letter: Some("H".into()),
            label: Some("Unidentified".into()),
            file_system: Some("NTFS".into()),
            drive_type: "Fixed".into(),
            size_bytes: Some(64 * 1024 * 1024 * 1024),
            free_bytes: Some(32 * 1024 * 1024 * 1024),
            volume_id: None,
        },
        WindowsVolume {
            drive_letter: None,
            label: Some("Malformed path".into()),
            file_system: Some("NTFS".into()),
            drive_type: "Fixed".into(),
            size_bytes: Some(64 * 1024 * 1024 * 1024),
            free_bytes: Some(32 * 1024 * 1024 * 1024),
            volume_id: Some("not-a-volume-guid".into()),
        },
    ]);

    let output = render_text(&snapshot);

    assert!(output.contains("Windows volumes:"));
    assert!(output.contains("C: System NTFS (Fixed) — ID vol-c — eligible"));
    assert!(output.contains(
        "F: Removable exFAT (Removable) — ID volume-f — ineligible: volume is not fixed"
    ));
    assert!(output.contains(
        "G: Unsupported FAT32 (Fixed) — ID volume-g — ineligible: filesystem is not NTFS/ReFS"
    ));
    assert!(output.contains("H: Unidentified NTFS (Fixed) — identity unknown — ineligible: stable volume identity is unavailable"));
    assert!(output.contains("(no drive letter) Malformed path NTFS (Fixed) — ID not-a-volume-guid — ineligible: no safe canonical target path"));
}

#[test]
fn windows_inventory_probe_does_not_filter_volumes_by_drive_type() {
    let script = windows_inventory_script();
    let normalized = script
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();

    assert!(normalized.contains("get-volume-erroractionstop"));
    assert!(!normalized.contains("where-object{$_"));
    assert!(normalized.contains("size_bytes=if($null-ne$_.size)"));
    assert!(normalized.contains("free_bytes=if($null-ne$_.sizeremaining)"));
}

#[test]
fn resource_view_labels_guest_and_windows_host_memory_separately() {
    let snapshot = fixture_snapshot();
    let output = render_text(&snapshot);

    assert!(output.contains("WSL guest RAM"));
    assert!(output.contains("Windows host RAM"));
    assert!(output.contains("Windows commit headroom"));
    assert!(output.contains("C:"));
    assert!(output.contains("I:"));
}

#[test]
fn resource_view_keeps_native_linux_memory_out_of_windows_scope() {
    let mut snapshot = fixture_snapshot();
    snapshot.platform = RuntimePlatform::NativeLinux;
    snapshot.windows = None;
    let mut device = fixture_block_device();
    device.mounts = vec![fixture_mount_info()];
    device.eligible_for_file_storage = true;
    snapshot.block_devices.push(device);
    let output = render_text(&snapshot);

    assert!(output.contains("System RAM"));
    assert!(!output.contains("Windows host RAM"));
    assert!(output.contains("Test NVMe"));
    assert!(output.contains("/data [ext4 mount 41"));
    assert!(output.contains("free 200.00 GiB of 300.00 GiB"));
    assert!(output.contains("Storage speed comparison: not measured"));
}

#[test]
fn malformed_device_and_windows_payloads_fail_closed() {
    assert!(parse_lsblk_json("not json").is_err());
    assert!(parse_lsblk_json("{}").is_err());
    assert!(parse_mountinfo("malformed mount record").is_err());
    assert!(parse_mountinfo("1 0 8:1 / /bad\\777 rw - ext4 /dev/sda1 rw").is_err());
    assert!(parse_lsblk_json(r#"{"blockdevices":[{"path":"/dev/sda","type":"disk"}]}"#).is_err());
    assert!(parse_swap_table("Filename Type Size Used Priority\nbad row\n").is_err());
    assert!(parse_windows_snapshot("[]").is_err());
    assert!(parse_windows_snapshot("{}").is_err());
    assert!(
        parse_windows_snapshot(r#"{"host_memory":{},"volumes":[null],"observed_utc":"now"}"#)
            .is_err()
    );
    assert!(parse_windows_snapshot(r#"{"host_memory":{},"volumes":[]}"#).is_err());
}

#[test]
fn interactive_config_requires_a_tty_and_tui_draws_both_memory_scopes() {
    if !io::stdin().is_terminal() {
        let error = run_interactive().expect_err("non-terminal interactive mode refuses");
        assert!(error.to_string().contains("needs a terminal"));
    }

    let backend = ratatui::backend::TestBackend::new(160, 48);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            draw_config_frame(
                frame,
                &fixture_snapshot(),
                0,
                &tier_editor::Editor::from_snapshot(&fixture_snapshot(), unix_millis()),
            )
        })
        .expect("draw resource configuration screen");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("WSL guest RAM"));
    assert!(rendered.contains("Windows host RAM"));
    assert!(rendered.contains("C:"));
    assert!(rendered.contains("I:"));
}

#[test]
fn config_draft_builds_native_targets_from_eligible_mounts() {
    let mut snapshot = fixture_snapshot();
    snapshot.platform = RuntimePlatform::NativeLinux;
    snapshot.windows = None;
    snapshot.block_devices = vec![fixture_block_device()];

    let candidates = draft_volume_candidates(&snapshot, unix_millis());
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].display.contains("/data"));
    assert_eq!(candidates[0].free_bytes, 200 * 1024 * 1024 * 1024);

    let swap = materialize_draft_target(
        &candidates[0],
        DraftTargetRole::FallbackSwap,
        2 * 1024 * 1024 * 1024,
    )
    .expect("native swap request materializes");
    let origin = materialize_draft_target(
        &candidates[0],
        DraftTargetRole::RamSharedOrigin,
        4 * 1024 * 1024 * 1024,
    )
    .expect("native origin request materializes");
    let profile = ResourceProfile {
        schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
        planned_caps: PlannedTierCaps::default(),
        targets: vec![swap, origin],
    };
    let profile_text = profile.to_toml().expect("draft profile encodes");
    let plan = build_resource_plan(&snapshot, Some(&profile_text))
        .expect("native selected targets plan read-only");

    assert_eq!(plan.status, "ready_for_review");
    assert!(!plan.apply_enabled);
    assert!(!plan.writes_performed);
    assert_eq!(
        plan.targets[0].volume_identity,
        "filesystem:fs-uuid;device:wwn:wwn-123"
    );
    assert_eq!(plan.targets[0].requested_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(plan.targets[1].requested_bytes, 4 * 1024 * 1024 * 1024);
    assert!(profile_text.contains("linux_file_origin_request"));
}

#[test]
fn config_draft_builds_wsl_targets_from_unique_eligible_volumes() {
    let snapshot = fixture_snapshot();
    let candidates = draft_volume_candidates(&snapshot, unix_millis());
    assert_eq!(candidates.len(), 2);
    let selected = candidates
        .iter()
        .find(|candidate| {
            matches!(
                &candidate.storage,
                DraftStorageIdentity::Wsl2 { volume_id, .. } if volume_id == "vol-i"
            )
        })
        .expect("I volume remains selectable");
    assert!(selected.display.contains("I:"));

    let swap = materialize_draft_target(
        selected,
        DraftTargetRole::FallbackSwap,
        2 * 1024 * 1024 * 1024,
    )
    .expect("WSL fallback selection materializes");
    let origin = materialize_draft_target(
        selected,
        DraftTargetRole::RamSharedOrigin,
        4 * 1024 * 1024 * 1024,
    )
    .expect("WSL origin selection materializes");
    let profile = ResourceProfile {
        schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
        planned_caps: PlannedTierCaps::default(),
        targets: vec![swap, origin],
    };
    let profile_text = profile.to_toml().expect("WSL draft profile encodes");
    let plan = build_resource_plan(&snapshot, Some(&profile_text))
        .expect("selected WSL targets plan read-only");

    assert_eq!(plan.status, "ready_for_review");
    assert_eq!(
        plan.targets[0].path,
        "I:\\wsl\\ramshared\\fallback-swap.vhdx"
    );
    assert_eq!(plan.targets[1].path, "I:\\wsl\\ramshared\\origin.vhdx");
    assert!(
        plan.targets
            .iter()
            .all(|target| target.volume_identity == "vol-i")
    );
    assert!(!plan.apply_enabled);
    assert!(!plan.writes_performed);
}
