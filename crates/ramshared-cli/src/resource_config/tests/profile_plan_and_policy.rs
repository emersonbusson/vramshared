// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/tests/profile_plan_and_policy.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn config_draft_builds_wsl_volume_guid_target_without_drive_letter() {
    let mut snapshot = fixture_snapshot();
    let volume_guid = r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\";
    snapshot
        .windows
        .as_mut()
        .expect("Windows inventory fixture")
        .volumes
        .push(WindowsVolume {
            drive_letter: None,
            label: Some("Unlettered data".into()),
            file_system: Some("NTFS".into()),
            drive_type: "Fixed".into(),
            size_bytes: Some(300 * 1024 * 1024 * 1024),
            free_bytes: Some(200 * 1024 * 1024 * 1024),
            volume_id: Some(volume_guid.into()),
        });

    let candidates = draft_volume_candidates(&snapshot, unix_millis());
    let selected = candidates
        .iter()
        .find(|candidate| {
            matches!(
                &candidate.storage,
                DraftStorageIdentity::Wsl2 { volume_id, drive_letter: None }
                    if volume_id == volume_guid
            )
        })
        .expect("canonical volume-GUID target is selectable without a drive letter");
    let target = materialize_draft_target(
        selected,
        DraftTargetRole::RamSharedOrigin,
        1024 * 1024 * 1024,
    )
    .expect("volume-GUID origin request materializes");
    let profile = ResourceProfile {
        schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
        planned_caps: PlannedTierCaps::default(),
        targets: vec![target],
    };
    let profile_text = profile.to_toml().expect("volume-GUID profile encodes");
    let plan = build_resource_plan(&snapshot, Some(&profile_text))
        .expect("volume-GUID target plans against its unique volume");

    assert_eq!(plan.status, "ready_for_review");
    assert_eq!(
        plan.targets[0].path,
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\wsl\ramshared\origin.vhdx"
    );
    assert_eq!(plan.targets[0].volume_identity, volume_guid.to_lowercase());
}

#[test]
fn config_draft_refuses_ineligible_ambiguous_stale_and_overflowed_targets() {
    let now = unix_millis();
    let mut ambiguous_linux = fixture_snapshot();
    ambiguous_linux.platform = RuntimePlatform::NativeLinux;
    ambiguous_linux.windows = None;
    ambiguous_linux.block_devices = vec![fixture_block_device(), fixture_block_device()];
    assert!(draft_volume_candidates(&ambiguous_linux, now).is_empty());

    let mut stale_windows = fixture_snapshot();
    stale_windows
        .windows
        .as_mut()
        .expect("Windows inventory fixture")
        .observed_unix_ms = Some(now.saturating_sub(STORAGE_SAMPLE_MAX_AGE_MS + 1));
    assert!(draft_volume_candidates(&stale_windows, now).is_empty());

    let mut unsupported_windows = fixture_snapshot();
    unsupported_windows
        .windows
        .as_mut()
        .expect("Windows inventory fixture")
        .volumes[1]
        .drive_type = "Removable".into();
    assert_eq!(draft_volume_candidates(&unsupported_windows, now).len(), 1);

    let mut duplicate_volume_id = fixture_snapshot();
    let duplicate = duplicate_volume_id
        .windows
        .as_mut()
        .expect("Windows inventory fixture")
        .volumes[1]
        .clone();
    duplicate_volume_id
        .windows
        .as_mut()
        .expect("Windows inventory fixture")
        .volumes
        .push(duplicate);
    assert_eq!(draft_volume_candidates(&duplicate_volume_id, now).len(), 1);

    let mut malformed_unlettered = fixture_snapshot();
    malformed_unlettered
        .windows
        .as_mut()
        .expect("Windows inventory fixture")
        .volumes
        .push(WindowsVolume {
            drive_letter: None,
            label: Some("Malformed identity".into()),
            file_system: Some("NTFS".into()),
            drive_type: "Fixed".into(),
            size_bytes: Some(64 * 1024 * 1024 * 1024),
            free_bytes: Some(32 * 1024 * 1024 * 1024),
            volume_id: Some("not-a-volume-guid".into()),
        });
    assert_eq!(draft_volume_candidates(&malformed_unlettered, now).len(), 2);

    assert!(parse_draft_size_mib("0").is_err());
    assert!(parse_draft_size_mib("18446744073709551615").is_err());
    assert!(parse_draft_size_mib("-1").is_err());
    assert_eq!(parse_draft_size_mib("4096").unwrap(), 4096 * 1024 * 1024);
}

#[test]
fn resource_plan_aggregates_case_aliases_before_capacity_check() {
    let mut snapshot = fixture_snapshot();
    snapshot
        .windows
        .as_mut()
        .expect("Windows inventory fixture")
        .volumes[0]
        .free_bytes = Some(65 * 1024 * 1024 * 1024);
    let profile = format!(
        r#"
schema_version = 1

[[targets]]
kind = "wsl_fallback"
windows_volume_id = "vol-c"
path = "C:\\wsl\\ramshared\\fallback.vhdx"
bytes = {}

[[targets]]
kind = "wsl_origin"
windows_volume_id = "VOL-C"
path = "C:\\wsl\\ramshared\\origin.vhdx"
allocated_bytes = {}
"#,
        30_u64 * 1024 * 1024 * 1024,
        30_u64 * 1024 * 1024 * 1024
    );

    let plan = build_resource_plan(&snapshot, Some(&profile))
        .expect("case variants resolve to the same current volume");

    assert_eq!(plan.status, "blocked");
    assert_eq!(plan.targets[0].status, "insufficient_space");
    assert_eq!(plan.targets[1].status, "insufficient_space");
    assert_eq!(plan.targets[0].required_free_bytes, 70 * 1024 * 1024 * 1024);
    assert_eq!(plan.targets[0].volume_identity, "vol-c");
    assert!(!plan.apply_enabled);
}

#[test]
fn config_draft_save_requires_owned_parent_uses_mode_0600_and_never_overwrites() {
    let directory = draft_test_directory("save");
    let path = directory.join("profile.toml");
    let first = "schema_version = 1\n";
    let saved = save_profile_draft(&path, first).expect("draft saves securely");
    assert_eq!(saved, path);
    assert_eq!(
        fs::read_to_string(&path).expect("draft content reads back"),
        first
    );
    let metadata = fs::metadata(&path).expect("draft metadata");
    assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert!(save_profile_draft(&path, "changed\n").is_err());
    assert_eq!(fs::read_to_string(&path).expect("original remains"), first);

    let unsafe_directory = directory.join("unsafe");
    fs::create_dir(&unsafe_directory).expect("unsafe fixture directory creates");
    let mut permissions = fs::metadata(&unsafe_directory)
        .expect("unsafe directory metadata")
        .permissions();
    permissions.set_mode(0o777);
    fs::set_permissions(&unsafe_directory, permissions).expect("unsafe mode applies");
    assert!(save_profile_draft(&unsafe_directory.join("blocked.toml"), first).is_err());

    fs::remove_dir_all(directory).expect("draft fixtures removed");
}

#[test]
fn config_draft_wizard_saves_planned_caps_as_unenforced_draft_policy() {
    let snapshot = fixture_snapshot();
    let directory = draft_test_directory("wizard");
    let output_path = directory.join("profile.toml");
    let mut input = io::Cursor::new("origin\n2\n4096\nswap\n1\n1024\ndone\n512\n2048\nSAVE\n");
    let mut output = Vec::new();

    run_draft_wizard_with_io(&snapshot, &output_path, &mut input, &mut output)
        .expect("confirmed draft wizard completes");

    let output = String::from_utf8(output).expect("wizard output is UTF-8");
    assert!(output.contains("Windows host RAM"));
    assert!(output.contains("I:\\wsl\\ramshared\\origin.vhdx"));
    assert!(output.contains("Draft only: no swap, origin, GPU, .wslconfig"));
    assert!(output.contains("ZRAM planned cap (unenforced): 512 MiB"));
    assert!(output.contains("SSD origin planned cap (unenforced): 2048 MiB"));
    assert!(
        output.contains("VRAM planned cap unavailable: no validated adapter budget was sampled")
    );
    assert!(output.contains("Planned caps are unenforced draft policy only"));
    assert!(output.contains("Validated draft saved"));
    let profile_text = fs::read_to_string(&output_path).expect("saved draft reads");
    let profile = ResourceProfile::parse(&profile_text).expect("saved profile parses");
    assert_eq!(profile.targets.len(), 2);
    assert!(profile.targets.iter().any(|target| matches!(
        target,
        ResourceTarget::WslOrigin { windows_volume_id, .. } if windows_volume_id == "vol-i"
    )));
    assert!(profile.targets.iter().any(|target| matches!(
        target,
        ResourceTarget::WslFallback { windows_volume_id, .. } if windows_volume_id == "vol-c"
    )));
    assert_eq!(profile.planned_caps.zram_bytes, Some(512 * 1024 * 1024));
    assert_eq!(profile.planned_caps.origin_bytes, Some(2048 * 1024 * 1024));
    assert!(profile.planned_caps.vram_bytes.is_empty());

    let canceled_path = directory.join("canceled.toml");
    let mut input = io::Cursor::new("swap\n1\n1024\ndone\n\n\nNO\n");
    let mut output = Vec::new();
    run_draft_wizard_with_io(&snapshot, &canceled_path, &mut input, &mut output)
        .expect("declined draft is a successful cancellation");
    assert!(!canceled_path.exists());

    let invalid_cap_path = directory.join("invalid-cap.toml");
    let mut input = io::Cursor::new("swap\n1\n1024\ndone\n18446744073709551615\n\nSAVE\n");
    let mut output = Vec::new();
    assert!(
        run_draft_wizard_with_io(&snapshot, &invalid_cap_path, &mut input, &mut output).is_err()
    );
    assert!(!invalid_cap_path.exists());

    for (index, invalid_cap) in ["0", "-1", "not-a-size"].iter().enumerate() {
        let invalid_path = directory.join(format!("invalid-cap-{index}.toml"));
        let input_text = format!("swap\n1\n1024\ndone\n{invalid_cap}\n\nSAVE\n");
        let mut input = io::Cursor::new(input_text);
        let mut output = Vec::new();
        assert!(
            run_draft_wizard_with_io(&snapshot, &invalid_path, &mut input, &mut output).is_err()
        );
        assert!(!invalid_path.exists());
    }

    let unset_caps_path = directory.join("unset-caps.toml");
    let mut input = io::Cursor::new("swap\n1\n1024\ndone\n\n\nSAVE\n");
    let mut output = Vec::new();
    run_draft_wizard_with_io(&snapshot, &unset_caps_path, &mut input, &mut output)
        .expect("blank planned caps remain unset");
    let profile_text = fs::read_to_string(&unset_caps_path).expect("unset draft reads");
    let profile = ResourceProfile::parse(&profile_text).expect("unset profile parses");
    assert_eq!(profile.planned_caps.zram_bytes, None);
    assert_eq!(profile.planned_caps.origin_bytes, None);

    fs::remove_dir_all(directory).expect("wizard fixtures removed");
}

#[test]
fn plan_text_and_json_label_caps_as_unenforced_draft_planned_caps() {
    let snapshot = fixture_snapshot();
    let profile_text = r#"
schema_version = 1

[caps]
zram_bytes = 536870912
origin_bytes = 2147483648

[[targets]]
kind = "wsl_fallback"
windows_volume_id = "vol-c"
path = "C:\\wsl\\ramshared\\fallback.vhdx"
bytes = 1073741824
"#;
    let plan = build_resource_plan(&snapshot, Some(profile_text))
        .expect("draft profile with caps builds a read-only plan");
    assert!(!plan.apply_enabled);
    assert!(!plan.writes_performed);

    let text = render_plan_text(&plan);
    let text_lowered = text.to_ascii_lowercase();
    assert!(
        text_lowered.contains("unenforced planned draft caps"),
        "plan text must label caps as unenforced draft caps: {text}"
    );
    assert!(
        text_lowered.contains("planned cap (unenforced)"),
        "plan text must mark each cap value unenforced: {text}"
    );
    assert!(
        !text_lowered.contains("ceiling"),
        "plan text must not present draft caps as ceilings: {text}"
    );
    assert!(text.contains("Apply enabled: false"));

    let json = render_plan_json(&plan).expect("plan JSON renders");
    let json_lowered = json.to_ascii_lowercase();
    assert!(
        json.contains("\"unenforced_planned_caps\""),
        "plan JSON must label caps as unenforced planned caps: {json}"
    );
    assert!(
        !json_lowered.contains("ceiling") && !json.contains("user_caps"),
        "plan JSON must not present draft caps as enforced user caps: {json}"
    );

    let directory = draft_test_directory("unenforced-labels");
    let output_path = directory.join("profile.toml");
    let mut input = io::Cursor::new("swap\n1\n1024\ndone\n512\n2048\nSAVE\n");
    let mut output = Vec::new();
    run_draft_wizard_with_io(&snapshot, &output_path, &mut input, &mut output)
        .expect("wizard completes");
    let wizard = String::from_utf8(output).expect("wizard output is UTF-8");
    let wizard_lowered = wizard.to_ascii_lowercase();
    assert!(
        !wizard_lowered.contains("ceiling"),
        "wizard text must not present draft caps as ceilings: {wizard}"
    );
    assert!(
        wizard_lowered.contains("unenforced") && wizard_lowered.contains("planned cap"),
        "wizard text must label values as unenforced planned caps: {wizard}"
    );
    let saved = fs::read_to_string(&output_path).expect("saved draft reads");
    assert!(
        saved.contains("planned_caps"),
        "saved draft must name the caps table planned_caps: {saved}"
    );
    fs::remove_dir_all(directory).expect("label fixtures removed");
}

#[test]
fn current_guest_config_show_is_read_only_before_and_after() {
    fn topology(text: &str) -> Vec<(String, String, u64, i32)> {
        parse_swap_table(text)
            .expect("guest swap table parses")
            .into_iter()
            .map(|swap| (swap.filename, swap.kind, swap.size_kib, swap.priority))
            .collect()
    }

    let swaps_before = std::fs::read_to_string("/proc/swaps").expect("guest swap table");
    let topology_before = topology(&swaps_before);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let exit = run(ConfigMode::Show { json: true }, &mut stdout, &mut stderr);

    let swaps_after = std::fs::read_to_string("/proc/swaps").expect("guest swap table");
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
    assert_eq!(topology(&swaps_after), topology_before);
    let json: serde_json::Value = serde_json::from_slice(&stdout).expect("resource snapshot JSON");
    assert!(json.get("platform").is_some());
    assert!(json.get("guest_memory").is_some());
    if json["platform"] == "wsl2" {
        // The Windows provider is optional and the host may be
        // unreachable (a wedged WSL interop, a stopped Windows host). The
        // read-only property this test proves is the guest's: the swap
        // table and the exit code are unchanged. When the provider is
        // simply absent there is nothing to assert about it, and failing
        // the suite on the host's availability would hide the product
        // property behind the environment.
        let Some(windows) = json.get("windows").and_then(serde_json::Value::as_object) else {
            eprintln!(
                "skipping the Windows provider assertion: the host inventory is \
                 unavailable; warnings={}",
                json.get("warnings").unwrap_or(&serde_json::Value::Null)
            );
            return;
        };
        assert!(windows["host_memory"]["free_bytes"].is_number());
        assert!(windows["host_memory"]["commit_limit_bytes"].is_number());
        assert!(!windows["volumes"].as_array().unwrap().is_empty());
    }
}

#[test]
fn config_plan_never_mutates_host_or_guest() {
    let snapshot = fixture_snapshot();
    let before = render_json(&snapshot).expect("snapshot serializes before planning");
    let profile = r#"
schema_version = 1

[[targets]]
kind = "wsl_fallback"
windows_volume_id = "vol-i"
path = "I:\\wsl\\swap.vhdx"
bytes = 4294967296
"#;

    let plan = build_resource_plan(&snapshot, Some(profile)).expect("profile plans");

    assert_eq!(plan.status, "ready_for_review");
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(plan.targets[0].status, "storage_ready");
    assert_eq!(plan.targets[0].required_free_bytes, 14 * 1024 * 1024 * 1024);
    assert!(!plan.apply_enabled);
    assert_eq!(
        render_json(&snapshot).expect("snapshot serializes after planning"),
        before,
        "planning must leave the observed host/guest snapshot unchanged"
    );
}

#[test]
fn resource_plan_without_profile_reports_not_configured_and_read_only() {
    let plan = build_resource_plan(&fixture_snapshot(), None).expect("empty plan builds");

    assert_eq!(plan.profile_state, "not_configured");
    assert_eq!(plan.status, "not_configured");
    assert!(plan.targets.is_empty());
    assert!(!plan.writes_performed);
    assert!(!plan.apply_enabled);
}

#[test]
fn resource_policy_rejects_unknown_stale_and_inconsistent_samples() {
    let unknown_volume = r#"
schema_version = 1

[[targets]]
kind = "wsl_fallback"
windows_volume_id = "volume-not-in-inventory"
path = "C:\\wsl\\swap.vhdx"
bytes = 1048576
"#;
    let unknown_plan = build_resource_plan(&fixture_snapshot(), Some(unknown_volume))
        .expect("unbound target remains a visible refusal");
    assert_eq!(unknown_plan.targets[0].status, "identity_unavailable");

    let mismatched_path = r#"
schema_version = 1

[[targets]]
kind = "wsl_fallback"
windows_volume_id = "vol-i"
path = "C:\\wsl\\swap.vhdx"
bytes = 1048576
"#;
    let mismatched_plan = build_resource_plan(&fixture_snapshot(), Some(mismatched_path))
        .expect("path-volume mismatch remains a visible refusal");
    assert_eq!(mismatched_plan.targets[0].status, "identity_unavailable");

    let valid_volume = r#"
schema_version = 1

[[targets]]
kind = "wsl_fallback"
windows_volume_id = "vol-i"
path = "I:\\wsl\\swap.vhdx"
bytes = 1048576
"#;
    let mut stale_snapshot = fixture_snapshot();
    stale_snapshot
        .windows
        .as_mut()
        .expect("fixture includes host inventory")
        .observed_unix_ms = Some(1);
    let stale_plan = build_resource_plan(&stale_snapshot, Some(valid_volume))
        .expect("stale telemetry is represented as a refusal");
    assert_eq!(stale_plan.targets[0].status, "stale_sample");

    let mut inconsistent_snapshot = fixture_snapshot();
    let volume = &mut inconsistent_snapshot
        .windows
        .as_mut()
        .expect("fixture includes host inventory")
        .volumes[1];
    volume.size_bytes = Some(1);
    volume.free_bytes = Some(2);
    let inconsistent_plan = build_resource_plan(&inconsistent_snapshot, Some(valid_volume))
        .expect("inconsistent capacity is represented as a refusal");
    assert_eq!(inconsistent_plan.targets[0].status, "inconsistent_sample");
}

#[test]
fn resource_plan_rejects_drive_and_volume_guid_aliases_for_same_target() {
    let mut snapshot = fixture_snapshot();
    let volume_guid = r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\";
    snapshot
        .windows
        .as_mut()
        .expect("fixture includes host inventory")
        .volumes[1]
        .volume_id = Some(volume_guid.into());
    let profile = format!(
        r#"
schema_version = 1

[[targets]]
kind = "wsl_fallback"
windows_volume_id = {volume_guid:?}
path = "I:\\wsl\\swap.vhdx"
bytes = 1048576

[[targets]]
kind = "wsl_origin"
windows_volume_id = {volume_guid:?}
path = "\\\\?\\Volume{{01234567-89ab-cdef-0123-456789abcdef}}\\wsl\\swap.vhdx"
allocated_bytes = 1048576
"#
    );

    let plan = build_resource_plan(&snapshot, Some(&profile)).expect("profile plans");

    assert_eq!(plan.targets[0].status, "storage_ready");
    assert_eq!(plan.targets[1].status, "duplicate_target_path");
    assert!(!plan.apply_enabled);
}

#[test]
fn profile_loader_rejects_symlinks_oversized_files_and_untrusted_system_profiles() {
    use std::os::unix::fs::symlink;

    let directory = std::env::temp_dir().join(format!(
        "ramshared-profile-loader-{}-{}",
        std::process::id(),
        unix_millis()
    ));
    fs::create_dir(&directory).expect("temporary test directory is created");
    let profile_path = directory.join("profile.toml");
    let profile_content = "schema_version = 1\n";
    fs::write(&profile_path, profile_content).expect("temporary profile is written");

    assert_eq!(
        load_profile_text(&profile_path, false).expect("explicit profile reads"),
        Some(profile_content.into())
    );
    assert!(load_profile_text(&profile_path, true).is_err());

    let symlink_path = directory.join("profile-link.toml");
    symlink(&profile_path, &symlink_path).expect("profile symlink is created");
    assert!(load_profile_text(&symlink_path, false).is_err());
    assert!(load_profile_text(&directory, false).is_err());

    let oversized_path = directory.join("oversized.toml");
    fs::write(&oversized_path, vec![b'x'; MAX_RESOURCE_PROFILE_BYTES + 1])
        .expect("oversized test profile is written");
    assert!(load_profile_text(&oversized_path, false).is_err());

    fs::remove_dir_all(directory).expect("temporary profile fixtures are removed");
}

#[test]
fn draft_output_refuses_symlink_parent() {
    let root = draft_test_directory("symlink-parent");
    let real = root.join("real");
    fs::create_dir(&real).unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(save_profile_draft(&link.join("draft.toml"), "schema_version = 1\n").is_err());
    assert!(!real.join("draft.toml").exists());
    fs::remove_dir_all(root).unwrap();
}
