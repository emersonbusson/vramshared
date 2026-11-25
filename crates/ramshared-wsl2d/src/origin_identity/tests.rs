// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/origin_identity/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::{process_fd_path, root_mount_source, validate_stable_critical_device_snapshot};
use std::fs::File;
use std::os::unix::fs::MetadataExt;

#[test]
fn origin_manifest_fd_identity_pairs_refusal_with_legitimate_fixture() {
    let text = format!(
        "schema_version=3\n\
             host_manifest_sha256={}\n\
             configuration_sha256={}\n\
             origin_path=/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555\n\
             partuuid=11111111-2222-4333-8444-555555555555\n\
             ptuuid=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee\n\
             partition_dev_t=43:1\n\
             parent_dev_t=43:0\n\
             expected_swap_uuid=99999999-8888-4777-8666-555555555555\n\
             swap_type=swap\n\
             logical_capacity_mib=4096\n\
             physical_cache_cap_mib=1024\n",
        "a".repeat(64),
        "b".repeat(64)
    );
    let manifest = parse_sealed_origin_manifest(&text).unwrap();
    let legitimate = OriginIdentityObservation {
        partuuid: manifest.partuuid.clone(),
        ptuuid: manifest.ptuuid.clone(),
        path_dev_t: manifest.partition_dev_t.clone(),
        fd_dev_t: manifest.partition_dev_t.clone(),
        parent_dev_t: manifest.parent_dev_t.clone(),
        swap_uuid: manifest.expected_swap_uuid.clone(),
        swap_type: "swap".into(),
        origin_size: 25 * GIB,
        critical_dev_ts: vec!["8:0".into(), "8:1".into()],
    };
    validate_origin_manifest_identity(&manifest, &legitimate, 4 * GIB).unwrap();
    validate_stable_critical_device_snapshot(
        &["8:1".into(), "8:0".into(), "8:1".into()],
        &["8:0".into(), "8:1".into()],
    )
    .expect("equivalent critical-device snapshots");
    assert!(
        validate_stable_critical_device_snapshot(
            &["8:0".into(), "8:1".into()],
            &["8:0".into(), "8:2".into()],
        )
        .is_err()
    );
    let null = File::open("/dev/null").expect("open fd-path fixture");
    let fd_path = process_fd_path(&null);
    let named = null.metadata().expect("stat fd-path fixture");
    let through_proc = std::fs::metadata(&fd_path).expect("stat process fd path");
    assert_eq!(named.dev(), through_proc.dev());
    assert_eq!(named.ino(), through_proc.ino());

    for mutated in [
        OriginIdentityObservation {
            fd_dev_t: "43:2".into(),
            ..legitimate.clone()
        },
        OriginIdentityObservation {
            ptuuid: "aaaaaaaa-bbbb-4ccc-8ddd-cccccccccccc".into(),
            ..legitimate.clone()
        },
        OriginIdentityObservation {
            swap_uuid: "99999999-8888-4777-8666-444444444444".into(),
            ..legitimate.clone()
        },
        OriginIdentityObservation {
            critical_dev_ts: vec!["43:1".into()],
            ..legitimate.clone()
        },
        OriginIdentityObservation {
            critical_dev_ts: vec!["43:0".into()],
            ..legitimate.clone()
        },
        OriginIdentityObservation {
            origin_size: GIB,
            ..legitimate.clone()
        },
    ] {
        assert!(validate_origin_manifest_identity(&manifest, &mutated, 4 * GIB).is_err());
    }
    assert!(validate_origin_manifest_identity(&manifest, &legitimate, 8 * GIB).is_err());
    assert_eq!(
        root_mount_source(
            "36 25 8:2 / / rw,relatime - ext4 /dev/sdb2 rw,discard,errors=remount-ro\n"
        ),
        Some("/dev/sdb2")
    );
    assert!(parse_sealed_origin_manifest(&(text + "unknown=value\n")).is_err());
}

#[test]
fn sealed_origin_manifest_parser_rejects_noncanonical_and_out_of_policy_inputs() {
    let host_hash = "a".repeat(64);
    let configuration_hash = "b".repeat(64);
    let valid = format!(
        "schema_version=3\n\
             host_manifest_sha256={}\n\
             configuration_sha256={}\n\
             origin_path=/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555\n\
             partuuid=11111111-2222-4333-8444-555555555555\n\
             ptuuid=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee\n\
             partition_dev_t=43:1\n\
             parent_dev_t=43:0\n\
             expected_swap_uuid=99999999-8888-4777-8666-555555555555\n\
             swap_type=swap\n\
             logical_capacity_mib=4096\n\
             physical_cache_cap_mib=1024\n",
        host_hash, configuration_hash
    );
    assert!(parse_sealed_origin_manifest(&valid).is_ok());

    let invalid = [
        format!("{valid}malformed-line\n"),
        valid.replace("schema_version=3\n", " schema_version=3\n"),
        valid.replace("schema_version=3\n", "schema_version=3\nschema_version=3\n"),
        valid.replace("schema_version=3", "schema_version=2"),
        valid.replace("swap_type=swap", "swap_type=ext4"),
        valid.replace("swap_type=swap", "swap_type="),
        valid.replace(
            "partuuid=11111111-2222-4333-8444-555555555555",
            "partuuid=invalid",
        ),
        valid.replace(
            "ptuuid=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            "ptuuid=invalid",
        ),
        valid.replace(
            "expected_swap_uuid=99999999-8888-4777-8666-555555555555",
            "expected_swap_uuid=invalid",
        ),
        valid.replace(
            "origin_path=/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555",
            "origin_path=/dev/sda1",
        ),
        valid.replace("logical_capacity_mib=4096", "logical_capacity_mib=bad"),
        valid.replace("logical_capacity_mib=4096", "logical_capacity_mib=1023"),
        valid.replace("logical_capacity_mib=4096", "logical_capacity_mib=24577"),
        valid.replace("physical_cache_cap_mib=1024", "physical_cache_cap_mib=512"),
        valid.replace(
            "physical_cache_cap_mib=1024",
            "physical_cache_cap_mib=18446744073709551615",
        ),
        valid.replace(host_hash.as_str(), "bad-hash"),
        valid.replace(configuration_hash.as_str(), "bad-hash"),
        valid.replace("partition_dev_t=43:1", "partition_dev_t=bad"),
        valid.replace("parent_dev_t=43:0", "parent_dev_t=bad"),
        valid.replace("parent_dev_t=43:0\n", ""),
    ];
    for text in invalid {
        assert!(
            parse_sealed_origin_manifest(&text).is_err(),
            "invalid sealed manifest was accepted: {text:?}"
        );
    }
}

/// DT-7 check (a), ITEM-5: the seal rejects a manifest whose reserve
/// fields disagree with the verifier's sealed literals. The rejection is
/// the integrity proof — no tautological self-comparison.
#[test]
fn manifest_seal_rejects_reserve_mismatch() {
    let make_host = |min_mib: u64, percent: u64| {
        let mut host = HostOriginManifest {
            schema_version: 3,
            origin_vhdx: "I:\\RamShared\\ramshared-origin.vhdx".into(),
            fixed_size_bytes: 25 * GIB,
            logical_capacity_mib: 4096,
            physical_cache_cap_mib: 1024,
            chunk_mib: 128,
            gpu_reserve_min_mib: min_mib,
            gpu_reserve_percent: percent,
            partuuid: "11111111-2222-4333-8444-555555555555".into(),
            disk_guid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            expected_swap_uuid: "99999999-8888-4777-8666-555555555555".into(),
            ownership_proof_schema: 1,
            existing_wsl_swap_vhdx: "R:\\wsl_swap\\swap.vhdx".into(),
            configuration_sha256: String::new(),
        };
        host.configuration_sha256 = sha256_hex(host_configuration_text(&host).as_bytes());
        let bytes = serde_json::to_vec(&host).expect("serialize fixture");
        let sealed = SealedOriginManifest {
            host_manifest_sha256: sha256_hex(&bytes),
            configuration_sha256: host.configuration_sha256.clone(),
            origin_path: "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555".into(),
            partuuid: host.partuuid.clone(),
            ptuuid: host.disk_guid.clone(),
            partition_dev_t: "43:1".into(),
            parent_dev_t: "43:0".into(),
            expected_swap_uuid: host.expected_swap_uuid.clone(),
            logical_capacity_mib: host.logical_capacity_mib,
            physical_cache_cap_mib: host.physical_cache_cap_mib,
        };
        (sealed, bytes)
    };

    // Matching seal is accepted (refusal plus pass).
    let (sealed, bytes) = make_host(SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT);
    validate_host_origin_manifest_bytes(&sealed, &bytes).expect("sealed literals match");

    // A lower minimum is rejected even though the configuration hash is
    // internally consistent — the seal constants, not the manifest's own
    // hash, are the authority.
    let (sealed_low, bytes_low) = make_host(128, SEALED_RESERVE_PERCENT);
    assert!(validate_host_origin_manifest_bytes(&sealed_low, &bytes_low).is_err());

    // A percentage below the 20% safety floor is rejected.
    let (sealed_pct, bytes_pct) = make_host(SEALED_RESERVE_MIN_MIB, 5);
    assert!(validate_host_origin_manifest_bytes(&sealed_pct, &bytes_pct).is_err());
}

#[test]
fn host_manifest_hash_fields_are_enforced_end_to_end() {
    let mut host = HostOriginManifest {
        schema_version: 3,
        origin_vhdx: "I:\\RamShared\\ramshared-origin.vhdx".into(),
        fixed_size_bytes: 25 * GIB,
        logical_capacity_mib: 4096,
        physical_cache_cap_mib: 1024,
        chunk_mib: 128,
        gpu_reserve_min_mib: 2048,
        gpu_reserve_percent: 20,
        partuuid: "11111111-2222-4333-8444-555555555555".into(),
        disk_guid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        expected_swap_uuid: "99999999-8888-4777-8666-555555555555".into(),
        ownership_proof_schema: 1,
        existing_wsl_swap_vhdx: "R:\\wsl_swap\\swap.vhdx".into(),
        configuration_sha256: String::new(),
    };
    host.configuration_sha256 = sha256_hex(host_configuration_text(&host).as_bytes());
    let bytes = serde_json::to_vec(&host).expect("serialize host origin manifest fixture");
    let sealed = SealedOriginManifest {
        host_manifest_sha256: sha256_hex(&bytes),
        configuration_sha256: host.configuration_sha256.clone(),
        origin_path: "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555".into(),
        partuuid: host.partuuid.clone(),
        ptuuid: host.disk_guid.clone(),
        partition_dev_t: "43:1".into(),
        parent_dev_t: "43:0".into(),
        expected_swap_uuid: host.expected_swap_uuid.clone(),
        logical_capacity_mib: host.logical_capacity_mib,
        physical_cache_cap_mib: host.physical_cache_cap_mib,
    };
    validate_host_origin_manifest_bytes(&sealed, &bytes)
        .expect("matching host and guest manifests");

    let mut one_byte_tamper = bytes.clone();
    let drive = one_byte_tamper
        .windows(3)
        .position(|window| window == b"I:\\")
        .expect("locate origin drive fixture");
    one_byte_tamper[drive] = b'J';
    assert!(validate_host_origin_manifest_bytes(&sealed, &one_byte_tamper).is_err());

    let mut resealed_raw_only = sealed.clone();
    resealed_raw_only.host_manifest_sha256 = sha256_hex(&one_byte_tamper);
    let error = validate_host_origin_manifest_bytes(&resealed_raw_only, &one_byte_tamper)
        .expect_err("raw hash alone cannot bypass the canonical configuration hash");
    assert!(error.contains("configuration SHA-256"), "{error}");

    host.configuration_sha256 = "f".repeat(64);
    let wrong_configuration =
        serde_json::to_vec(&host).expect("serialize wrong configuration fixture");
    let mut resealed_wrong_configuration = sealed;
    resealed_wrong_configuration.host_manifest_sha256 = sha256_hex(&wrong_configuration);
    assert!(
        validate_host_origin_manifest_bytes(&resealed_wrong_configuration, &wrong_configuration)
            .is_err()
    );
}

#[test]
fn host_manifest_accepts_five_gib_and_rejects_under_capacity() {
    let mut host = HostOriginManifest {
        schema_version: 3,
        origin_vhdx: "I:\\RamShared\\ramshared-origin.vhdx".into(),
        fixed_size_bytes: 5 * GIB,
        logical_capacity_mib: 4096,
        physical_cache_cap_mib: 1024,
        chunk_mib: 128,
        gpu_reserve_min_mib: 2048,
        gpu_reserve_percent: 20,
        partuuid: "11111111-2222-4333-8444-555555555555".into(),
        disk_guid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        expected_swap_uuid: "99999999-8888-4777-8666-555555555555".into(),
        ownership_proof_schema: 1,
        existing_wsl_swap_vhdx: "R:\\wsl_swap\\swap.vhdx".into(),
        configuration_sha256: String::new(),
    };
    host.configuration_sha256 = sha256_hex(host_configuration_text(&host).as_bytes());
    let bytes = serde_json::to_vec(&host).expect("serialize host origin manifest fixture");
    let sealed = SealedOriginManifest {
        host_manifest_sha256: sha256_hex(&bytes),
        configuration_sha256: host.configuration_sha256.clone(),
        origin_path: "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555".into(),
        partuuid: host.partuuid.clone(),
        ptuuid: host.disk_guid.clone(),
        partition_dev_t: "43:1".into(),
        parent_dev_t: "43:0".into(),
        expected_swap_uuid: host.expected_swap_uuid.clone(),
        logical_capacity_mib: host.logical_capacity_mib,
        physical_cache_cap_mib: host.physical_cache_cap_mib,
    };
    // 5 GiB fixed container with 4096 MiB swap must be accepted
    validate_host_origin_manifest_bytes(&sealed, &bytes)
        .expect("5 GiB container must be accepted for 4096 MiB logical swap");

    // 25 GiB fixed container with 4096 MiB swap must remain accepted (backward compatibility)
    let mut legacy_host = host.clone();
    legacy_host.fixed_size_bytes = 25 * GIB;
    legacy_host.configuration_sha256 = sha256_hex(host_configuration_text(&legacy_host).as_bytes());
    let legacy_bytes =
        serde_json::to_vec(&legacy_host).expect("serialize legacy host origin manifest fixture");
    let mut legacy_sealed = sealed.clone();
    legacy_sealed.host_manifest_sha256 = sha256_hex(&legacy_bytes);
    legacy_sealed.configuration_sha256 = legacy_host.configuration_sha256.clone();
    validate_host_origin_manifest_bytes(&legacy_sealed, &legacy_bytes)
        .expect("25 GiB legacy container must be accepted");

    // Under-capacity: 5 GiB container with 8192 MiB logical swap must be rejected
    let mut undersized_host = host.clone();
    undersized_host.logical_capacity_mib = 8192;
    undersized_host.configuration_sha256 =
        sha256_hex(host_configuration_text(&undersized_host).as_bytes());
    let undersized_bytes =
        serde_json::to_vec(&undersized_host).expect("serialize undersized fixture");
    let mut undersized_sealed = sealed.clone();
    undersized_sealed.logical_capacity_mib = 8192;
    undersized_sealed.host_manifest_sha256 = sha256_hex(&undersized_bytes);
    undersized_sealed.configuration_sha256 = undersized_host.configuration_sha256.clone();
    assert!(
        validate_host_origin_manifest_bytes(&undersized_sealed, &undersized_bytes).is_err(),
        "5 GiB container cannot host 8192 MiB swap"
    );
}

#[test]
fn physical_cache_cap_is_explicit_bounded_and_defaults_to_one_gib() {
    assert_eq!(parse_physical_cache_cap(None, 4 * GIB).unwrap(), GIB);
    assert_eq!(parse_physical_cache_cap(None, GIB).unwrap(), GIB);
    assert!(parse_physical_cache_cap(None, GIB - 1).is_err());
    assert_eq!(
        parse_physical_cache_cap(Some("1024"), 4 * GIB).unwrap(),
        GIB
    );
    assert!(parse_physical_cache_cap(Some("0"), 4 * GIB).is_err());
    assert!(parse_physical_cache_cap(Some("4097"), 4 * GIB).is_err());
    assert!(parse_physical_cache_cap(Some("invalid"), 4 * GIB).is_err());
}

#[test]
fn origin_cache_cap_and_root_device_policy_cover_boundaries() {
    assert_eq!(parse_physical_cache_cap(None, 4 * GIB).unwrap(), GIB);
    assert_eq!(
        parse_physical_cache_cap(Some("2048"), 4 * GIB).unwrap(),
        2 * GIB
    );
    for invalid in ["bad", "1023", "4097", "18446744073709551615"] {
        assert!(parse_physical_cache_cap(Some(invalid), 4 * GIB).is_err());
    }

    let mountinfo = "36 25 8:1 / / rw - ext4 /dev/sda1 rw\n".to_owned()
        + "37 25 0:44 / /home rw - ext4 /dev/mapper/home rw\n";
    assert_eq!(root_mount_source(&mountinfo), Some("/dev/sda1"));
    assert_eq!(root_mount_source("36 25 8:1 / / rw - tmpfs tmpfs rw"), None);
    assert_eq!(root_mount_source("malformed mountinfo"), None);
    assert!(validate_stable_critical_device_snapshot(&["8:1".into()], &["8:1".into()]).is_ok());
    assert!(validate_stable_critical_device_snapshot(&["8:1".into()], &["8:2".into()]).is_err());
}

#[test]
fn host_manifest_refuses_overflowing_capacity_without_panicking() {
    let mut host = HostOriginManifest {
        schema_version: 3,
        origin_vhdx: "I:\\RamShared\\ramshared-origin.vhdx".into(),
        fixed_size_bytes: 5 * GIB,
        logical_capacity_mib: 4096,
        physical_cache_cap_mib: 1024,
        chunk_mib: 128,
        gpu_reserve_min_mib: 2048,
        gpu_reserve_percent: 20,
        partuuid: "11111111-2222-4333-8444-555555555555".into(),
        disk_guid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        expected_swap_uuid: "99999999-8888-4777-8666-555555555555".into(),
        ownership_proof_schema: 1,
        existing_wsl_swap_vhdx: "R:\\wsl_swap\\swap.vhdx".into(),
        configuration_sha256: String::new(),
    };
    host.configuration_sha256 = sha256_hex(host_configuration_text(&host).as_bytes());
    let bytes = serde_json::to_vec(&host).expect("serialize host origin manifest fixture");
    let sealed = SealedOriginManifest {
        host_manifest_sha256: sha256_hex(&bytes),
        configuration_sha256: host.configuration_sha256.clone(),
        origin_path: "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555".into(),
        partuuid: host.partuuid.clone(),
        ptuuid: host.disk_guid.clone(),
        partition_dev_t: "43:1".into(),
        parent_dev_t: "43:0".into(),
        expected_swap_uuid: host.expected_swap_uuid.clone(),
        logical_capacity_mib: host.logical_capacity_mib,
        physical_cache_cap_mib: host.physical_cache_cap_mib,
    };

    validate_host_origin_manifest_bytes(&sealed, &bytes).unwrap();
    for logical_capacity_mib in [u64::MAX, u64::MAX - 1024, u64::MAX / (1024 * 1024)] {
        host.logical_capacity_mib = logical_capacity_mib;
        host.configuration_sha256 = sha256_hex(host_configuration_text(&host).as_bytes());
        let bytes = serde_json::to_vec(&host).unwrap();
        let mut sealed = sealed.clone();
        sealed.host_manifest_sha256 = sha256_hex(&bytes);
        sealed.configuration_sha256 = host.configuration_sha256.clone();
        assert!(validate_host_origin_manifest_bytes(&sealed, &bytes).is_err());
    }
}

#[test]
fn sealed_manifest_readers_refuse_unowned_locations() {
    assert!(read_host_origin_manifest_bytes("/tmp/not-the-host-manifest").is_err());
    assert!(read_sealed_origin_manifest("/tmp/not-the-origin-manifest").is_err());
}

#[test]
#[ignore = "requires root and an installed schema-3 host/guest origin seal; read-only"]
fn installed_sealed_origin_matches_host_without_effects() {
    assert!(rustix::process::geteuid().is_root());
    let guest_before = read_bounded_manifest_file(Path::new(ORIGIN_MANIFEST_PATH), true).unwrap();
    let host_before = read_host_origin_manifest_bytes(HOST_ORIGIN_MANIFEST_PATH).unwrap();
    let swaps_text = std::fs::read_to_string("/proc/swaps").unwrap();
    let swaps_before = swap_topology(&swaps_text).unwrap();

    let sealed = read_sealed_origin_manifest(ORIGIN_MANIFEST_PATH).unwrap();
    let mut changed = sealed.clone();
    changed.logical_capacity_mib += 1;
    assert!(validate_host_origin_manifest_bytes(&changed, &host_before).is_err());
    validate_host_origin_manifest_bytes(&sealed, &host_before).unwrap();

    assert_eq!(
        read_bounded_manifest_file(Path::new(ORIGIN_MANIFEST_PATH), true).unwrap(),
        guest_before
    );
    assert_eq!(
        read_host_origin_manifest_bytes(HOST_ORIGIN_MANIFEST_PATH).unwrap(),
        host_before
    );
    assert_eq!(
        swap_topology(&std::fs::read_to_string("/proc/swaps").unwrap()).unwrap(),
        swaps_before
    );
    println!(
        "PASS_INSTALLED_SEAL_READ_ONLY guest_sha256={} host_sha256={} swap_topology_sha256={} logical_mib={} cache_cap_mib={}",
        sha256_hex(&guest_before),
        sha256_hex(&host_before),
        sha256_hex(&serde_json::to_vec(&swaps_before).unwrap()),
        sealed.logical_capacity_mib,
        sealed.physical_cache_cap_mib,
    );
}

// Occupancy changes independently under load. Validate every row with the
// runtime parser, then compare the paths, types, sizes and priorities we own.
fn swap_topology(text: &str) -> Result<Vec<[&str; 4]>, String> {
    crate::parse_strict_proc_swaps(text)?;
    let mut rows: Vec<_> = text
        .lines()
        .skip(1)
        .map(|line| {
            let columns: Vec<_> = line.split_whitespace().collect();
            [columns[0], columns[1], columns[2], columns[4]]
        })
        .collect();
    rows.sort_unstable();
    Ok(rows)
}

#[test]
fn installed_swap_observer_accepts_occupancy_changes_and_detects_topology_drift() {
    let text = "Filename Type Size Used Priority\n/dev/nbd0 partition 8192 64 100\n";
    let original = swap_topology(text).unwrap();
    assert_eq!(
        swap_topology(&text.replace("8192 64", "8192 128")).unwrap(),
        original
    );
    for changed in [
        text.replace("/dev/nbd0", "/dev/nbd1"),
        text.replace("partition", "file"),
        text.replace("8192", "16384"),
        text.replace("100", "90"),
        "Filename Type Size Used Priority\n".into(),
    ] {
        assert_ne!(swap_topology(&changed).unwrap(), original);
    }
    assert!(swap_topology("not a swap table").is_err());
}
