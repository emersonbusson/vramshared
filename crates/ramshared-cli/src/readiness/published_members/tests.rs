// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/published_members/tests.rs
//! Budget-set regressions; all driver observations are explicitly synthetic.
#![allow(clippy::unwrap_used)]
use super::*;
use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSource, GpuBudgetTelemetry};

fn member(key: &str) -> CacheMemberStatus {
    let adapter = GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key: key.into(),
        luid: None,
    };
    CacheMemberStatus {
        key: key.into(),
        state: CacheState::Active,
        requested_target_bytes: 4096,
        cached_bytes: Some(0),
        failures: 0,
        adapter: Some(adapter.clone()),
        budget: Some(GpuBudgetTelemetry {
            schema_version: 1,
            adapter: Some(adapter),
            total_bytes: Some(8 << 30),
            budget_bytes: 8 << 30,
            used_bytes: 0,
            available_bytes: 8 << 30,
            source: GpuBudgetSource::DriverReported,
            sampled_at_unix_ms: 1000,
        }),
        utilization: None,
        cache: None,
    }
}

#[test]
fn member_records_preserve_each_budget_and_do_not_require_a_singular_sample() {
    let a = member("00112233445566778899aabbccddeeff");
    let b = member("ffeeddccbbaa99887766554433221100");
    let document = serde_json::json!({"gpu_budget":null,"cache_members":[a,b]});
    let PublishedGpuCapacities::Observed(observations) = observe(Some(&document), 1001) else {
        panic!("valid members refused")
    };
    assert_eq!(observations.len(), 2);
    assert!(
        observations
            .iter()
            .all(|o| o.capacity.admitted_driver_budget().is_some())
    );
}

#[test]
fn malformed_or_empty_members_never_fall_back_to_the_singular_budget() {
    assert!(matches!(
        observe(None, 1001),
        PublishedGpuCapacities::Legacy
    ));
    assert!(matches!(
        observe(Some(&serde_json::json!({})), 1001),
        PublishedGpuCapacities::Legacy
    ));
    for field in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!([{}]),
    ] {
        let document = serde_json::json!({"gpu_budget":member("00112233445566778899aabbccddeeff").budget,"cache_members":field});
        assert!(matches!(
            observe(Some(&document), 1001),
            PublishedGpuCapacities::Unavailable(_)
        ));
    }
    assert!(
        matches!(observe(Some(&serde_json::json!({"cache_members":[]})),1001),PublishedGpuCapacities::Observed(o) if o.is_empty())
    );
}

#[test]
fn wrong_member_identity_aliases_and_geometry_are_refused() {
    let a = member("00112233445566778899aabbccddeeff");
    for mutation in 0..5 {
        let mut b = member("ffeeddccbbaa99887766554433221100");
        match mutation {
            0 => b.key.clear(),
            1 => b.key = a.key.clone(),
            2 => b.requested_target_bytes = 0,
            3 => b.adapter = a.adapter.clone(),
            _ => {
                b.adapter = a.adapter.clone();
                b.budget.as_mut().unwrap().adapter = a.adapter.clone();
            }
        }
        assert!(matches!(
            observe(Some(&serde_json::json!({"cache_members":[a,b]})), 1001),
            PublishedGpuCapacities::Unavailable(_)
        ));
    }
}

#[test]
fn released_missing_stale_and_estimated_members_keep_unknown_admission() {
    let base = member("00112233445566778899aabbccddeeff");
    for mutation in 0..4 {
        let mut value = base.clone();
        match mutation {
            0 => value.state = CacheState::Off,
            1 => value.budget = None,
            2 => value.budget.as_mut().unwrap().sampled_at_unix_ms = 1,
            _ => value.budget.as_mut().unwrap().source = GpuBudgetSource::ProviderLocalEstimate,
        }
        let PublishedGpuCapacities::Observed(values) =
            observe(Some(&serde_json::json!({"cache_members":[value]})), 6000)
        else {
            panic!("record lost")
        };
        assert!(values[0].capacity.admitted_driver_budget().is_none());
    }
    let mut vk = base;
    vk.adapter.as_mut().unwrap().backend = "vulkan".into();
    vk.budget.as_mut().unwrap().adapter = vk.adapter.clone();
    let PublishedGpuCapacities::Observed(values) =
        observe(Some(&serde_json::json!({"cache_members":[vk]})), 1001)
    else {
        panic!("Vulkan record refused")
    };
    assert_eq!(values[0].capacity.provider, Some(GpuProviderKind::Vulkan));
}
