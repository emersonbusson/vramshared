// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/nbd_readiness/tests.rs
use super::*;

const GIB: u64 = 1024 * MIB_BYTES;

fn ready_input() -> ProductInput {
    ProductInput {
        transport: Some(ProductTransport::Nbd),
        nbd_swap_active: Some(true),
        daemon_running: true,
        legacy_ublk_product_active: Some(false),
        release_gate: Gate::Pass,
        relay_gate: Gate::Pass,
        binary_match: Gate::Pass,
        capacity: CapacitySample::Observed {
            sink: LowerTierSink::Known("lower-sink".into()),
            free_absorbable_bytes: 2 * GIB,
            alignment_bytes: MIB_BYTES,
        },
        vram_bytes: GIB,
        operation: Operation::ReadOnly,
        approval: Approval::NotRequired,
        reboot_requested: false,
    }
}

#[test]
fn nbd_only_transport_is_the_only_ready_value() {
    assert_eq!(evaluate_product(&ready_input()).state, ProductState::Ready);

    let mut no_transport = ready_input();
    no_transport.transport = Some(ProductTransport::None);
    assert_eq!(
        evaluate_product(&no_transport).reason,
        ReadinessReason::Refusal(RefusalCode::TransportMustBeNbd)
    );

    let mut legacy_ublk = ready_input();
    legacy_ublk.legacy_ublk_product_active = Some(true);
    assert_eq!(
        evaluate_product(&legacy_ublk).reason,
        ReadinessReason::Refusal(RefusalCode::LegacyUblkProductActive)
    );
}

#[test]
fn evaluate_product_never_publishes_a_measured_absence_from_an_unreadable_swap_table() {
    // `nbd_swap_active: None` is a probe that never took. It must not be
    // read as `false` — that is what published a measured `ProductOff`
    // from an unreadable table.
    let mut input = ready_input();
    input.nbd_swap_active = None;
    let decision = evaluate_product(&input);
    assert_eq!(decision.state, ProductState::Blocked);
    assert_eq!(
        decision.reason,
        ReadinessReason::Refusal(RefusalCode::NbdSwapActiveUnknown)
    );
    assert!(!decision.mutation_permitted);
}

#[test]
fn evaluate_product_never_skips_the_legacy_ublk_refusal_on_an_unreadable_swap_table() {
    // The worse direction: `.unwrap_or(false)` used to *skip* the hard
    // `LegacyUblkProductActive` refusal, so an unreadable table could let
    // a live legacy ublk product through. Fail open is not available here.
    let mut input = ready_input();
    input.legacy_ublk_product_active = None;
    let decision = evaluate_product(&input);
    assert_eq!(decision.state, ProductState::Blocked);
    assert_eq!(
        decision.reason,
        ReadinessReason::Refusal(RefusalCode::LegacyUblkProductUnknown)
    );
    assert!(!decision.mutation_permitted);
}

#[test]
fn unobserved_swap_facts_refuse_before_product_off_and_before_lifecycle_disagreement() {
    // Order is load-bearing: `None` must be refused before the
    // `ProductOff` arm would accept it and before the lifecycle
    // comparison would read it as a measured `false`.
    let mut off = ready_input();
    off.transport = Some(ProductTransport::None);
    off.nbd_swap_active = None;
    off.daemon_running = false;
    off.binary_match = Gate::NotApplicable;
    assert_eq!(
        evaluate_product(&off).reason,
        ReadinessReason::Refusal(RefusalCode::NbdSwapActiveUnknown)
    );

    // A measured daemon with an unobserved swap table is a lifecycle
    // disagreement — but the unobserved fact is refused first, because
    // disagreement is a claim about two *measured* facts.
    let mut disagree = ready_input();
    disagree.nbd_swap_active = None;
    disagree.daemon_running = true;
    assert_eq!(
        evaluate_product(&disagree).reason,
        ReadinessReason::Refusal(RefusalCode::NbdSwapActiveUnknown)
    );
}

#[test]
fn unobserved_transport_on_an_active_nbd_tier_is_a_transport_refusal() {
    // `transport: None` is not "no transport" — it is a transport that was
    // never taken. With a measured active NBD tier, that cannot back a
    // positive claim, and `TransportMustBeNbd` already names "missing or
    // is not NBD".
    let mut input = ready_input();
    input.transport = None;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::TransportMustBeNbd)
    );
}

#[test]
fn lower_tier_formula_uses_ten_percent_or_512_mib() {
    assert_eq!(
        minimum_lower_tier_bytes(GIB),
        Ok(GIB + MIN_LOWER_TIER_MARGIN_BYTES)
    );
    let ten_gib = 10 * GIB;
    assert_eq!(minimum_lower_tier_bytes(ten_gib), Ok(11 * GIB));
    assert_eq!(
        minimum_lower_tier_bytes(0),
        Err(RefusalCode::InvalidVramSize)
    );
}

#[test]
fn capacity_shortfall_refuses_before_mutation() {
    let required = match minimum_lower_tier_bytes(GIB) {
        Ok(value) => value,
        Err(error) => panic!("valid one GiB capacity requirement: {error:?}"),
    };
    let mut input = ready_input();
    input.capacity = CapacitySample::Observed {
        sink: LowerTierSink::Known("lower-sink".into()),
        free_absorbable_bytes: required - MIB_BYTES,
        alignment_bytes: MIB_BYTES,
    };
    let decision = evaluate_product(&input);
    assert_eq!(decision.state, ProductState::Blocked);
    assert_eq!(
        decision.reason,
        ReadinessReason::Refusal(RefusalCode::LowerTierShortfall)
    );
    assert!(!decision.mutation_permitted);

    input.capacity = CapacitySample::Stale;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::LowerTierMeasurementStale)
    );
    input.capacity = CapacitySample::Unknown;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::LowerTierSinkUnknown)
    );
    input.vram_bytes = u64::MAX;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::CapacityOverflow)
    );
}

#[test]
fn product_off_is_not_ready_alias() {
    let ready = evaluate_product(&ready_input());
    let mut off_input = ready_input();
    off_input.transport = Some(ProductTransport::None);
    off_input.nbd_swap_active = Some(false);
    off_input.daemon_running = false;
    off_input.binary_match = Gate::NotApplicable;
    let off = evaluate_product(&off_input);
    assert_eq!(off.state, ProductState::ProductOff);
    assert_eq!(off.reason, ReadinessReason::ProductOff);
    assert_ne!(off.state, ready.state);
}

#[test]
fn deterministic_gate_failure_is_not_retried() {
    let mut input = ready_input();
    input.release_gate = Gate::Fail;
    let first = evaluate_product(&input);
    let second = evaluate_product(&input);
    assert_eq!(
        first.reason,
        ReadinessReason::Refusal(RefusalCode::ReleaseGateFailed)
    );
    assert!(!first.retry_allowed);
    assert_eq!(first.attempts, 1);
    assert_eq!(first, second);
}

#[test]
fn activation_and_deactivation_are_idempotent() {
    let mut activation = ready_input();
    activation.operation = Operation::Activate;
    activation.approval = Approval::Present;
    let first_activation = evaluate_product(&activation);
    assert_eq!(first_activation, evaluate_product(&activation));
    assert!(first_activation.mutation_permitted);

    let mut deactivation = ready_input();
    deactivation.transport = Some(ProductTransport::None);
    deactivation.nbd_swap_active = Some(false);
    deactivation.daemon_running = false;
    deactivation.binary_match = Gate::NotApplicable;
    deactivation.operation = Operation::Deactivate;
    deactivation.approval = Approval::Present;
    let first_deactivation = evaluate_product(&deactivation);
    assert_eq!(first_deactivation, evaluate_product(&deactivation));
    assert_eq!(first_deactivation.state, ProductState::ProductOff);
    assert!(first_deactivation.mutation_permitted);
}

#[test]
fn invalid_gate_shapes_fail_closed() {
    let mut input = ready_input();
    input.operation = Operation::Install;
    input.approval = Approval::Missing;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::ApprovalMissing)
    );

    input.operation = Operation::ReadOnly;
    input.approval = Approval::NotRequired;
    input.reboot_requested = true;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::RebootRequested)
    );

    input.reboot_requested = false;
    input.capacity = CapacitySample::Observed {
        sink: LowerTierSink::Ambiguous,
        free_absorbable_bytes: 2 * GIB,
        alignment_bytes: MIB_BYTES,
    };
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::LowerTierSinkAmbiguous)
    );

    input.capacity = CapacitySample::Observed {
        sink: LowerTierSink::Known("lower-sink".into()),
        free_absorbable_bytes: 2 * GIB + 1,
        alignment_bytes: MIB_BYTES,
    };
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::LowerTierAlignmentInvalid)
    );
}

#[test]
fn binary_and_lifecycle_disagreement_fail_closed() {
    let mut input = ready_input();
    input.binary_match = Gate::NotApplicable;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::BinaryMatchRequired)
    );

    input.binary_match = Gate::Pass;
    input.daemon_running = false;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::NbdLifecycleIncomplete)
    );
}

#[test]
fn validate_lower_tier_capacity_success() {
    let vram = GIB;
    let required = match minimum_lower_tier_bytes(vram) {
        Ok(value) => value,
        Err(error) => panic!("valid one GiB capacity requirement: {error:?}"),
    };

    // Exact required capacity
    let sample_exact = CapacitySample::Observed {
        sink: LowerTierSink::Known("valid-sink".into()),
        free_absorbable_bytes: required,
        alignment_bytes: MIB_BYTES,
    };
    assert_eq!(
        validate_lower_tier_capacity(vram, &sample_exact),
        Ok(required)
    );

    // Excess capacity
    let sample_excess = CapacitySample::Observed {
        sink: LowerTierSink::Known("valid-sink".into()),
        free_absorbable_bytes: required + 10 * MIB_BYTES,
        alignment_bytes: MIB_BYTES,
    };
    assert_eq!(
        validate_lower_tier_capacity(vram, &sample_excess),
        Ok(required)
    );
}

#[test]
fn validate_lower_tier_capacity_vram_errors() {
    let sample = CapacitySample::Observed {
        sink: LowerTierSink::Known("valid-sink".into()),
        free_absorbable_bytes: 2 * GIB,
        alignment_bytes: MIB_BYTES,
    };

    // Zero VRAM size
    assert_eq!(
        validate_lower_tier_capacity(0, &sample),
        Err(RefusalCode::InvalidVramSize)
    );

    // VRAM overflow
    assert_eq!(
        validate_lower_tier_capacity(u64::MAX, &sample),
        Err(RefusalCode::CapacityOverflow)
    );
}

#[test]
fn validate_lower_tier_capacity_sample_and_sink_errors() {
    let vram = GIB;

    // Unknown sample
    assert_eq!(
        validate_lower_tier_capacity(vram, &CapacitySample::Unknown),
        Err(RefusalCode::LowerTierSinkUnknown)
    );

    // Stale sample
    assert_eq!(
        validate_lower_tier_capacity(vram, &CapacitySample::Stale),
        Err(RefusalCode::LowerTierMeasurementStale)
    );

    // Empty sink identity
    let empty_sink = CapacitySample::Observed {
        sink: LowerTierSink::Known("   ".into()),
        free_absorbable_bytes: 2 * GIB,
        alignment_bytes: MIB_BYTES,
    };
    assert_eq!(
        validate_lower_tier_capacity(vram, &empty_sink),
        Err(RefusalCode::LowerTierSinkUnknown)
    );

    // Ambiguous sink
    let ambiguous_sink = CapacitySample::Observed {
        sink: LowerTierSink::Ambiguous,
        free_absorbable_bytes: 2 * GIB,
        alignment_bytes: MIB_BYTES,
    };
    assert_eq!(
        validate_lower_tier_capacity(vram, &ambiguous_sink),
        Err(RefusalCode::LowerTierSinkAmbiguous)
    );
}

#[test]
fn validate_lower_tier_capacity_alignment_and_shortfall_errors() {
    let vram = GIB;
    let required = match minimum_lower_tier_bytes(vram) {
        Ok(value) => value,
        Err(error) => panic!("valid one GiB capacity requirement: {error:?}"),
    };

    // Alignment bytes == 0
    let zero_alignment = CapacitySample::Observed {
        sink: LowerTierSink::Known("sink".into()),
        free_absorbable_bytes: required,
        alignment_bytes: 0,
    };
    assert_eq!(
        validate_lower_tier_capacity(vram, &zero_alignment),
        Err(RefusalCode::LowerTierAlignmentInvalid)
    );

    // Misaligned absorbable bytes
    let misaligned = CapacitySample::Observed {
        sink: LowerTierSink::Known("sink".into()),
        free_absorbable_bytes: required + 500,
        alignment_bytes: MIB_BYTES,
    };
    assert_eq!(
        validate_lower_tier_capacity(vram, &misaligned),
        Err(RefusalCode::LowerTierAlignmentInvalid)
    );

    // Capacity shortfall
    let shortfall = CapacitySample::Observed {
        sink: LowerTierSink::Known("sink".into()),
        free_absorbable_bytes: required - MIB_BYTES,
        alignment_bytes: MIB_BYTES,
    };
    assert_eq!(
        validate_lower_tier_capacity(vram, &shortfall),
        Err(RefusalCode::LowerTierShortfall)
    );
}

#[test]
fn product_state_and_transport_as_str_spell_stable_tokens() {
    assert_eq!(ProductState::ProductOff.as_str(), "PRODUCT_OFF");
    assert_eq!(ProductState::Ready.as_str(), "READY");
    assert_eq!(ProductState::Blocked.as_str(), "BLOCKED");
    assert_eq!(ProductTransport::Nbd.as_str(), "nbd");
    assert_eq!(ProductTransport::None.as_str(), "none");
}

#[test]
fn refusal_code_as_str_names_every_variant() {
    // Every refusal code is operator-facing status surface. A silent rename
    // would break dashboards and triage scripts that match these tokens.
    let cases = [
        (RefusalCode::TransportMustBeNbd, "TRANSPORT_MUST_BE_NBD"),
        (
            RefusalCode::LegacyUblkProductActive,
            "LEGACY_UBLK_PRODUCT_ACTIVE",
        ),
        (
            RefusalCode::NbdLifecycleIncomplete,
            "NBD_LIFECYCLE_INCOMPLETE",
        ),
        (RefusalCode::ReleaseGateFailed, "RELEASE_GATE_FAILED"),
        (RefusalCode::ReleaseGateUnknown, "RELEASE_GATE_UNKNOWN"),
        (RefusalCode::RelayGateFailed, "RELAY_GATE_FAILED"),
        (RefusalCode::RelayGateUnknown, "RELAY_GATE_UNKNOWN"),
        (RefusalCode::BinaryMatchFailed, "BINARY_MATCH_FAILED"),
        (RefusalCode::BinaryMatchRequired, "BINARY_MATCH_REQUIRED"),
        (RefusalCode::BinaryMatchUnknown, "BINARY_MATCH_UNKNOWN"),
        (RefusalCode::InvalidVramSize, "INVALID_VRAM_SIZE"),
        (RefusalCode::CapacityOverflow, "CAPACITY_OVERFLOW"),
        (RefusalCode::LowerTierSinkUnknown, "LOWER_TIER_SINK_UNKNOWN"),
        (
            RefusalCode::LowerTierSinkAmbiguous,
            "LOWER_TIER_SINK_AMBIGUOUS",
        ),
        (
            RefusalCode::LowerTierMeasurementStale,
            "LOWER_TIER_MEASUREMENT_STALE",
        ),
        (
            RefusalCode::LowerTierAlignmentInvalid,
            "LOWER_TIER_ALIGNMENT_INVALID",
        ),
        (RefusalCode::LowerTierShortfall, "LOWER_TIER_SHORTFALL"),
        (RefusalCode::ApprovalMissing, "APPROVAL_MISSING"),
        (RefusalCode::RebootRequested, "REBOOT_REQUESTED"),
    ];
    for (code, expected) in cases {
        assert_eq!(code.as_str(), expected, "{code:?}");
    }
}

#[test]
fn readiness_reason_as_str_names_every_variant() {
    assert_eq!(ReadinessReason::NotEvaluated.as_str(), "not_evaluated");
    assert_eq!(ReadinessReason::ProductOff.as_str(), "product_off");
    assert_eq!(ReadinessReason::AllGatesPass.as_str(), "all_gates_pass");
    assert_eq!(
        ReadinessReason::Refusal(RefusalCode::RebootRequested).as_str(),
        "REBOOT_REQUESTED"
    );
}

#[test]
fn relay_and_binary_gates_fail_closed() {
    let mut input = ready_input();
    input.relay_gate = Gate::Fail;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::RelayGateFailed)
    );
    input.relay_gate = Gate::Unknown;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::RelayGateUnknown)
    );
    input.relay_gate = Gate::NotApplicable;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::RelayGateUnknown)
    );

    input.relay_gate = Gate::Pass;
    input.binary_match = Gate::Fail;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::BinaryMatchFailed)
    );
    input.binary_match = Gate::Unknown;
    assert_eq!(
        evaluate_product(&input).reason,
        ReadinessReason::Refusal(RefusalCode::BinaryMatchUnknown)
    );
}
