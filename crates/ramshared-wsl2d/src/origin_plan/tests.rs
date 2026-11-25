// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/origin_plan/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use ramshared_vram::GpuAdapterIdentity;

fn member(key: u8, target: u64) -> WorkerSpawnParams {
    WorkerSpawnParams {
        generation: None,
        target_bytes: target,
        chunk_bytes: 4096,
        reserve_floor: 2 << 30,
        expected_adapter: Some(GpuAdapterIdentity {
            backend: "cuda".into(),
            key: format!("{key:032x}"),
            luid: None,
            node_mask: None,
        }),
        host_catalog: None,
    }
}
fn plan(members: Vec<WorkerSpawnParams>) -> WorkerPlan {
    WorkerPlan {
        chunk_bytes: 4096,
        members,
    }
}

#[test]
fn canonical_members_preserve_arbitrary_capacities_and_node_masks() {
    let (_, heterogeneous) = plan(vec![member(2, 96 << 30), member(1, 32 << 30)])
        .prepare(128 << 30, 2 << 30)
        .unwrap()
        .into_parts();
    assert_eq!(heterogeneous[0].target_bytes, 32 << 30);
    assert_eq!(heterogeneous[1].target_bytes, 96 << 30);
    for gib in [6, 8, 10, 12, 20, 32, 96, 192, 256] {
        let target = (gib as u64) << 29;
        let mut second = member(11, target);
        let id = second.expected_adapter.as_mut().unwrap();
        id.key.make_ascii_uppercase();
        id.luid = Some("00000001:0000000A".into());
        id.node_mask = Some(2);
        let (_, members) = plan(vec![second, member(10, target)])
            .prepare(target * 2, 2 << 30)
            .unwrap()
            .into_parts();
        assert_eq!(members.len(), 2);
        assert_eq!(
            members.iter().map(|p| p.target_bytes).sum::<u64>(),
            target * 2
        );
        assert_eq!(
            members[0].expected_adapter.as_ref().unwrap().key,
            format!("{:032x}", 10)
        );
        assert_eq!(
            members[1]
                .expected_adapter
                .as_ref()
                .unwrap()
                .luid
                .as_deref(),
            Some("00000001:0000000a")
        );
        assert_eq!(
            members[1].expected_adapter.as_ref().unwrap().node_mask,
            Some(2)
        );
    }
    let (chunk, members) = WorkerPlan::automatic(4096, 4096, 2 << 30)
        .prepare(4096, 2 << 30)
        .unwrap()
        .into_parts();
    assert_eq!(chunk, 4096);
    assert_eq!(member_key(&members[0]), "automatic");
}

#[test]
fn invalid_member_geometry_sums_and_metadata_refuse_before_construction() {
    let good = member(1, 4096);
    let mut cases = vec![
        plan(vec![]),
        plan(vec![member(1, 0)]),
        plan(vec![member(1, u64::MAX), member(2, 1)]),
    ];
    let mut bad = good.clone();
    bad.chunk_bytes = 0;
    cases.push(plan(vec![bad]));
    let mut bad = good.clone();
    bad.reserve_floor = 0;
    cases.push(plan(vec![bad]));
    let mut bad = plan(vec![good.clone()]);
    bad.chunk_bytes = 0;
    cases.push(bad);
    for case in cases {
        assert!(case.prepare(u64::MAX, 2 << 30).is_err());
    }
    assert!(plan(vec![good.clone()]).prepare(4095, 2 << 30).is_err());
    let count = MAX_RESOURCE_PROFILE_BYTES / std::mem::size_of::<WorkerSpawnParams>() + 1;
    let error = plan(vec![good; count])
        .prepare(u64::MAX, 2 << 30)
        .err()
        .unwrap();
    assert!(error.contains("metadata"));
}

#[test]
fn configured_members_refuse_aliases_unpinned_and_ambiguous_api_pairs() {
    let first = member(10, 4096);
    let mut alias = first.clone();
    alias
        .expected_adapter
        .as_mut()
        .unwrap()
        .key
        .make_ascii_uppercase();
    assert!(
        plan(vec![first.clone(), alias])
            .prepare(8192, 2 << 30)
            .is_err()
    );
    let mut unpinned = member(2, 4096);
    unpinned.expected_adapter = None;
    assert!(
        plan(vec![first.clone(), unpinned])
            .prepare(8192, 2 << 30)
            .is_err()
    );
    let mut invalid = first.clone();
    invalid.expected_adapter.as_mut().unwrap().key = "ordinal:0".into();
    assert!(plan(vec![invalid]).prepare(8192, 2 << 30).is_err());
    let mut other = member(2, 4096);
    other.expected_adapter.as_mut().unwrap().backend = "vulkan".into();
    assert!(
        plan(vec![first.clone(), other.clone()])
            .prepare(8192, 2 << 30)
            .is_err()
    );
    let mut known = first;
    known.expected_adapter.as_mut().unwrap().luid = Some("00000001:00000001".into());
    other.expected_adapter.as_mut().unwrap().luid = Some("00000002:00000001".into());
    assert!(plan(vec![known, other]).prepare(8192, 2 << 30).is_ok());
}
