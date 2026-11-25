// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/tests/relay_and_status.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

// the hard question is "is the safe-mode marker present?",
// not "can this uid read its bytes?". Presence is the gate; content is a
// secondary validation. An unreadable marker must still report SafeMode.
#[test]
fn safe_mode_marker_presence_is_the_gate_even_when_content_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "ramshared-guardian-unreadable-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let safe = root.join("safe.json");
    let health = root.join("health.json");
    fs::write(&health, test_health_body("HEALTHY")).unwrap();

    fs::write(&safe, "not-json").unwrap();
    assert_eq!(
        guardian_state_from_files(&safe, &health, Duration::from_secs(15), &test_identity()),
        (GuardianState::Blocked, Some("safe_mode_invalid".into()))
    );

    // Deliberately invalid content: only presence-first semantics can yield
    // SafeMode when the read is denied.
    let mut perms = fs::metadata(&safe).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&safe, perms).unwrap();
    let denied = fs::read_to_string(&safe).is_err();
    let observed =
        guardian_state_from_files(&safe, &health, Duration::from_secs(15), &test_identity());
    let mut perms = fs::metadata(&safe).unwrap().permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&safe, perms).unwrap();

    if denied {
        assert_eq!(observed, (GuardianState::SafeMode, None));
    } else {
        // CAP_DAC_OVERRIDE can still read mode 0000 and must reject the
        // invalid content rather than invent SafeMode from bytes it saw.
        assert_eq!(
            observed,
            (GuardianState::Blocked, Some("safe_mode_invalid".into()))
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn guardian_health_accepts_a_windows_utf8_bom_and_rejects_malformed_json() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-guardian-bom-state-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let safe = root.join("safe.json");
    let health = root.join("health.json");

    fs::write(&health, format!("\u{feff}{}", test_health_body("HEALTHY"))).unwrap();
    assert_eq!(
        guardian_state_from_files(&safe, &health, Duration::from_secs(15), &test_identity()),
        (GuardianState::Healthy, None)
    );

    fs::write(&health, "\u{feff}not-json").unwrap();
    assert_eq!(
        guardian_state_from_files(&safe, &health, Duration::from_secs(15), &test_identity()),
        (
            GuardianState::Blocked,
            Some("guardian_state_invalid".into())
        )
    );
    fs::remove_dir_all(root).unwrap();
}

// AUD-40. A non-default distro is a first-class identity: the sealed
// config names it, the health file is located from that name, and the
// proof is accepted for that pair. No `Ubuntu-24.04` is privileged.
#[test]
fn sealed_guardian_state_accepts_a_non_default_distro() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-sealed-nondefault-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let (config, state_dir) = write_sealed_config(&root, "Debian", "S-1-5-21-1-2-3-1001");
    let safe = root.join("safe.json");

    let health = state_dir.join("Debian.health.json");
    fs::write(
        &health,
        health_json("Debian", "S-1-5-21-1-2-3-1001", "HEALTHY"),
    )
    .unwrap();
    assert_eq!(
        sealed_guardian_state_from(&config, &state_dir, &safe),
        (GuardianState::Healthy, None),
        "a non-default distro must resolve its own health proof"
    );

    // Ubuntu-22.04 is also not the default name and also works.
    let (config, state_dir) = write_sealed_config(&root, "Ubuntu-22.04", "S-1-5-21-1-2-3-1001");
    let health = state_dir.join("Ubuntu-22.04.health.json");
    fs::write(
        &health,
        health_json("Ubuntu-22.04", "S-1-5-21-1-2-3-1001", "HEALTHY"),
    )
    .unwrap();
    assert_eq!(
        sealed_guardian_state_from(&config, &state_dir, &safe),
        (GuardianState::Healthy, None)
    );

    fs::remove_dir_all(root).unwrap();
}

// AUD-40. Two users or two distros on one host are two identities. A
// health proof published for one is not a proof for the other, even when
// both are HEALTHY and fresh.
#[test]
fn sealed_guardian_state_separates_two_users_and_distros() {
    let root =
        std::env::temp_dir().join(format!("ramshared-sealed-twousers-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let safe = root.join("safe.json");

    // Alice's proof is on disk under the shared per-distro slot.
    let (config, state_dir) = write_sealed_config(&root, "Ubuntu-24.04", "S-1-5-21-1-2-3-1001");
    let health = state_dir.join("Ubuntu-24.04.health.json");
    fs::write(
        &health,
        health_json("Ubuntu-24.04", "S-1-5-21-1-2-3-1001", "HEALTHY"),
    )
    .unwrap();

    assert_eq!(
        sealed_guardian_state_from(&config, &state_dir, &safe),
        (GuardianState::Healthy, None)
    );

    // Bob is sealed on the same distro but is a different Windows user.
    // Alice's proof is not his.
    fs::write(
            &config,
            r#"{"schema_version":2,"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1002","task_name":"RamSharedWslGuardian.v1","guardian_policy":{}}"#,
        )
        .unwrap();
    let (state, error) = sealed_guardian_state_from(&config, &state_dir, &safe);
    assert_eq!(state, GuardianState::Blocked);
    assert_eq!(error.as_deref(), Some("guardian_state_identity_mismatch"));

    // A different distro under Alice is also a different identity.
    let (config, state_dir) = write_sealed_config(&root, "Debian", "S-1-5-21-1-2-3-1001");
    let health = state_dir.join("Debian.health.json");
    fs::write(
        &health,
        health_json("Debian", "S-1-5-21-1-2-3-1001", "HEALTHY"),
    )
    .unwrap();
    assert_eq!(
        sealed_guardian_state_from(&config, &state_dir, &safe),
        (GuardianState::Healthy, None)
    );

    fs::remove_dir_all(root).unwrap();
}

// AUD-40. A renamed or unregistered distro does not inherit the previous
// identity's proof. The location is derived from the sealed name, so a
// rename points at a different file; if that file is absent the state is
// unavailable, never the old identity's HEALTHY.
#[test]
fn sealed_guardian_state_refuses_a_renamed_or_unregistered_distro() {
    let root =
        std::env::temp_dir().join(format!("ramshared-sealed-renamed-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let safe = root.join("safe.json");

    // The old name's proof is on disk and HEALTHY.
    let (config, state_dir) = write_sealed_config(&root, "Ubuntu-24.04", "S-1-5-21-1-2-3-1001");
    let health = state_dir.join("Ubuntu-24.04.health.json");
    fs::write(
        &health,
        health_json("Ubuntu-24.04", "S-1-5-21-1-2-3-1001", "HEALTHY"),
    )
    .unwrap();
    assert_eq!(
        sealed_guardian_state_from(&config, &state_dir, &safe),
        (GuardianState::Healthy, None)
    );

    // The distro is renamed (or never registered under the new name).
    fs::write(
            &config,
            r#"{"schema_version":2,"distro":"Ubuntu-99.99","user_sid":"S-1-5-21-1-2-3-1001","task_name":"RamSharedWslGuardian.v1","guardian_policy":{}}"#,
        )
        .unwrap();
    let (state, error) = sealed_guardian_state_from(&config, &state_dir, &safe);
    assert_eq!(state, GuardianState::Blocked);
    assert_eq!(
        error.as_deref(),
        Some("guardian_state_unavailable"),
        "a renamed distro must not read the previous name's proof"
    );

    fs::remove_dir_all(root).unwrap();
}

// AUD-40. A stale prior-boot proof is not a current-boot proof, even when
// the identity pair matches. Boot identity is preserved as its own axis.
#[test]
fn sealed_guardian_state_refuses_a_stale_prior_boot_proof() {
    let root =
        std::env::temp_dir().join(format!("ramshared-sealed-priorboot-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let safe = root.join("safe.json");
    let (config, state_dir) = write_sealed_config(&root, "Ubuntu-24.04", "S-1-5-21-1-2-3-1001");

    // A prior-boot proof that names the right pair but an old boot.
    let health = state_dir.join("Ubuntu-24.04.health.json");
    fs::write(
            &health,
            r#"{"schema_version":3,"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY","boot_id":"11111111-2222-3333-4444-555555555555"}"#,
        )
        .unwrap();

    // The freshness window is what expires a prior-boot proof on this
    // path. A proof written now is within it; a proof left behind by the
    // previous boot is not (its mtime ages out). Backdate it to prove the
    // expiry rather than only asserting the field differs.
    let old = SystemTime::now() - Duration::from_secs(3600);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&health)
        .and_then(|file| file.set_modified(old).map(|_| file));
    match file {
        Ok(_) => {
            let (state, error) = sealed_guardian_state_from(&config, &state_dir, &safe);
            assert_eq!(state, GuardianState::Blocked);
            assert_eq!(error.as_deref(), Some("guardian_state_stale"));
        }
        Err(error) => {
            panic!("set_modified failed, cannot prove prior-boot staleness: {error}");
        }
    }

    fs::remove_dir_all(root).unwrap();
}

// AUD-40. Malicious identity text never becomes a path or a `wsl`
// argument. The sealed config is refused before any location is derived.
#[test]
fn sealed_guardian_state_refuses_malicious_identity_text() {
    let root =
        std::env::temp_dir().join(format!("ramshared-sealed-malicious-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let safe = root.join("safe.json");
    let (config, state_dir) = write_sealed_config(&root, "Ubuntu-24.04", "S-1-5-21-1-2-3-1001");

    for (distro, sid, expected) in [
        (
            "../etc",
            "S-1-5-21-1-2-3-1001",
            "guardian_identity_invalid_distro",
        ),
        (
            "Ubuntu-24.04; reboot",
            "S-1-5-21-1-2-3-1001",
            "guardian_identity_invalid_distro",
        ),
        (
            "$(reboot)",
            "S-1-5-21-1-2-3-1001",
            "guardian_identity_invalid_distro",
        ),
        (
            "Ubuntu-24.04",
            "S-1-5-21-1-2-3-1001; rm -rf /",
            "guardian_identity_invalid_user_sid",
        ),
        ("Ubuntu-24.04", "root", "guardian_identity_invalid_user_sid"),
        ("Ubuntu-24.04", "", "guardian_identity_invalid_user_sid"),
    ] {
        fs::write(
                &config,
                format!(
                    r#"{{"schema_version":2,"distro":"{distro}","user_sid":"{sid}","task_name":"RamSharedWslGuardian.v1","guardian_policy":{{}}}}"#
                ),
            )
            .unwrap();
        let (state, error) = sealed_guardian_state_from(&config, &state_dir, &safe);
        assert_eq!(state, GuardianState::Blocked, "case ({distro:?}, {sid:?})");
        assert_eq!(
            error.as_deref(),
            Some(expected),
            "case ({distro:?}, {sid:?})"
        );
    }

    // A missing or malformed sealed config is unavailable, never a
    // default distro name.
    fs::remove_file(&config).unwrap();
    let (state, error) = sealed_guardian_state_from(&config, &state_dir, &safe);
    assert_eq!(state, GuardianState::Blocked);
    assert_eq!(error.as_deref(), Some("guardian_identity_unavailable"));

    fs::write(&config, "not-json").unwrap();
    let (state, error) = sealed_guardian_state_from(&config, &state_dir, &safe);
    assert_eq!(state, GuardianState::Blocked);
    assert_eq!(error.as_deref(), Some("guardian_identity_malformed"));

    fs::remove_dir_all(root).unwrap();
}

// AUD-40. A mismatched health proof is refused: a proof that names
// another user, another distro, or omits either half of the pair is not
// a weaker proof of this identity — it is not this identity's proof.
#[test]
fn sealed_guardian_state_refuses_a_mismatched_health_proof() {
    let root =
        std::env::temp_dir().join(format!("ramshared-sealed-mismatch-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let safe = root.join("safe.json");
    let (config, state_dir) = write_sealed_config(&root, "Ubuntu-24.04", "S-1-5-21-1-2-3-1001");
    let health = state_dir.join("Ubuntu-24.04.health.json");

    for body in [
            r#"{"schema_version":3,"distro":"Debian","user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY"}"#.to_string(),
            r#"{"schema_version":3,"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1002","state":"HEALTHY"}"#.to_string(),
            r#"{"schema_version":3,"distro":"Ubuntu-24.04","state":"HEALTHY"}"#.to_string(),
            r#"{"schema_version":3,"user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY"}"#.to_string(),
            r#"{"schema_version":3,"distro":"Ubuntu-24.04 ","user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY"}"#.to_string(),
        ] {
            fs::write(&health, &body).unwrap();
            let (state, error) = sealed_guardian_state_from(&config, &state_dir, &safe);
            assert_eq!(state, GuardianState::Blocked, "body {body}");
            assert_eq!(
                error.as_deref(),
                Some("guardian_state_identity_mismatch"),
                "body {body}"
            );
        }

    // The matching pair is still accepted after the refusals.
    fs::write(&health, test_health_body("HEALTHY")).unwrap();
    assert_eq!(
        sealed_guardian_state_from(&config, &state_dir, &safe),
        (GuardianState::Healthy, None)
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stale_foreign_or_malformed_cache_or_supervisor_status_is_never_green() {
    let fresh_cache = serde_json::json!({
        "schema_version": 1,
        "daemon_instance_id": "daemon-1",
        "written_at_unix_ms": 1_000,
        "ok": true,
        "origin_state": "READY",
        "cache_state": "ACTIVE",
    });
    let mut fresh_supervisor = valid_supervisor_status_v3();
    fresh_supervisor["control_state"] = serde_json::json!("HEALTHY");
    fresh_supervisor["healthy_samples"] = serde_json::json!(60);
    fresh_supervisor["action_results"] = serde_json::json!([]);
    assert!(control_plane_status_is_current(
        &fresh_cache,
        &fresh_supervisor,
        "daemon-1",
        1_001,
    ));

    for (cache, supervisor, now_unix_ms) in [
        (
            serde_json::json!({"origin_state":"READY","cache_state":"ACTIVE"}),
            fresh_supervisor.clone(),
            1_001,
        ),
        (
            fresh_cache.clone(),
            {
                let mut foreign = fresh_supervisor.clone();
                foreign["daemon_instance_id"] = serde_json::json!("foreign-daemon");
                foreign
            },
            1_001,
        ),
        (
            serde_json::json!({
                "schema_version": 1,
                "daemon_instance_id": "daemon-1",
                "written_at_unix_ms": 0,
                "ok": true,
                "origin_state": "READY",
                "cache_state": "ACTIVE",
            }),
            fresh_supervisor.clone(),
            16_001,
        ),
    ] {
        assert!(
            !control_plane_status_is_current(&cache, &supervisor, "daemon-1", now_unix_ms),
            "stale, foreign, or malformed status was accepted"
        );
    }
}

/// The cache-status document gate is the one place that answers "is this
/// the live daemon's current document?". Readiness calls it before reducing
/// any field, so this is what stands between a leftover file on disk and a
/// confident `DriverReported` headroom figure.
#[test]
fn cache_status_gate_admits_only_the_live_daemons_current_document() {
    let live = serde_json::json!({
        "schema_version": 1,
        "daemon_instance_id": "daemon-1",
        "written_at_unix_ms": 1_000,
        "ok": true,
        "origin_state": "READY",
        "cache_state": "ACTIVE",
        // A fully-formed budget of the kind readiness reduces. It does not
        // buy the document anything: the gate answers about the document,
        // and a field of a foreign one is not that field of the live one.
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "vulkan", "key": "0", "luid": null },
            "budget_bytes": 4 * 1024 * 1024 * 1024i64,
            "used_bytes": 0,
            "available_bytes": 4 * 1024 * 1024 * 1024i64,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1_000,
        },
    });
    assert!(cache_status_is_current_daemon(&live, "daemon-1", 1_001));

    // The age bound is inclusive: the whole window is current.
    assert!(cache_status_is_current_daemon(
        &live,
        "daemon-1",
        1_000 + CONTROL_STATUS_MAX_AGE_MS
    ));

    let mut foreign = live.clone();
    foreign["daemon_instance_id"] = serde_json::json!("daemon-2");
    let mut future = live.clone();
    future["written_at_unix_ms"] = serde_json::json!(2_000);
    let mut wrong_schema = live.clone();
    wrong_schema["schema_version"] = serde_json::json!(2);
    let mut missing_written = live.clone();
    missing_written
        .as_object_mut()
        .expect("fixture is an object")
        .remove("written_at_unix_ms");
    // The daemon publishes `null` when its clock read fails — the third
    // state, not a zero. It has to be refused whole here for the same
    // reason a missing key is: neither establishes that the measurements
    // are live. What must not happen is reading the absent figure as `0`
    // and admitting the document as a write at the Unix epoch.
    let mut unmeasured_written = live.clone();
    unmeasured_written["written_at_unix_ms"] = serde_json::Value::Null;
    let mut missing_shape = live.clone();
    missing_shape
        .as_object_mut()
        .expect("fixture is an object")
        .remove("origin_state");

    for (name, document, now_unix_ms) in [
        ("foreign instance", &foreign, 1_001u64),
        ("aged past the window", &live, 16_001),
        ("written into the future", &future, 1_001),
        ("wrong schema_version", &wrong_schema, 1_001),
        ("missing written_at_unix_ms", &missing_written, 1_001),
        ("unmeasured written_at_unix_ms", &unmeasured_written, 1_001),
        ("missing shape field", &missing_shape, 1_001),
    ] {
        assert!(
            !cache_status_is_current_daemon(document, "daemon-1", now_unix_ms),
            "{name} was admitted"
        );
    }
}

#[test]
fn cache_and_supervisor_freshness_are_reported_independently() {
    let fresh_cache = serde_json::json!({
        "schema_version": 1,
        "daemon_instance_id": "daemon-1",
        "written_at_unix_ms": 1_000,
        "ok": true,
        "origin_state": "READY",
        "cache_state": "ACTIVE",
    });
    let fresh_supervisor = valid_supervisor_status_v3();

    assert_eq!(
        control_plane_status_freshness(Some(&fresh_cache), None, "daemon-1", 1_001,),
        (true, false),
        "fresh cache telemetry must remain visible when supervisor telemetry is absent",
    );
    assert_eq!(
        control_plane_status_freshness(None, Some(&fresh_supervisor), "daemon-1", 1_001,),
        (false, true),
        "fresh supervisor telemetry must remain visible when cache telemetry is absent",
    );
}

#[test]
fn supervisor_status_v3_with_ordered_action_results_is_current() {
    let supervisor = valid_supervisor_status_v3();
    assert!(supervisor_status_shape_is_valid(&supervisor));
    assert!(supervisor_status_matches_current_daemon(
        &supervisor,
        "daemon-1",
        1_001,
    ));
}

#[test]
fn supervisor_status_v2_or_missing_action_results_is_refused() {
    let mut legacy = valid_supervisor_status_v3();
    legacy["schema_version"] = serde_json::json!(2);
    assert!(!supervisor_status_shape_is_valid(&legacy));
    assert!(!supervisor_status_matches_current_daemon(
        &legacy, "daemon-1", 1_001,
    ));

    let mut missing = valid_supervisor_status_v3();
    missing.as_object_mut().unwrap().remove("action_results");
    assert!(!supervisor_status_shape_is_valid(&missing));
}

#[test]
fn supervisor_status_malformed_foreign_and_stale_states_are_refused() {
    let mut malformed = valid_supervisor_status_v3();
    malformed["action_results"][0]["error"] = serde_json::json!("unexpected");
    assert!(!supervisor_status_shape_is_valid(&malformed));

    let mut failed_without_error = valid_supervisor_status_v3();
    failed_without_error["action_results"][1]["error"] = serde_json::Value::Null;
    assert!(!supervisor_status_shape_is_valid(&failed_without_error));

    let mut failed_with_empty_error = valid_supervisor_status_v3();
    failed_with_empty_error["action_results"][1]["error"] = serde_json::json!("");
    assert!(!supervisor_status_shape_is_valid(&failed_with_empty_error));

    let mut failed_with_unbounded_error = valid_supervisor_status_v3();
    failed_with_unbounded_error["action_results"][1]["error"] =
        serde_json::json!("x".repeat(SUPERVISOR_ACTION_ERROR_MAX_BYTES + 1));
    assert!(!supervisor_status_shape_is_valid(
        &failed_with_unbounded_error
    ));

    let foreign = valid_supervisor_status_v3();
    assert!(!supervisor_status_matches_current_daemon(
        &foreign,
        "foreign-daemon",
        1_001,
    ));

    let stale = valid_supervisor_status_v3();
    assert!(!supervisor_status_matches_current_daemon(
        &stale, "daemon-1", 16_001,
    ));
}

#[test]
fn supervisor_status_unknown_duplicate_reordered_or_legacy_actions_are_refused() {
    let mut unknown = valid_supervisor_status_v3();
    unknown["action_results"][0]["action"] = serde_json::json!("foreign_action");
    assert!(!supervisor_status_shape_is_valid(&unknown));

    let mut duplicate = valid_supervisor_status_v3();
    duplicate["action_results"][1]["action"] = serde_json::json!("close_admission");
    assert!(!supervisor_status_shape_is_valid(&duplicate));

    let mut reordered = valid_supervisor_status_v3();
    reordered["action_results"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    assert!(!supervisor_status_shape_is_valid(&reordered));

    let mut legacy = valid_supervisor_status_v3();
    legacy["last_action"] = serde_json::json!("request_reclaim");
    assert!(!supervisor_status_shape_is_valid(&legacy));

    let mut extra_result_field = valid_supervisor_status_v3();
    extra_result_field["action_results"][0]["detail"] = serde_json::json!("foreign");
    assert!(!supervisor_status_shape_is_valid(&extra_result_field));

    let mut extra_status_field = valid_supervisor_status_v3();
    extra_status_field["foreign"] = serde_json::json!(true);
    assert!(!supervisor_status_shape_is_valid(&extra_status_field));
}

#[test]
fn resolve_transport_explicit_and_auto_on_wsl() {
    // Named transports are a preference passthrough: this function never
    // refuses, because `check_transport` owns refusal. Only `auto` is
    // chosen here.
    assert_eq!(resolve_transport(Transport::Nbd), Transport::Nbd);
    assert_eq!(resolve_transport(Transport::Ublk), Transport::Ublk);
    if is_wsl2() {
        assert_eq!(resolve_transport(Transport::Auto), Transport::Nbd);
    } else {
        assert!(matches!(
            resolve_transport(Transport::Auto),
            Transport::Nbd | Transport::Ublk
        ));
    }
}

#[test]
fn bare_up_zram_follows_cascade_conf_before_env() {
    // DT-3: the host policy file outranks the environment. A bare `up` on
    // a host whose `cascade.conf` says 2048 must not silently start 1024.
    clear_test_seams();
    set_test_mb(Some(("RAMSHARED_ZRAM_MIB", 512)));
    set_test_cascade_conf(Some(Ok("ZRAM_MIB=2048\n".to_string())));
    let a = parse(&[]).unwrap();
    assert_eq!(a.zram_mb, 2048);
    clear_test_seams();
}

#[test]
fn bare_up_zram_falls_back_to_env_then_builtin() {
    clear_test_seams();
    set_test_mb(Some(("RAMSHARED_ZRAM_MIB", 1536)));
    set_test_cascade_conf(Some(Ok("# no sizing keys\n".to_string())));
    let a = parse(&[]).unwrap();
    assert_eq!(a.zram_mb, 1536);

    set_test_mb(None);
    set_test_cascade_conf(Some(Ok(String::new())));
    let a = parse(&[]).unwrap();
    assert_eq!(a.zram_mb, 1024);
    clear_test_seams();
}

#[test]
fn bare_up_zram_explicit_flag_still_outranks_the_policy_file() {
    clear_test_seams();
    set_test_cascade_conf(Some(Ok("ZRAM_MIB=2048\n".to_string())));
    let a = parse(&["--zram", "0"]).unwrap();
    assert_eq!(a.zram_mb, 0);
    clear_test_seams();
}
