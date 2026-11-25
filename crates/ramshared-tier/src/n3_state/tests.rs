// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/n3_state/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn test_initial_state() {
    let machine = LeaseMachine::new();
    assert_eq!(machine.lease_state(), LeaseState::Absent);
    assert_eq!(machine.preflight_state(), PreflightState::HostUnavailable);
}

#[test]
fn test_request_demotion() {
    let mut model = PreflightModel::new();
    // Requesting demotion when not constrained should be an invalid transition
    let decision = model.request_demotion();
    assert_eq!(
        decision.action,
        PreflightAction::Unavailable(FailureReason::StateTransition(
            StateTransitionError::IllegalPreflight {
                expected: Some(PreflightState::Constrained),
                actual: model.state
            }
        ))
    );

    // Explicitly set state to other non-constrained values
    model.state = PreflightState::Observing;
    let decision2 = model.request_demotion();
    assert_eq!(
        decision2.action,
        PreflightAction::Unavailable(FailureReason::StateTransition(
            StateTransitionError::IllegalPreflight {
                expected: Some(PreflightState::Constrained),
                actual: PreflightState::Observing
            }
        ))
    );

    // Let's create a constrained state by observing an empty budget
    let event_id = EventId::new(b"event-1").unwrap_or_else(|_| panic!("failed to create event_id"));
    let adapter_id = AdapterId::new(b"adapter-1").unwrap();

    let observation = HostObservation::new(
        N3_SCHEMA_VERSION,
        1,
        adapter_id,
        Authority::Host,
        0, // Budget 0 -> Constrained
        0,
        0,
        100,
        MAX_OBSERVATION_AGE,
        event_id,
        Vec::new(),
    );

    let decision = model.observe(observation, 100);
    assert_eq!(decision.state, PreflightState::Constrained);

    // Now request demotion should succeed
    let decision = model.request_demotion();
    assert_eq!(decision.state, PreflightState::DemotionRequested);
    assert_eq!(decision.action, PreflightAction::DemotionRequested);
}

#[test]
fn test_restart_record_serialization() {
    let lease_id = LeaseId::new(b"lease-abc").unwrap();
    let checkpoint = GenerationCheckpoint {
        lease_id: lease_id.clone(),
        generation: 42,
    };

    let record = RestartRecord::host(10, vec![checkpoint.clone()]).unwrap();
    let bytes = record.to_bytes();

    let decoded = RestartRecord::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.host_epoch(), 10);

    let checkpoints = decoded.checkpoints();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].lease_id, lease_id);
    assert_eq!(checkpoints[0].generation, 42);
}

#[test]
fn test_invalid_revoke_transition() {
    let mut machine = LeaseMachine::new();
    let lease_id = LeaseId::new(b"lease-1").unwrap_or_else(|_| panic!("failed to create lease_id"));
    let event_id = EventId::new(b"event-1").unwrap_or_else(|_| panic!("failed to create event_id"));

    let revoke = Revoke::host(lease_id, 1, event_id, 200);

    // Applying Revoke to an Absent state should result in an invalid transition error
    let decision = machine.receive_revoke(revoke);
    match decision {
        ProtocolDecision::FailAck(fail_ack) => {
            assert_eq!(
                fail_ack.reason,
                FailureReason::StateTransition(StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Granted),
                    actual: StateTag::Absent
                })
            );
        }
        _ => panic!("Expected FailAck"),
    }

    // The machine's state should now be Failed(InvalidTransition)
    assert_eq!(
        machine.lease_state(),
        LeaseState::Failed(FailureReason::StateTransition(
            StateTransitionError::IllegalTransition {
                expected: Some(StateTag::Granted),
                actual: StateTag::Absent
            }
        ))
    );
}

fn host_obs(host_epoch: u64, budget: u64, now: u64, tag: &[u8]) -> HostObservation {
    HostObservation::new(
        N3_SCHEMA_VERSION,
        host_epoch,
        AdapterId::new(b"adapter-1").unwrap(),
        Authority::Host,
        budget,
        0,
        0,
        now,
        MAX_OBSERVATION_AGE,
        EventId::new(tag).unwrap(),
        Vec::new(),
    )
}

#[test]
fn restore_grant_ack_and_revoke_refusals_fail_closed() {
    let mut non_fresh = LeaseMachine::new();
    let _ = non_fresh.observe(host_obs(2, 8192, 100, b"obs-1"), 100);
    let rec = RestartRecord::host(2, Vec::new()).unwrap();
    assert!(matches!(
        non_fresh.restore_restart_record(rec),
        Err(FailureReason::StateTransition(
            StateTransitionError::IllegalTransition { .. }
        ))
    ));
    let mut fresh = LeaseMachine::new();
    assert_eq!(
        fresh.restore_restart_bytes(b"bad"),
        Err(FailureReason::MalformedRecord)
    );

    // Stale grant epoch relative to restored restart epoch fails closed.
    let mut stale = LeaseMachine::new();
    stale
        .restore_restart_record(RestartRecord::host(5, Vec::new()).unwrap())
        .unwrap();
    let _ = stale.observe(host_obs(4, 8192, 100, b"obs-stale"), 100);
    let lease = LeaseId::new(b"lease-1").unwrap();
    let stale_grant = Grant::host(
        lease.clone(),
        1,
        EventId::new(b"grant-stale").unwrap(),
        4096,
        4,
        100,
        200,
    );
    assert!(matches!(
        stale.receive_grant(stale_grant, 100),
        ProtocolDecision::FailAck(_)
    ));

    // Unexpected expected_state & non-Absent machine state.
    let mut bad_expected = LeaseMachine::new();
    let _ = bad_expected.observe(host_obs(1, 8192, 100, b"obs-exp"), 100);
    let g_exp = Grant::host(
        lease.clone(),
        1,
        EventId::new(b"g-exp").unwrap(),
        4096,
        1,
        100,
        200,
    )
    .with_expected_state(StateTag::Granted);
    assert!(matches!(
        bad_expected.receive_grant(g_exp, 100),
        ProtocolDecision::FailAck(_)
    ));

    // Negotiation -> wrong state grant -> mismatched ack -> valid ack via host_accepts_grant_ack.
    let mut m = LeaseMachine::new();
    assert!(matches!(
        m.accept_grant_ack(GrantAck {
            contract_version: N3_SCHEMA_VERSION,
            lease_id: lease.clone(),
            generation: 1,
            event_id: EventId::new(b"missing-ack").unwrap(),
        }),
        ProtocolDecision::FailAck(_)
    ));
    let mut m2 = LeaseMachine::new();
    let _ = m2.observe(host_obs(1, 8192, 100, b"obs-2"), 100);
    let g1 = Grant::host(
        lease.clone(),
        1,
        EventId::new(b"g-1").unwrap(),
        4096,
        1,
        100,
        200,
    );
    let ack = match m2.receive_grant(g1.clone(), 100) {
        ProtocolDecision::GrantAck(a) => a,
        other => panic!("expected GrantAck, got {other:?}"),
    };
    let mut bad_g1 = g1.clone();
    bad_g1.capacity_bytes = 8192;
    assert!(matches!(
        m2.receive_grant(bad_g1, 100),
        ProtocolDecision::FailAck(_)
    ));
    let g2 = Grant::host(
        lease.clone(),
        2,
        EventId::new(b"g-2").unwrap(),
        4096,
        1,
        100,
        200,
    );
    assert!(matches!(
        m2.receive_grant(g2, 100),
        ProtocolDecision::FailAck(_)
    ));
    let mut wrong_ack = ack.clone();
    wrong_ack.generation = 99;
    let mut m3 = LeaseMachine::new();
    let _ = m3.observe(host_obs(1, 8192, 100, b"obs-3"), 100);
    let ack3 = match m3.receive_grant(g1, 100) {
        ProtocolDecision::GrantAck(a) => a,
        other => panic!("expected GrantAck, got {other:?}"),
    };
    let mut bad_ack3 = ack3.clone();
    bad_ack3.generation = 99;
    assert!(matches!(
        m3.accept_grant_ack(bad_ack3),
        ProtocolDecision::FailAck(_)
    ));

    let mut m4 = LeaseMachine::new();
    let _ = m4.observe(host_obs(1, 8192, 100, b"obs-4"), 100);
    let ack4 = match m4.receive_grant(
        Grant::host(
            lease.clone(),
            1,
            EventId::new(b"g-4").unwrap(),
            4096,
            1,
            100,
            200,
        ),
        100,
    ) {
        ProtocolDecision::GrantAck(a) => a,
        other => panic!("expected GrantAck, got {other:?}"),
    };
    assert!(matches!(
        m4.host_accepts_grant_ack(ack4),
        ProtocolDecision::Accepted(_)
    ));
    let bad_revoke = Revoke::host(lease, 1, EventId::new(b"r-exp").unwrap(), 200)
        .with_expected_state(StateTag::Absent);
    assert!(matches!(
        m4.apply_host_event(HostEvent::Revoke(bad_revoke), 100),
        ProtocolDecision::FailAck(_)
    ));

    // History overflow on grants and revokes.
    let mut ov = LeaseMachine::new();
    for i in 0..MAX_PROTOCOL_EVENT_HISTORY {
        let ev = EventId::new(format!("ev-{i}").as_bytes()).unwrap();
        let _ = ov.receive_revoke(Revoke::host(LeaseId::new(b"lease-ov").unwrap(), 1, ev, 200));
    }
    let overflow_grant = Grant::host(
        LeaseId::new(b"lease-ov").unwrap(),
        1,
        EventId::new(b"ev-over").unwrap(),
        4096,
        1,
        100,
        200,
    );
    assert!(matches!(
        ov.receive_grant(overflow_grant, 100),
        ProtocolDecision::FailAck(_)
    ));
    let overflow_revoke = Revoke::host(
        LeaseId::new(b"lease-ov").unwrap(),
        1,
        EventId::new(b"ev-over-rev").unwrap(),
        200,
    );
    assert!(matches!(
        ov.receive_revoke(overflow_revoke),
        ProtocolDecision::FailAck(_)
    ));
}

#[test]
fn flight_counters_drain_prereqs_and_lifecycle_events_fail_closed() {
    let mut m = LeaseMachine::new();
    let lease = LeaseId::new(b"lease-io").unwrap();
    let _ = m.observe(host_obs(1, 8192, 100, b"obs-io"), 100);
    // Draining without active lease fails closed.
    assert!(matches!(m.drain(110), ProtocolDecision::FailAck(_)));

    let mut m = LeaseMachine::new();
    let _ = m.observe(host_obs(1, 8192, 100, b"obs-io2"), 100);
    let ack = match m.receive_grant(
        Grant::host(
            lease.clone(),
            1,
            EventId::new(b"g-io").unwrap(),
            4096,
            1,
            100,
            200,
        ),
        100,
    ) {
        ProtocolDecision::GrantAck(a) => a,
        other => panic!("expected GrantAck, got {other:?}"),
    };
    assert!(matches!(
        m.accept_grant_ack(ack),
        ProtocolDecision::Accepted(_)
    ));

    assert_eq!(m.complete_io(), Err(FailureReason::UnknownInFlight));
    assert_eq!(
        m.set_in_flight(MAX_IN_FLIGHT + 1),
        Err(FailureReason::UnknownInFlight)
    );
    assert_eq!(
        m.set_callbacks_pending(MAX_IN_FLIGHT + 1),
        Err(FailureReason::UnknownInFlight)
    );
    m.unknown_in_flight = false;
    m.set_in_flight(MAX_IN_FLIGHT).unwrap();
    assert_eq!(m.begin_io(), Err(FailureReason::UnknownInFlight));
    // Granted without revoke fails drain closed.
    assert!(matches!(m.drain(110), ProtocolDecision::FailAck(_)));

    // Revoked while unknown_in_flight triggers UnknownInFlight on drain; wrong state fails too.
    let mut m2 = LeaseMachine::new();
    let _ = m2.observe(host_obs(1, 8192, 100, b"obs-unk"), 100);
    let ack2 = match m2.receive_grant(
        Grant::host(
            lease.clone(),
            1,
            EventId::new(b"g-unk").unwrap(),
            4096,
            1,
            100,
            200,
        ),
        100,
    ) {
        ProtocolDecision::GrantAck(a) => a,
        other => panic!("expected GrantAck, got {other:?}"),
    };
    let _ = m2.accept_grant_ack(ack2);
    let _ = m2.receive_revoke(Revoke::host(
        lease.clone(),
        1,
        EventId::new(b"r-unk").unwrap(),
        200,
    ));
    m2.mark_unknown_in_flight();
    assert!(matches!(m2.drain(110), ProtocolDecision::FailAck(_)));
    assert!(matches!(m2.drain(110), ProtocolDecision::FailAck(_)));

    // Pending grant fail_for_active + idle LifecycleEvent variants.
    for ev in [
        LifecycleEvent::ChannelLoss,
        LifecycleEvent::LeaseExpired,
        LifecycleEvent::WslRestart,
        LifecycleEvent::Suspend,
        LifecycleEvent::DriverUpgrade,
        LifecycleEvent::GuestCrash,
    ] {
        let mut idle = LeaseMachine::new();
        assert!(matches!(
            idle.apply_host_event(HostEvent::Lifecycle(ev), 100),
            ProtocolDecision::FailAck(_)
        ));
    }

    let mut pend = LeaseMachine::new();
    let _ = pend.observe(host_obs(1, 8192, 100, b"obs-pend"), 100);
    let _ = pend.receive_grant(
        Grant::host(
            lease,
            1,
            EventId::new(b"g-pend").unwrap(),
            4096,
            1,
            100,
            200,
        ),
        100,
    );
    assert!(matches!(
        pend.fail_closed(LifecycleEvent::Suspend),
        ProtocolDecision::FailAck(_)
    ));
}
