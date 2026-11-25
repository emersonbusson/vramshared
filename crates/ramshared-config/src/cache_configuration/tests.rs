// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-config/src/cache_configuration/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
const GIB: u64 = 1 << 30;
fn member(index: u8, capacity: u64, target: u64) -> CacheMemberConfiguration {
    CacheMemberConfiguration {
        adapter: GpuAdapterIdentity {
            backend: "cuda".into(),
            key: format!("{index:032x}"),
            luid: None,
            node_mask: None,
        },
        capacity_bytes: capacity,
        target_bytes: target,
        reserve_bytes: ReserveFloorPolicy::sealed().configured_reserve_bytes(capacity),
        runtime_buffer_bytes: RUNTIME_FREE_BUFFER_BYTES,
    }
}
fn configuration() -> CacheConfiguration {
    let mut origin = ResourceTarget::LinuxFileOrigin {
        filesystem_uuid: "fs".into(),
        device_identity: "device".into(),
        managed_relative_path: "origin/cache.img".into(),
        inode: 42,
        allocated_bytes: 256 * GIB,
        identity_field_hash: String::new(),
    };
    let hash = identity_hash(&origin).unwrap();
    if let ResourceTarget::LinuxFileOrigin {
        identity_field_hash,
        ..
    } = &mut origin
    {
        *identity_field_hash = hash;
    }
    CacheConfiguration {
        schema_version: 1,
        generation: 1,
        plan_fingerprint: "a".repeat(64),
        approval_id: "approval-1".into(),
        approval_issued_unix: 1000,
        applied_unix: 1001,
        profile_fingerprint: "b".repeat(64),
        release_version: "installed-release".into(),
        daemon_executable: "/opt/ramshared/releases/test/bin/ramsharedd".into(),
        daemon_sha256: "c".repeat(64),
        origin,
        lower_sink_identity: "known-sink".into(),
        required_lower_sink_bytes: GIB,
        members: vec![member(1, 6 * GIB, 2 * GIB)],
    }
}
#[test]
fn cache_configuration_accepts_dynamic_independent_members() {
    for capacity in [6, 8, 10, 12, 20, 32, 96, 192] {
        let mut policy = configuration();
        policy.members = vec![
            member(1, capacity * GIB, GIB),
            member(2, capacity * GIB, 2 * GIB),
        ];
        assert_eq!(policy.validate().unwrap(), 3 * GIB);
        let text = policy.encode().unwrap();
        assert_eq!(CacheConfiguration::parse(&text).unwrap(), policy);
    }
    assert_eq!(
        CacheConfiguration::next_generation(None, "first", 1).unwrap(),
        1
    );
    assert_eq!(
        CacheConfiguration::next_generation(Some(&configuration()), "second", 1002).unwrap(),
        2
    );
    assert_eq!(
        configuration_path().file_name().unwrap(),
        CONFIGURATION_NAME
    );
    assert!(lease_path().to_string_lossy().starts_with('/'));
}

#[test]
fn cache_configuration_refuses_a_truncated_complete_member_prefix() {
    let mut policy = configuration();
    policy.members.push(member(2, 96 * GIB, 8 * GIB));
    let text = policy.encode().unwrap();
    let last = text.rfind("[[configuration.members]]").unwrap();
    assert!(
        CacheConfiguration::parse(&text[..last]).is_err(),
        "a partial write must not authorize only a prefix of the approved GPU set"
    );
}
#[test]
fn cache_configuration_refuses_invalid_identity_geometry_and_replay() {
    for change in 0..21 {
        let mut p = configuration();
        match change {
            0 => p.schema_version = 0,
            1 => p.generation = 0,
            2 => p.members.clear(),
            3 => p.plan_fingerprint = "x".repeat(64),
            4 => p.approval_id.clear(),
            5 => p.release_version = "bad\nrelease".into(),
            6 => p.approval_issued_unix = 0,
            7 => p.applied_unix = 999,
            8 => p.daemon_executable = "relative".into(),
            9 => p.required_lower_sink_bytes = 0,
            10 => p.members[0].adapter.key = "0".repeat(32),
            11 => p.members[0].capacity_bytes = 0,
            12 => p.members[0].target_bytes = 0,
            13 => p.members[0].reserve_bytes = 0,
            14 => p.members[0].runtime_buffer_bytes = 0,
            15 => p.members[0].target_bytes = u64::MAX,
            16 => p.members.push(p.members[0].clone()),
            17 => {
                p.members.push(member(2, 6 * GIB, GIB));
                p.members.reverse();
            }
            18 => {
                if let ResourceTarget::LinuxFileOrigin { inode, .. } = &mut p.origin {
                    *inode += 1;
                }
            }
            19 => {
                p.origin = ResourceTarget::LinuxFileOriginRequest {
                    filesystem_uuid: "fs".into(),
                    device_identity: "device".into(),
                    managed_relative_path: "origin/cache.img".into(),
                    allocated_bytes: GIB,
                }
            }
            _ => p.members = vec![member(1, u64::MAX, u64::MAX / 2)],
        }
        assert!(p.validate().is_err(), "invalid mutation {change}");
        assert!(p.encode().is_err());
    }
    let p = configuration();
    assert!(
        CacheConfiguration::parse(&("unknown = true\n".to_string() + &p.encode().unwrap()))
            .is_err()
    );
    assert!(CacheConfiguration::parse(&"x".repeat(MAX_CONFIGURATION_BYTES + 1)).is_err());
    assert!(CacheConfiguration::parse("").is_err());
    let mut large = p.clone();
    large.approval_id = "x".repeat(MAX_CONFIGURATION_BYTES);
    assert!(large.encode().is_err());
    for (id, issued) in [("approval-1", 1002), ("second", 1000), ("second", 999)] {
        assert!(CacheConfiguration::next_generation(Some(&p), id, issued).is_err());
    }
    let mut exhausted = p;
    exhausted.generation = u64::MAX;
    assert!(CacheConfiguration::next_generation(Some(&exhausted), "second", 1002).is_err());
}
