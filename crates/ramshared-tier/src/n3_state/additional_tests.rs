// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/n3_state/additional_tests.rs
use super::*;

#[test]
fn test_guard_clauses_generation_validation() {
    let mut machine = LeaseMachine::new();
    let lease_id = LeaseId::new(b"lease-1").unwrap_or_else(|_| panic!("failed to create lease_id"));
    machine.remember_generation(lease_id.clone(), 10);

    assert_eq!(
        machine.validate_generation(&lease_id, 10),
        Err(FailureReason::StateTransition(
            StateTransitionError::StaleGeneration {
                provided: 10,
                expected: 10
            }
        ))
    );
    assert_eq!(
        machine.validate_generation(&lease_id, 9),
        Err(FailureReason::StateTransition(
            StateTransitionError::StaleGeneration {
                provided: 9,
                expected: 10
            }
        ))
    );
    assert_eq!(
        machine.validate_generation(&lease_id, 12),
        Err(FailureReason::GenerationGap)
    );
    assert_eq!(machine.validate_generation(&lease_id, 11), Ok(()));
}

#[test]
fn test_opaque_id_validation() -> Result<(), FailureReason> {
    assert_eq!(LeaseId::new(b""), Err(FailureReason::MalformedRecord));
    let long_id = vec![b'a'; 65];
    assert_eq!(LeaseId::new(&long_id), Err(FailureReason::MalformedRecord));
    let valid = LeaseId::new(b"valid-lease-id")?;
    assert_eq!(valid.as_bytes(), b"valid-lease-id");
    Ok(())
}

#[test]
fn test_restart_record_deserialization_errors() -> Result<(), FailureReason> {
    // Less than 17 bytes (header size)
    assert_eq!(
        RestartRecord::from_bytes(&[0u8; 13]),
        Err(FailureReason::MalformedRecord)
    );

    let valid_bytes = RestartRecord::host(1, Vec::new())?.to_bytes();

    // Unknown schema (bytes 4..6 is schema_version)
    let mut bad_schema = valid_bytes.clone();
    bad_schema[5] = 0x99;
    assert_eq!(
        RestartRecord::from_bytes(&bad_schema),
        Err(FailureReason::UnknownSchema)
    );

    // Guest authority marker (byte 6 is authority)
    let mut guest_auth = valid_bytes.clone();
    guest_auth[6] = GUEST_AUTHORITY_MARKER;
    assert_eq!(
        RestartRecord::from_bytes(&guest_auth),
        Err(FailureReason::HostAuthorityRequired)
    );
    Ok(())
}

#[test]
fn test_grant_and_revoke_constructors() -> Result<(), FailureReason> {
    let lease_id = LeaseId::new(b"lease-1")?;
    let event_id = EventId::new(b"event-1")?;

    let grant = Grant::host(lease_id.clone(), 1, event_id.clone(), 4096, 1, 100, 200);
    assert_eq!(grant.lease_id, lease_id);
    assert_eq!(grant.capacity_bytes, 4096);

    let revoke = Revoke::host(lease_id.clone(), 1, event_id.clone(), 200);
    assert_eq!(revoke.lease_id, lease_id);
    assert_eq!(revoke.deadline, 200);
    Ok(())
}

#[test]
fn test_state_transition_error_display() {
    use alloc::string::ToString;
    extern crate alloc;

    let err1 = StateTransitionError::IllegalTransition {
        expected: Some(StateTag::Granted),
        actual: StateTag::Absent,
    };
    assert_eq!(
        err1.to_string(),
        "illegal state transition: expected Granted, actual Absent"
    );

    let err2 = StateTransitionError::IllegalTransition {
        expected: None,
        actual: StateTag::Absent,
    };
    assert_eq!(err2.to_string(), "illegal state transition: actual Absent");

    let err3 = StateTransitionError::IllegalPreflight {
        expected: Some(PreflightState::Constrained),
        actual: PreflightState::HostUnavailable,
    };
    assert_eq!(
        err3.to_string(),
        "illegal preflight transition: expected Constrained, actual HostUnavailable"
    );

    let err4 = StateTransitionError::IllegalPreflight {
        expected: None,
        actual: PreflightState::HostUnavailable,
    };
    assert_eq!(
        err4.to_string(),
        "illegal preflight transition: actual HostUnavailable"
    );

    let err5 = StateTransitionError::StaleGeneration {
        provided: 5,
        expected: 10,
    };
    assert_eq!(
        err5.to_string(),
        "stale generation: provided 5, expected > 10"
    );
}

#[test]
fn test_host_observation_validate() -> Result<(), FailureReason> {
    let valid_obs = HostObservation::host(1, 4096, 0, 4096, 100, 10, EventId::new(b"evt-1")?);

    // Happy path
    assert_eq!(valid_obs.validate(100), Ok(()));
    assert_eq!(valid_obs.validate(105), Ok(()));

    // Unknown schema
    assert_eq!(
        valid_obs.clone().with_schema_version(99).validate(100),
        Err(FailureReason::UnknownSchema)
    );

    // Zero epoch
    let mut zero_epoch = valid_obs.clone();
    zero_epoch.host_epoch = 0;
    assert_eq!(
        zero_epoch.validate(100),
        Err(FailureReason::MalformedRecord)
    );

    // Empty adapter ID
    let mut empty_adapter = valid_obs.clone();
    empty_adapter.adapter_id = OpaqueId(Vec::new());
    assert_eq!(
        empty_adapter.validate(100),
        Err(FailureReason::MalformedRecord)
    );

    // Empty event ID
    let mut empty_event = valid_obs.clone();
    empty_event.event_id = OpaqueId(Vec::new());
    assert_eq!(
        empty_event.validate(100),
        Err(FailureReason::MalformedRecord)
    );

    // Non-host authority
    let mut guest_auth = valid_obs.clone();
    guest_auth.authority = Authority::Guest;
    assert_eq!(
        guest_auth.validate(100),
        Err(FailureReason::HostAuthorityRequired)
    );

    // Invalid max_age (0 or > MAX_OBSERVATION_AGE)
    let mut zero_max_age = valid_obs.clone();
    zero_max_age.max_age = 0;
    assert_eq!(
        zero_max_age.validate(100),
        Err(FailureReason::MalformedRecord)
    );

    let mut excess_max_age = valid_obs.clone();
    excess_max_age.max_age = MAX_OBSERVATION_AGE + 1;
    assert_eq!(
        excess_max_age.validate(100),
        Err(FailureReason::MalformedRecord)
    );

    // Clock errors
    assert_eq!(
        valid_obs.validate(99),
        Err(FailureReason::InvalidObservationClock)
    );
    assert_eq!(
        valid_obs.validate(111),
        Err(FailureReason::StaleObservation)
    );

    // Observation events validation
    let mut too_many_events = valid_obs.clone();
    too_many_events.events = (0..=MAX_OBSERVATION_EVENTS)
        .map(|i| {
            let id_bytes = [b'e', i as u8];
            ObservationEvent::new(
                EventId::new(id_bytes).unwrap_or_else(|_| panic!("failed to create event_id")),
                ObservationEventKind::Healthy,
            )
        })
        .collect();
    assert_eq!(
        too_many_events.validate(100),
        Err(FailureReason::MalformedRecord)
    );

    let mut unknown_event_kind = valid_obs.clone();
    unknown_event_kind.events = vec![ObservationEvent::new(
        EventId::new(b"evt-unk")?,
        ObservationEventKind::Unknown,
    )];
    assert_eq!(
        unknown_event_kind.validate(100),
        Err(FailureReason::MalformedRecord)
    );

    let mut empty_event_id_event = valid_obs.clone();
    empty_event_id_event.events = vec![ObservationEvent::new(
        OpaqueId(Vec::new()),
        ObservationEventKind::Healthy,
    )];
    assert_eq!(
        empty_event_id_event.validate(100),
        Err(FailureReason::MalformedRecord)
    );

    // Budget counters validation
    let mut unaligned_budget = valid_obs.clone();
    unaligned_budget.budget_bytes = 100;
    assert_eq!(
        unaligned_budget.validate(100),
        Err(FailureReason::ImpossibleBudget)
    );

    let mut excess_budget = valid_obs.clone();
    excess_budget.budget_bytes = MAX_CAPACITY_BYTES + CAPACITY_ALIGNMENT_BYTES;
    assert_eq!(
        excess_budget.validate(100),
        Err(FailureReason::ImpossibleBudget)
    );

    let mut resident_over_budget = valid_obs.clone();
    resident_over_budget.resident_bytes = 8192;
    resident_over_budget.budget_bytes = 4096;
    assert_eq!(
        resident_over_budget.validate(100),
        Err(FailureReason::ImpossibleBudget)
    );

    let mut available_over_budget = valid_obs.clone();
    available_over_budget.available_bytes = 8192;
    available_over_budget.budget_bytes = 4096;
    assert_eq!(
        available_over_budget.validate(100),
        Err(FailureReason::ImpossibleBudget)
    );

    Ok(())
}
