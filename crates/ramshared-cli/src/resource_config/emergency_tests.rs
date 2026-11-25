// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/emergency_tests.rs
#![allow(clippy::expect_used)]
use super::*;

#[test]
fn config_draft_persists_emergency_request_without_apply() {
    for platform in [RuntimePlatform::Wsl2, RuntimePlatform::NativeLinux] {
        let mut snapshot = tests::fixture_snapshot();
        snapshot.platform = platform;
        if platform == RuntimePlatform::NativeLinux {
            snapshot.windows = None;
            snapshot.block_devices = vec![tests::fixture_block_device()];
        }
        let directory = tests::draft_test_directory("emergency");
        let path = directory.join("draft.toml");
        let mut input = io::Cursor::new("emergency\n1\n512\ndone\n\n\nSAVE\n");
        run_draft_wizard_with_io(&snapshot, &path, &mut input, &mut Vec::new())
            .expect("save emergency intent");
        let text = fs::read_to_string(&path).expect("saved draft");
        let profile = ResourceProfile::parse(&text).expect("parse intent");
        assert_eq!(profile.targets[0].allocated_bytes(), 512 * BYTES_PER_MIB);
        assert!(matches!(
            profile.targets[0],
            ResourceTarget::LinuxEmergencySwapfile { .. }
                | ResourceTarget::WslEmergencyStorage { .. }
        ));
        let plan = build_resource_plan(&snapshot, Some(&text)).expect("read-only plan");
        assert_eq!(plan.status, "ready_for_review");
        assert!(!plan.apply_enabled && !plan.writes_performed);
        assert!(plan.targets[0].path.contains("emergency"));
        assert_eq!(
            plan.targets[0].required_free_bytes,
            ramshared_config::resource_profile::DISK_RESERVE_FLOOR_BYTES + 512 * BYTES_PER_MIB
        );
        fs::remove_dir_all(directory).expect("remove fixture");
    }
}
