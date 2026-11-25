// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/gpu_set/tests.rs
//! Budget-set regressions; all driver observations are explicitly synthetic.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::customer::gpu_plan::tests::fixture_budget;
use crate::readiness::{capacity::effective_capacity, probes::GpuProviderKind};
const GIB: u64 = 1024 * 1024 * 1024;
const A: &str = "00112233445566778899aabbccddeeff";
const B: &str = "ffeeddccbbaa99887766554433221100";

fn capacity(key: &str, total: u64, used: u64) -> CapacityObservation {
    let mut budget = fixture_budget(total, used);
    budget.adapter.as_mut().unwrap().key = key.into();
    effective_capacity(Some(&budget), 1001, Some(GpuProviderKind::Cuda))
}
fn profile_key(observation: &CapacityObservation) -> String {
    observation
        .admitted_driver_budget()
        .unwrap()
        .budget()
        .adapter
        .as_ref()
        .unwrap()
        .resource_profile_key()
        .unwrap()
}

#[test]
fn independently_occupied_96_and_32_gib_budgets_keep_separate_reserves() {
    let observations = [
        capacity(B, 32 * GIB, 20 * GIB),
        capacity(A, 96 * GIB, 10 * GIB),
    ];
    let caps = BTreeMap::from([
        (profile_key(&observations[0]), 2 * GIB),
        (profile_key(&observations[1]), 64 * GIB),
    ]);
    let plans = capture(&observations, &caps).unwrap();
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].gpu.budget.adapter.as_ref().unwrap().key, A);
    assert_eq!(plans[1].gpu.budget.adapter.as_ref().unwrap().key, B);
    assert!(plans[0].configured_reserve_bytes > plans[1].configured_reserve_bytes);
    assert_eq!(plans[0].gpu.target_bytes, 64 * GIB);
    assert_eq!(plans[1].gpu.target_bytes, 2 * GIB);
    assert_eq!(validate(&plans).unwrap(), 66 * GIB);
    let reversed = [observations[1].clone(), observations[0].clone()];
    assert_eq!(capture(&reversed, &caps).unwrap(), plans);
}

#[test]
fn disabled_gpu_ceilings_do_not_select_or_donate_an_adapter() {
    let observed = capacity(A, 6 * GIB, 0);
    let mut caps = BTreeMap::from([
        (profile_key(&observed), GIB),
        ("nvidia-uuid:gpu-missing".into(), 0),
    ]);
    let plans = capture(&[observed], &caps).unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].gpu.target_bytes, GIB);
    caps.values_mut().for_each(|value| *value = 0);
    assert_eq!(
        capture(&[], &caps).unwrap_err().reason,
        CustomerReason::CapacityRefused
    );
}

#[test]
fn requested_members_need_unique_fresh_proofs_and_nonaliased_caps() {
    let observation = capacity(A, 6 * GIB, 0);
    let key = profile_key(&observation);
    let caps = BTreeMap::from([(key.clone(), GIB)]);
    assert_eq!(
        capture(&[], &caps).unwrap_err().reason,
        CustomerReason::ObservationUnavailable
    );
    assert!(capture(&[observation.clone(), observation.clone()], &caps).is_err());
    let aliases = BTreeMap::from([(key.to_uppercase(), 0), (key.clone(), GIB)]);
    assert_eq!(
        capture(std::slice::from_ref(&observation), &aliases)
            .unwrap_err()
            .reason,
        CustomerReason::IdentityChanged
    );
    let overflow = BTreeMap::from([(key, u64::MAX), ("other".into(), u64::MAX)]);
    assert_eq!(
        capture(std::slice::from_ref(&observation), &overflow)
            .unwrap_err()
            .reason,
        CustomerReason::CapacityRefused
    );
    let mut stale = observation;
    stale.effective_bytes = None;
    assert_eq!(
        capture(&[stale], &caps).unwrap_err().reason,
        CustomerReason::ObservationUnavailable
    );
}

#[test]
fn automatic_selection_keeps_one_largest_safe_adapter_without_model_ceiling() {
    let observations = [capacity(B, 32 * GIB, 0), capacity(A, 256 * GIB, 0)];
    let plans = capture(&observations, &BTreeMap::new()).unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].gpu.budget.adapter.as_ref().unwrap().key, A);
    assert!(plans[0].gpu.target_bytes > 96 * GIB);
    assert!(capture(&[], &BTreeMap::new()).is_err());
    assert!(capture(&[capacity(A, 0, 0)], &BTreeMap::new()).is_err());
}

#[test]
fn reviewed_set_refuses_aliases_order_drift_and_modified_member_metadata() {
    let observations = [capacity(A, 8 * GIB, 0), capacity(B, 12 * GIB, 0)];
    let caps = observations.iter().map(|o| (profile_key(o), GIB)).collect();
    let plans = capture(&observations, &caps).unwrap();
    for mutation in 0..6 {
        let mut altered = plans.clone();
        match mutation {
            0 => altered.reverse(),
            1 => altered[1] = altered[0].clone(),
            2 => altered[1].configured_reserve_bytes -= 1,
            3 => altered[1].runtime_free_buffer_bytes = 0,
            4 => altered[1].gpu.target_bytes += 1,
            _ => {
                altered[0].gpu.budget.adapter.as_mut().unwrap().luid =
                    Some("12345678:00000001".into());
                altered[1].gpu.budget.adapter.as_mut().unwrap().luid =
                    Some("12345678:00000001".into());
            }
        }
        assert!(validate(&altered).is_err(), "mutation {mutation} admitted");
    }
    assert!(validate(&[]).is_err());
    // Every member fits the unchanged checked reserve arithmetic. Only the
    // combined target overflows; a refused individual proof is not this case.
    let mut huge: Vec<_> = (1..=26)
        .map(|index| {
            let identity = format!("{index:032x}");
            CacheAdapterPlan::new(
                GpuPreparationPlan::capture(
                    &capacity(&identity, u64::MAX / 20, 0),
                    &BTreeMap::new(),
                )
                .unwrap(),
            )
        })
        .collect();
    huge.sort_by_key(key);
    assert_eq!(
        validate(&huge).unwrap_err().reason,
        CustomerReason::CapacityRefused
    );
}
