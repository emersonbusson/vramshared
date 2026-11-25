// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/tests/member_composition.rs
//! Current-document member authority; observations are synthetic, no driver I/O.
use super::*;

fn observations() -> Observations {
    obs(
        platform_probe(Observation::Measured(PlatformKind::NativeLinux), "6.18"),
        gpu_cuda(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    )
}
fn row(key: &str) -> serde_json::Value {
    let mut budget = driver_budget(1000);
    budget.adapter = Some(GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key: key.into(),
        luid: None,
    });
    serde_json::json!({"key":key,"state":"ACTIVE","requested_target_bytes":4096,"cached_bytes":0,"failures":0,"adapter":budget.adapter,"budget":budget})
}

#[test]
fn readiness_renders_each_member_without_a_fabricated_aggregate_capacity() {
    let document = serde_json::json!({"cache_members":[row("00112233445566778899aabbccddeeff"),row("ffeeddccbbaa99887766554433221100")],"gpu_budget":driver_budget(1000)});
    let report = report_with_published_document(observations(), Some(&document), 1001);
    assert!(report.capacity.effective_bytes.is_none());
    let value: serde_json::Value = serde_json::from_str(&render_json(&report)).unwrap();
    assert_eq!(value["gpu_capacities"]["source"], "members");
    assert_eq!(
        value["gpu_capacities"]["members"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        value["gpu_capacities"]["members"][1]["adapter"]["key"],
        "ffeeddccbbaa99887766554433221100"
    );
    assert_eq!(
        value["gpu_capacities"]["members"][0]["effective_bytes"],
        3u64 << 30
    );
}

#[test]
fn malformed_members_override_contradictory_singular_budget_in_readiness() {
    let document = serde_json::json!({"cache_members":null,"gpu_budget":driver_budget(1000)});
    let report = report_with_published_document(observations(), Some(&document), 1001);
    assert!(report.capacity.effective_bytes.is_none());
    assert!(!report.warnings.is_empty());
    let value: serde_json::Value = serde_json::from_str(&render_json(&report)).unwrap();
    assert_eq!(value["gpu_capacities"]["source"], "unavailable");
}

#[test]
fn one_member_uses_its_own_capacity_and_legacy_documents_stay_explicit() {
    let document = serde_json::json!({"cache_members":[row("00112233445566778899aabbccddeeff")]});
    let report = report_with_published_document(observations(), Some(&document), 1001);
    assert_eq!(report.capacity.effective_bytes, Some(3 << 30));
    let legacy = report_with_published_document(observations(), None, 1001);
    let value: serde_json::Value = serde_json::from_str(&render_json(&legacy)).unwrap();
    assert_eq!(value["gpu_capacities"]["source"], "legacy_singular");
}
