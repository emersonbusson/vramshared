// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/plan_document/node_binding_tests.rs
//! Approval integrity over complete optional identities and later GPU members.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::customer::plan::{ApprovalBinding, binding_matches};
use crate::customer::setup_scope::SetupOperation;
use serde_json::{Value, json};

pub(super) fn complete_fixture() -> SetupPlan {
    let mut plan = tests::fixture_plan();
    let SetupOperation::GpuCache { cache } = &mut plan.operation else {
        panic!("cache fixture")
    };
    let mut second = cache.gpus[0].clone();
    second.gpu.budget.adapter.as_mut().unwrap().key = "ffeeddccbbaa99887766554433221100".into();
    cache.gpus.push(second);
    for (index, member) in cache.gpus.iter_mut().enumerate() {
        let adapter = member.gpu.budget.adapter.as_mut().unwrap();
        adapter.luid = Some(format!("{:08x}:00000001", index + 1));
        adapter.node_mask = Some(2);
        member.gpu.requested_ceiling_bytes = Some(member.gpu.target_bytes);
    }
    cache.required_lower_sink_bytes = ramshared_tier::nbd_readiness::minimum_lower_tier_bytes(
        crate::customer::gpu_set::validate(&cache.gpus).unwrap(),
    )
    .unwrap();
    plan.validate_operation().unwrap();
    plan
}

#[test]
fn reviewed_node_binding_refuses_valid_node_changes() {
    let original = complete_fixture();
    let document: Value =
        serde_json::from_str(&encode_plan_document(&original, "resource-profile").unwrap())
            .unwrap();
    assert_eq!(
        parse_plan_document(&document.to_string()).unwrap().0,
        original
    );
    let grant = ApprovalBinding {
        approval_id: "node-binding".into(),
        plan_fingerprint: original.fingerprint(),
        issued_unix: 100,
        expires_unix: 200,
    };
    assert!(binding_matches(&grant, &original, 150));
    for member in 0..2 {
        for mask in [None, Some(1), Some(4)] {
            let mut altered = original.clone();
            let SetupOperation::GpuCache { cache } = &mut altered.operation else {
                panic!("cache fixture")
            };
            cache.gpus[member]
                .gpu
                .budget
                .adapter
                .as_mut()
                .unwrap()
                .node_mask = mask;
            altered.validate_operation().unwrap();
            assert_ne!(
                original.fingerprint(),
                altered.fingerprint(),
                "valid node mutation omitted for member {member}"
            );
            assert!(!binding_matches(&grant, &altered, 150));
            let mut tampered = document.clone();
            tampered["plan"] = serde_json::to_value(&altered).unwrap();
            assert!(
                parse_plan_document(&tampered.to_string())
                    .unwrap_err()
                    .detail
                    .contains("fingerprint")
            );
            if mask.is_none() {
                let absent_grant = ApprovalBinding {
                    plan_fingerprint: altered.fingerprint(),
                    ..grant.clone()
                };
                assert!(
                    !binding_matches(&absent_grant, &original, 150),
                    "adding a node also changes the approval"
                );
            }
        }
    }
}

fn leaves(value: &Value, path: Vec<String>, out: &mut Vec<Vec<String>>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let mut path = path.clone();
                path.push(key.clone());
                leaves(child, path, out);
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter().enumerate() {
                let mut path = path.clone();
                path.push(index.to_string());
                leaves(child, path, out);
            }
        }
        _ => out.push(path),
    }
}
fn at_mut<'a>(mut value: &'a mut Value, path: &[String]) -> &'a mut Value {
    for part in path {
        value = if value.is_array() {
            value.get_mut(part.parse::<usize>().unwrap()).unwrap()
        } else {
            value.get_mut(part).unwrap()
        };
    }
    value
}

#[test]
fn reviewed_node_binding_censuses_every_populated_wire_field() {
    let original = complete_fixture();
    let value = serde_json::to_value(&original).unwrap();
    let mut fields = vec![];
    leaves(&value, vec![], &mut fields);
    assert_eq!(
        fields
            .iter()
            .filter(|path| path.last().unwrap() == "node_mask")
            .count(),
        2
    );
    for path in fields {
        let field = path.last().unwrap().as_str();
        if field == "kind" {
            continue;
        } // Existing tagged-variant refusal test owns this case.
        let mut changed = value.clone();
        let leaf = at_mut(&mut changed, &path);
        *leaf = match field {
            "report_decision" => json!("blocked"),
            "source" => json!("provider_local_estimate"),
            "node_mask" => json!(4),
            _ => match leaf {
                Value::Number(n) => json!(n.as_u64().unwrap() + 1),
                Value::String(s) => json!(format!("{s}-changed")),
                _ => panic!("populate and define a mutation for {path:?}"),
            },
        };
        let altered: SetupPlan = serde_json::from_value(changed).unwrap();
        assert_ne!(
            original.fingerprint(),
            altered.fingerprint(),
            "unbound wire field {path:?}"
        );
    }
}

#[test]
fn reviewed_node_binding_rejects_previous_cache_bodies() {
    let plan = complete_fixture();
    for version in [1, 2] {
        let mut previous = plan.clone();
        let SetupOperation::GpuCache { cache } = &mut previous.operation else {
            panic!("cache fixture")
        };
        cache.schema_version = version;
        let document = SetupPlanDocument {
            schema_version: PLAN_DOCUMENT_SCHEMA_VERSION,
            profile_name: "resource-profile".into(),
            plan_fingerprint: previous.fingerprint(),
            plan: previous,
        };
        assert!(
            parse_plan_document(&serde_json::to_string(&document).unwrap()).is_err(),
            "old body admitted with a recomputed fingerprint"
        );
    }
}
