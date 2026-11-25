// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/config/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

const GOOD: &str = r#"
[win_drive]
size_bytes = 536870912
block_size = 4096
cuda_device = 0
reserve_bytes = 536870912
queue_depth = 4
max_io_bytes = 1048576
evidence_path = "C:\\ProgramData\\RamShared\\evidence"
volume_letter = "D"
broker_pipe = "named_pipe_v1"
broker_ready_timeout_secs = 30
tenant = "windrive-host"
"#;

#[test]
fn test_config_valid_toml_parses_successfully() {
    let c = WinDriveConfig::from_toml(GOOD).unwrap();
    assert_eq!(c.size_bytes, 512 * 1024 * 1024);
    assert_eq!(c.block_size, 4096);
    assert_eq!(c.cuda_device, 0);
    assert_eq!(c.queue_depth, 4);
    assert_eq!(c.max_io_bytes, 1 << 20);
    assert_eq!(c.heartbeat_secs, 5);
    assert_eq!(c.tenant, "windrive-host");
    assert_eq!(c.volume_letter, 'D');
    assert!(c.volume_mount_path.is_none());
    assert!(is_absolute_path(&c.evidence_path));
    assert_eq!(c.broker_pipe, BrokerPipeV1::NamedPipeV1);
}

#[test]
fn reject_unknown_backend() {
    let bad = format!("{GOOD}backend = \"ram\"\n");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn test_config_empty_input_fails() {
    let bad = "";
    let e = WinDriveConfig::from_toml(bad).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn test_config_missing_required_fields_fails() {
    let bad = r#"
[win_drive]
size_bytes = 536870912
block_size = 4096
cuda_device = 0
reserve_bytes = 536870912
queue_depth = 4
max_io_bytes = 1048576
evidence_path = "C:\\ProgramData\\RamShared\\evidence"
volume_letter = "D"
broker_pipe = "named_pipe_v1"
broker_ready_timeout_secs = 30
"#;
    let e = WinDriveConfig::from_toml(bad).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn test_config_missing_fields_with_defaults_parses_successfully() {
    let toml_without_defaults = r#"
[win_drive]
size_bytes = 536870912
block_size = 4096
cuda_device = 0
reserve_bytes = 536870912
queue_depth = 4
max_io_bytes = 1048576
evidence_path = "C:\\ProgramData\\RamShared\\evidence"
volume_letter = "D"
broker_pipe = "named_pipe_v1"
broker_ready_timeout_secs = 30
tenant = "windrive-host"
"#;
    let c = WinDriveConfig::from_toml(toml_without_defaults).unwrap();
    assert_eq!(c.heartbeat_secs, 5);
    assert_eq!(c.volume_mount_path, None);
}

#[test]
fn test_config_invalid_types_fails() {
    let bad = r#"
[win_drive]
size_bytes = "536870912"
block_size = 4096
cuda_device = 0
reserve_bytes = 536870912
queue_depth = 4
max_io_bytes = 1048576
evidence_path = "C:\\ProgramData\\RamShared\\evidence"
volume_letter = "D"
broker_pipe = "named_pipe_v1"
broker_ready_timeout_secs = 30
tenant = "windrive-host"
"#;
    let e = WinDriveConfig::from_toml(bad).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn reject_zero_size() {
    let bad = GOOD.replace("536870912", "0");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "size_bytes",
            ..
        }
    ));
}

#[test]
fn test_config_zero_capacity_rejects() {
    let bad = GOOD.replace("size_bytes = 536870912", "size_bytes = 0");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "size_bytes",
            ..
        }
    ));
}

#[test]
fn test_config_size_negative_rejects() {
    let bad = GOOD.replace("size_bytes = 536870912", "size_bytes = -1");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn test_config_size_u64_max_rejects() {
    let bad = GOOD.replace(
        "size_bytes = 536870912",
        &format!("size_bytes = {}", u64::MAX),
    );
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "size_bytes",
            ..
        }
    ));
}

#[test]
fn test_config_non_utf8_rejects() {
    let bad: &[u8] = &[0xff, 0xfe, 0xfd];
    let e = WinDriveConfig::from_reader(bad).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn reject_size_over_usize() {
    // On 64-bit hosts usize max is huge; force invalid by using unaligned + below floor path
    // via a direct validate of an oversized conceptual field when possible.
    let mut c = WinDriveConfig::from_toml(GOOD).unwrap();
    // Use size that fails block alignment after we set a huge value that is not
    // representable only on 32-bit; on 64-bit still exercise the usize check path.
    if usize::BITS < 64 {
        c.size_bytes = (u64::from(u32::MAX) + 1) * 4096;
        let e = c.validate().unwrap_err();
        assert!(matches!(
            e,
            ConfigError::Invalid {
                field: "size_bytes",
                ..
            }
        ));
    } else {
        // Still reject sizes below floor or unaligned when forced.
        c.size_bytes = 4096;
        let e = c.validate().unwrap_err();
        assert!(matches!(
            e,
            ConfigError::Invalid {
                field: "size_bytes",
                ..
            }
        ));
    }
}

#[test]
fn reject_unaligned_max_io() {
    let bad = GOOD.replace("max_io_bytes = 1048576", "max_io_bytes = 1000");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "max_io_bytes",
            ..
        }
    ));
}

#[test]
fn reject_queue_data_area_over_4mib() {
    // QD=8 * 1MiB = 8 MiB > 4 MiB
    let bad = GOOD.replace("queue_depth = 4", "queue_depth = 8");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "queue_depth",
            ..
        }
    ));
}

#[test]
fn reserve_cannot_lower_policy_floor() {
    let c = WinDriveConfig::from_toml(GOOD).unwrap();
    // Requested 1 byte would still floor to 512 MiB or 10% of total.
    let mut low = c.clone();
    low.reserve_bytes = 1;
    assert_eq!(
        low.effective_reserve_bytes(10 * 1024 * 1024 * 1024),
        RESERVE_FLOOR_BYTES.max((10 * 1024 * 1024 * 1024u64).div_ceil(10))
    );
    // 10% of small total still respects floor when floor wins.
    assert_eq!(
        low.effective_reserve_bytes(1024 * 1024 * 1024),
        RESERVE_FLOOR_BYTES
    );
}

#[test]
fn effective_reserve_bytes_comprehensive_scenarios() {
    let mut c = WinDriveConfig::from_toml(GOOD).unwrap();

    // 1. Config reserve dominance: reserve_bytes > floor (512 MiB) and > 10% VRAM
    c.reserve_bytes = 2 * 1024 * 1024 * 1024; // 2 GiB
    let total_vram = 10 * 1024 * 1024 * 1024; // 10 GiB, 10% = 1 GiB
    assert_eq!(
        c.effective_reserve_bytes(total_vram),
        2 * 1024 * 1024 * 1024
    );

    // 2. Policy floor dominance: reserve_bytes = 0, VRAM 10% < 512 MiB
    c.reserve_bytes = 0;
    let small_vram = 2 * 1024 * 1024 * 1024; // 2 GiB, 10% = 204.8 MiB
    assert_eq!(c.effective_reserve_bytes(small_vram), RESERVE_FLOOR_BYTES);

    // 3. 10% VRAM dominance: reserve_bytes = floor, VRAM 10% > 512 MiB
    c.reserve_bytes = RESERVE_FLOOR_BYTES;
    let large_vram: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB, 10% = 1.6 GiB
    let expected_tenth = large_vram.div_ceil(10);
    assert_eq!(c.effective_reserve_bytes(large_vram), expected_tenth);

    // 4. Ceil rounding for VRAM not divisible by 10
    c.reserve_bytes = 0;
    assert_eq!(c.effective_reserve_bytes(11), RESERVE_FLOOR_BYTES); // 11 div_ceil 10 is 2, floor wins
    c.reserve_bytes = 0;
    let unaligned_vram: u64 = 10 * 1024 * 1024 * 1024 + 7; // 10% ceil is 1073741825
    assert_eq!(
        c.effective_reserve_bytes(unaligned_vram),
        unaligned_vram.div_ceil(10)
    );

    // 5. Zero VRAM
    c.reserve_bytes = 100;
    assert_eq!(c.effective_reserve_bytes(0), RESERVE_FLOOR_BYTES);

    // 6. Max u64 VRAM
    c.reserve_bytes = 0;
    assert_eq!(c.effective_reserve_bytes(u64::MAX), u64::MAX.div_ceil(10));
}

#[test]
fn example_config_parses() {
    let example = include_str!("../../winsvc.example.toml");
    let c = WinDriveConfig::from_toml(example).unwrap();
    assert!(c.size_bytes >= MIN_SIZE_BYTES);
    assert_eq!(c.queue_depth, 4);
    assert!(is_absolute_path(&c.evidence_path));
}

#[test]
fn reject_empty_tenant() {
    let bad = GOOD.replace("windrive-host", "");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "tenant",
            ..
        }
    ));
}

#[test]
fn from_reader_rejects_oversize() {
    let huge = vec![b'a'; MAX_CONFIG_BYTES + 1];
    let e = WinDriveConfig::from_reader(&huge).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "config",
            ..
        }
    ));
}

#[test]
fn from_toml_rejects_oversize_text() {
    let huge = format!("{GOOD}{}", "x".repeat(MAX_CONFIG_BYTES));
    let e = WinDriveConfig::from_toml(&huge).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "config",
            ..
        }
    ));
}

#[test]
fn from_reader_parses_good_utf8() {
    let c = WinDriveConfig::from_reader(GOOD.as_bytes()).unwrap();
    assert_eq!(c.queue_depth, 4);
    assert_eq!(c.broker_ready_timeout_secs, 30);
}

#[test]
fn from_reader_rejects_non_utf8() {
    let e = WinDriveConfig::from_reader(&[0xff, 0xfe, 0xfd]).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn from_reader_empty_buffer_fails() {
    let e = WinDriveConfig::from_reader(b"").unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn from_reader_invalid_toml_syntax_fails() {
    let bad = b"[win_drive\ninvalid_toml";
    let e = WinDriveConfig::from_reader(bad).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn from_reader_invalid_config_invariants_fails() {
    let bad = GOOD.replace("block_size = 4096", "block_size = 1234");
    let e = WinDriveConfig::from_reader(bad.as_bytes()).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "block_size",
            ..
        }
    ));
}

#[test]
fn reject_bad_block_size() {
    let bad = GOOD.replace("block_size = 4096", "block_size = 1024");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "block_size",
            ..
        }
    ));
}

#[test]
fn reject_unaligned_size() {
    let bad = GOOD.replace("size_bytes = 536870912", "size_bytes = 67108865");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "size_bytes",
            ..
        }
    ));
}

#[test]
fn reject_bad_queue_depth_not_pow2() {
    let bad = GOOD.replace("queue_depth = 4", "queue_depth = 3");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "queue_depth",
            ..
        }
    ));
}

#[test]
fn reject_zero_max_io() {
    let bad = GOOD.replace("max_io_bytes = 1048576", "max_io_bytes = 0");
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "max_io_bytes",
            ..
        }
    ));
}

#[test]
fn reject_relative_evidence_path() {
    let bad = GOOD.replace(
        r#"evidence_path = "C:\\ProgramData\\RamShared\\evidence""#,
        r#"evidence_path = "relative/evidence""#,
    );
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "evidence_path",
            ..
        }
    ));
}

#[test]
fn reject_volume_letter_a() {
    let bad = GOOD.replace(r#"volume_letter = "D""#, r#"volume_letter = "A""#);
    let e = WinDriveConfig::from_toml(&bad).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "volume_letter",
            ..
        }
    ));
}

#[test]
fn accept_private_volume_mount_path() {
    let text = GOOD.replace(
        r#"volume_letter = "D""#,
        r#"volume_letter = "D"
volume_mount_path = "C:\\ProgramData\\RamShared\\mounts\\lun-123""#,
    );
    let c = WinDriveConfig::from_toml(&text).unwrap();
    assert_eq!(
        c.volume_mount_path.as_deref(),
        Some(Path::new(r"C:\ProgramData\RamShared\mounts\lun-123"))
    );
}

#[test]
fn reject_private_mount_outside_owned_root() {
    let text = GOOD.replace(
        r#"volume_letter = "D""#,
        r#"volume_letter = "D"
volume_mount_path = "C:\\Users\\Public\\lun""#,
    );
    let e = WinDriveConfig::from_toml(&text).unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "volume_mount_path",
            ..
        }
    ));
}

#[test]
fn accept_named_pipe_v1() {
    let c = WinDriveConfig::from_toml(GOOD).unwrap();
    assert_eq!(c.broker_pipe, BrokerPipeV1::NamedPipeV1);
}

#[test]
fn reject_tcp_daily_transport() {
    let bad = GOOD.replace(r#"broker_pipe = "named_pipe_v1""#, r#"broker_pipe = "tcp""#);
    assert!(matches!(
        WinDriveConfig::from_toml(&bad),
        Err(ConfigError::Parse(_))
    ));
}

#[test]
fn reject_ready_timeout_over_30() {
    let bad = GOOD.replace(
        "broker_ready_timeout_secs = 30",
        "broker_ready_timeout_secs = 31",
    );
    assert!(matches!(
        WinDriveConfig::from_toml(&bad),
        Err(ConfigError::Invalid {
            field: "broker_ready_timeout_secs",
            ..
        })
    ));
}

#[test]
fn reject_unknown_broker_fields() {
    let bad = GOOD.replace(
        r#"broker_pipe = "named_pipe_v1""#,
        "broker_pipe = \"named_pipe_v1\"\nbroker = \"127.0.0.1:7700\"",
    );
    assert!(matches!(
        WinDriveConfig::from_toml(&bad),
        Err(ConfigError::Parse(_))
    ));
}

#[test]
fn display_errors() {
    let p = ConfigError::Parse("x".into());
    assert!(p.to_string().contains("parse"));
    let i = ConfigError::Invalid {
        field: "f",
        detail: "d".into(),
    };
    assert!(i.to_string().contains("invalid f"));
}

#[test]
fn is_absolute_accepts_native_and_windows_config_paths() {
    assert!(is_absolute_path(Path::new(r"C:\ProgramData\RamShared")));
    assert!(is_absolute_path(Path::new(r"\\?\C:\x")));
    assert_eq!(is_absolute_path(Path::new("/tmp/x")), cfg!(unix));
    assert!(!is_absolute_path(Path::new("relative")));
}

#[test]
fn evidence_path_accessor() {
    let c = WinDriveConfig::from_toml(GOOD).unwrap();
    assert_eq!(
        c.evidence_path(),
        Path::new(r"C:\ProgramData\RamShared\evidence")
    );
    assert_eq!(c.evidence_path(), c.evidence_path.as_path());

    let unc_toml = GOOD.replace(
        r#"evidence_path = "C:\\ProgramData\\RamShared\\evidence""#,
        r#"evidence_path = "\\\\server\\share\\evidence""#,
    );
    let c_unc = WinDriveConfig::from_toml(&unc_toml).unwrap();
    assert_eq!(c_unc.evidence_path(), Path::new(r"\\server\share\evidence"));

    let ext_toml = GOOD.replace(
        r#"evidence_path = "C:\\ProgramData\\RamShared\\evidence""#,
        r#"evidence_path = "\\\\?\\C:\\ProgramData\\RamShared\\evidence""#,
    );
    let c_ext = WinDriveConfig::from_toml(&ext_toml).unwrap();
    assert_eq!(
        c_ext.evidence_path(),
        Path::new(r"\\?\C:\ProgramData\RamShared\evidence")
    );

    let mut custom = c.clone();
    custom.evidence_path = PathBuf::from(r"D:\CustomEvidenceDir\log.jsonl");
    assert_eq!(
        custom.evidence_path(),
        Path::new(r"D:\CustomEvidenceDir\log.jsonl")
    );
}

fn valid_config() -> WinDriveConfig {
    WinDriveConfig {
        size_bytes: MIN_SIZE_BYTES,
        block_size: 4096,
        cuda_device: 0,
        reserve_bytes: RESERVE_FLOOR_BYTES,
        queue_depth: 4,
        max_io_bytes: 1048576,
        evidence_path: PathBuf::from(r"C:\ProgramData\RamShared\evidence"),
        volume_letter: 'D',
        volume_mount_path: None,
        broker_pipe: BrokerPipeV1::NamedPipeV1,
        broker_ready_timeout_secs: 30,
        tenant: "windrive-host".into(),
        heartbeat_secs: 5,
    }
}

#[test]
fn validate_valid_config_succeeds() {
    let c = valid_config();
    assert!(c.validate().is_ok());
}

#[test]
fn validate_block_size_512_succeeds() {
    let mut c = valid_config();
    c.block_size = 512;
    c.max_io_bytes = 512 * 1024;
    assert!(c.validate().is_ok());
}

#[test]
fn validate_size_bytes_below_min_fails() {
    let mut c = valid_config();
    c.size_bytes = MIN_SIZE_BYTES - c.block_size as u64;
    let e = c.validate().unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "size_bytes",
            ..
        }
    ));
}

#[test]
fn validate_queue_depth_zero_and_exceeds_max_fails() {
    let mut c = valid_config();
    c.queue_depth = 0;
    assert!(matches!(
        c.validate().unwrap_err(),
        ConfigError::Invalid {
            field: "queue_depth",
            ..
        }
    ));

    c.queue_depth = 512;
    assert!(matches!(
        c.validate().unwrap_err(),
        ConfigError::Invalid {
            field: "queue_depth",
            ..
        }
    ));
}

#[test]
fn validate_max_io_bytes_exceeds_cap_fails() {
    let mut c = valid_config();
    c.max_io_bytes = MAX_IO_BYTES_CAP + c.block_size;
    let e = c.validate().unwrap_err();
    assert!(matches!(
        e,
        ConfigError::Invalid {
            field: "max_io_bytes",
            ..
        }
    ));
}

#[test]
fn validate_volume_letter_cases() {
    let mut c = valid_config();
    c.volume_letter = 'd';
    assert!(c.validate().is_ok());

    c.volume_letter = 'z';
    assert!(c.validate().is_ok());

    c.volume_letter = 'C';
    assert!(matches!(
        c.validate().unwrap_err(),
        ConfigError::Invalid {
            field: "volume_letter",
            ..
        }
    ));

    c.volume_letter = '[';
    assert!(matches!(
        c.validate().unwrap_err(),
        ConfigError::Invalid {
            field: "volume_letter",
            ..
        }
    ));
}

#[test]
fn validate_volume_mount_path_invariants() {
    let mut c = valid_config();
    c.volume_mount_path = Some(PathBuf::from(r"C:\ProgramData\RamShared\mounts\..\secret"));
    assert!(matches!(
        c.validate().unwrap_err(),
        ConfigError::Invalid {
            field: "volume_mount_path",
            ..
        }
    ));

    c.volume_mount_path = Some(PathBuf::from(r"C:\ProgramData\RamShared\mounts\lun;1"));
    assert!(matches!(
        c.validate().unwrap_err(),
        ConfigError::Invalid {
            field: "volume_mount_path",
            ..
        }
    ));

    c.volume_mount_path = Some(PathBuf::from(r"C:\ProgramData\RamShared\mounts\"));
    assert!(matches!(
        c.validate().unwrap_err(),
        ConfigError::Invalid {
            field: "volume_mount_path",
            ..
        }
    ));
}

#[test]
fn validate_broker_ready_timeout_secs_boundaries() {
    let mut c = valid_config();
    c.broker_ready_timeout_secs = 0;
    assert!(matches!(
        c.validate().unwrap_err(),
        ConfigError::Invalid {
            field: "broker_ready_timeout_secs",
            ..
        }
    ));

    c.broker_ready_timeout_secs = 1;
    assert!(c.validate().is_ok());

    c.broker_ready_timeout_secs = 30;
    assert!(c.validate().is_ok());
}

#[test]
fn test_from_toml_lowercase_volume_letter_accepted() {
    let text = GOOD.replace(r#"volume_letter = "D""#, r#"volume_letter = "d""#);
    let c = WinDriveConfig::from_toml(&text).unwrap();
    assert_eq!(c.volume_letter, 'd');
}

#[test]
fn test_from_toml_block_size_512_accepted() {
    let text = GOOD.replace("block_size = 4096", "block_size = 512");
    let c = WinDriveConfig::from_toml(&text).unwrap();
    assert_eq!(c.block_size, 512);
}

#[test]
fn test_from_toml_missing_win_drive_section_fails() {
    let bad = r#"
[other_drive]
size_bytes = 536870912
"#;
    let e = WinDriveConfig::from_toml(bad).unwrap_err();
    assert!(matches!(e, ConfigError::Parse(_)));
}

#[test]
fn test_from_toml_ready_timeout_boundary_validation() {
    let bad_zero = GOOD.replace(
        "broker_ready_timeout_secs = 30",
        "broker_ready_timeout_secs = 0",
    );
    assert!(matches!(
        WinDriveConfig::from_toml(&bad_zero),
        Err(ConfigError::Invalid {
            field: "broker_ready_timeout_secs",
            ..
        })
    ));

    let good_one = GOOD.replace(
        "broker_ready_timeout_secs = 30",
        "broker_ready_timeout_secs = 1",
    );
    let c = WinDriveConfig::from_toml(&good_one).unwrap();
    assert_eq!(c.broker_ready_timeout_secs, 1);
}

#[test]
fn test_from_toml_queue_depth_exceeds_max_fails() {
    let bad = GOOD.replace("queue_depth = 4", "queue_depth = 512");
    assert!(matches!(
        WinDriveConfig::from_toml(&bad),
        Err(ConfigError::Invalid {
            field: "queue_depth",
            ..
        })
    ));
}

#[test]
fn test_from_toml_max_io_exceeds_cap_fails() {
    let bad = GOOD.replace("max_io_bytes = 1048576", "max_io_bytes = 1052672");
    assert!(matches!(
        WinDriveConfig::from_toml(&bad),
        Err(ConfigError::Invalid {
            field: "max_io_bytes",
            ..
        })
    ));
}

#[test]
fn test_from_toml_invalid_mount_paths() {
    let text_exact_prefix = GOOD.replace(
        r#"volume_letter = "D""#,
        r#"volume_letter = "D"
volume_mount_path = "C:\\ProgramData\\RamShared\\mounts\\""#,
    );
    assert!(matches!(
        WinDriveConfig::from_toml(&text_exact_prefix),
        Err(ConfigError::Invalid {
            field: "volume_mount_path",
            ..
        })
    ));

    let text_dotdot = GOOD.replace(
        r#"volume_letter = "D""#,
        r#"volume_letter = "D"
volume_mount_path = "C:\\ProgramData\\RamShared\\mounts\\..\\secret""#,
    );
    assert!(matches!(
        WinDriveConfig::from_toml(&text_dotdot),
        Err(ConfigError::Invalid {
            field: "volume_mount_path",
            ..
        })
    ));
}
