// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/tests/lifecycle_extra.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn malformed_cascade_conf_refuses_a_bare_up() {
    // Fail closed, same as `boot`: a policy file the chain cannot parse is
    // a refusal, not a silent fall back to 1024.
    clear_test_seams();
    set_test_cascade_conf(Some(Ok("ZRAM_MIB=not-a-number\n".to_string())));
    let err = parse(&[]).unwrap_err();
    assert!(err.to_string().contains("ZRAM_MIB"), "got: {err}");
    clear_test_seams();
}

#[test]
fn refuse_ghost_state_with_injected_swaps() {
    clear_test_seams();
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/nbd0\\040(deleted) partition 1 1 100\n",
    ));
    let err = refuse_ghost_swap_state().unwrap_err();
    clear_test_seams();
    assert!(err.to_string().contains("fantasma") || err.to_string().contains("deleted"));
}

#[test]
fn refuse_half_cascade_when_vram_live_without_health() {
    clear_test_seams();
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/nbd0 partition 1024 0 100\n",
    ));
    let e = read_swaps().expect("strict injected swaps");
    if !cascade_already_healthy(&e) {
        let err = refuse_half_cascade(&e).unwrap_err();
        assert!(err.to_string().contains("metade") || err.to_string().contains("down"));
    }
    clear_test_seams();
}

#[test]
fn try_recover_refuses_dirty_backend() {
    clear_test_seams();
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/nbd0 partition 1024 500 100\n",
    ));
    let err = refuse_unbound_managed_devices().unwrap_err();
    clear_test_seams();
    assert!(
        err.to_string().contains("used_kb") || err.to_string().contains("orphan"),
        "{err}"
    );
}

#[test]
fn try_recover_zero_used_unbound_device_refuses_without_mutation() {
    clear_test_seams();
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/nbd0 partition 1024 0 100\n",
    ));
    let err = refuse_unbound_managed_devices().unwrap_err();
    clear_test_seams();
    assert!(
        err.to_string().contains("detection-only") || err.to_string().contains("lifecycle binding")
    );
}

#[test]
fn try_recover_zero_used_never_consumes_shell_mutations() {
    clear_test_seams();
    push_sh("*", Err("shell mutation must not run"));
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/nbd0 partition 1024 0 100\n\
             /dev/sdc partition 999 0 -2\n",
    ));
    let error = refuse_unbound_managed_devices().unwrap_err();
    let remaining = SH_SCRIPT.with(|queue| queue.borrow().len());
    clear_test_seams();
    assert!(error.to_string().contains("detection-only"));
    assert_eq!(remaining, 1, "unbound recovery consumed a shell mutation");
}

#[test]
fn demote_reader_rejects_links_fifos_and_oversized_files() {
    let root = std::env::temp_dir().join(format!("demote-reader-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("status");
    std::fs::write(&path, br#"{"total":1,"in_progress":false}"#).unwrap();
    assert_eq!(
        super::super::status_view::read_demote_snapshot_at(&path).in_progress,
        Some(false)
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, [0xff]).unwrap();
    assert_eq!(
        super::super::status_view::read_demote_snapshot_at(&path).in_progress,
        None
    );
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("missing", &path).unwrap();
    assert_eq!(
        super::super::status_view::read_demote_snapshot_at(&path).in_progress,
        None
    );
    std::fs::remove_file(&path).unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &path,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    assert_eq!(
        super::super::status_view::read_demote_snapshot_at(&path).in_progress,
        None
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::write(
        &path,
        vec![b' '; super::super::health_documents::HEALTH_DOCUMENT_MAX_BYTES + 1],
    )
    .unwrap();
    assert_eq!(
        super::super::status_view::read_demote_snapshot_at(&path).in_progress,
        None
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_timestamp_falls_back_to_a_numeric_epoch() {
    clear_test_seams();
    push_sh("date", Err("fixture unavailable"));
    let stamp = super::super::status_view::status_timestamp();
    clear_test_seams();
    assert!(stamp.parse::<u64>().is_ok());
}

#[test]
fn demote_status_rejects_malformed_or_duplicate_json() {
    for body in [
        r#"{"total":2,"in_progress":false"#,
        r#"{"total":2,"in_progress":falsex}"#,
        r#"{"total":2,"in_progress":true,"in_progress":false}"#,
    ] {
        assert!(parse_demote_status_file(body).is_none(), "body {body}");
    }
    let parsed =
        parse_demote_status_file(r#"{"total":2,"last_reason":"line\nnext","in_progress":false}"#)
            .unwrap();
    assert_eq!(parsed.last_reason.as_deref(), Some("line\nnext"));
}

#[test]
fn parse_demote_status_file_roundtrip_shape() {
    let j = r#"{"total":2,"last_reason":"Latency","in_progress":true}"#;
    let d = parse_demote_status_file(j).expect("parse");
    assert_eq!(d.total, Some(2));
    assert_eq!(d.last_reason.as_deref(), Some("Latency"));
    assert_eq!(d.in_progress, Some(true));
    let j2 = r#"{"total":0,"last_reason":null,"in_progress":false}"#;
    let d2 = parse_demote_status_file(j2).unwrap();
    assert_eq!(d2.total, Some(0));
    assert!(d2.last_reason.is_none());
    assert_eq!(d2.in_progress, Some(false));
}

#[test]
fn parse_demote_status_file_is_witness_backed_for_in_progress_tokens() {
    let t = parse_demote_status_file(r#"{"total":2,"in_progress":true}"#).unwrap();
    assert_eq!(t.in_progress, Some(true));
    let f = parse_demote_status_file(r#"{"total":2,"in_progress":false}"#).unwrap();
    assert_eq!(f.in_progress, Some(false));
}

#[test]
fn parse_demote_status_file_never_publishes_a_measured_refutation_from_a_missing_in_progress_key() {
    let d = parse_demote_status_file(r#"{"total":2,"last_reason":"Latency"}"#)
        .expect("total present admits the document");
    assert_eq!(d.in_progress, None);
    assert_ne!(d.in_progress, Some(false));
}

#[test]
fn parse_demote_status_file_never_publishes_a_measured_refutation_from_a_non_bool_in_progress_value()
 {
    for body in [
        r#"{"total":2,"in_progress":null}"#,
        r#"{"total":2,"in_progress":1}"#,
        r#"{"total":2,"in_progress":"yes"}"#,
    ] {
        let d = parse_demote_status_file(body).expect("total present admits the document");
        assert_eq!(d.in_progress, None, "body {body}");
        assert_ne!(d.in_progress, Some(false), "body {body}");
    }
}

#[test]
fn parse_demote_status_file_never_publishes_a_measured_refutation_from_a_corrupt_body() {
    assert!(parse_demote_status_file("not json").is_none());
    // A body with a measured in_progress but no total is refused as a
    // document; the consumer then publishes None, never Some(false).
    assert!(parse_demote_status_file(r#"{"in_progress":true}"#).is_none());
}

#[test]
fn demote_snapshot_default_never_publishes_a_measured_refutation() {
    let d = DemoteSnapshot::default();
    assert_eq!(d.in_progress, None);
    assert_ne!(d.in_progress, Some(false));
}

#[test]
fn status_warns_on_ghost_with_mock_swapon() {
    clear_test_seams();
    push_sh("swapon", Ok("NAME TYPE SIZE USED PRIO"));
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/nbd0\\040(deleted) partition 1 2 100\n",
    ));
    let r = status(false);
    clear_test_seams();
    assert!(r.is_ok());
}

#[test]
fn chrono_and_default_daemon_and_mem() {
    assert!(!chrono_like_now().is_empty());
    let d = default_daemon();
    assert!(d.contains("ramsharedd") || d.ends_with("ramsharedd"));
    set_test_mem(Some(12345));
    assert_eq!(mem_available_bytes(), 12345);
    set_test_mem(None);
    assert!(mem_available_bytes() > 0);
}

#[test]
fn lower_tier_free_bytes_counts_only_durable_absorber_capacity() {
    clear_test_seams();
    // 8 GiB free on a disk sink: the full capacity is available to DEMOTE.
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/sdc partition 8388608 0 -2\n",
    ));
    assert_eq!(
        lower_tier_free_bytes().expect("strict injected swaps"),
        8388608 * 1024
    );
    // zram is RAM-backed and is not a durable A1 sink: it contributes 0.
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/zram0 partition 1024 0 200\n",
    ));
    assert_eq!(lower_tier_free_bytes().expect("strict injected swaps"), 0);
    // A present-but-full sink reports 0 free, which is the A1 capacity case.
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/sdc partition 4194304 4194304 -2\n",
    ));
    assert_eq!(lower_tier_free_bytes().expect("strict injected swaps"), 0);
    // Two sinks share the evacuation: free capacity is summed.
    set_test_swaps(Some(
        "Filename Type Size Used Priority\n\
             /dev/sdc partition 4194304 3145728 -2\n\
             /dev/sdd partition 4194304 4194304 -3\n\
             /dev/nbd0 partition 2097152 0 100\n",
    ));
    assert_eq!(
        lower_tier_free_bytes().expect("strict injected swaps"),
        (4194304 - 3145728) * 1024
    );
    clear_test_seams();
}

#[test]
fn allowlist_ublkb_and_ramshared_name() {
    assert!(is_allowlisted_managed_path("/dev/ublkb0"));
    assert!(is_allowlisted_managed_path("/dev/mapper/ramshared0"));
    assert!(!is_allowlisted_managed_path("/swapfile"));
}

#[test]
fn setup_zram_zero_skips() {
    let z = cascade_io::setup_zram(0, 200).unwrap();
    assert!(z.is_empty());
}

#[test]
fn daemon_kill_allowed_active_nbd() {
    let live = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/nbd0 partition 1 0 100\n",
    );
    assert!(!daemon_kill_allowed(&live));
    assert!(active_vram_block_swap(&live));
}

#[test]
fn pressure_refusals_and_snapshot_helpers_report_each_structural_fault() {
    struct SeamGuard;
    impl Drop for SeamGuard {
        fn drop(&mut self) {
            clear_test_seams();
        }
    }
    let _guard = SeamGuard;

    assert_eq!(snapshot::device_basename("/dev/zram0"), "zram0");
    assert_eq!(snapshot::device_basename("nbd0"), "nbd0");
    assert_eq!(snapshot::device_basename(""), "");

    assert!(snapshot::pressure_refusals(&ready_snapshot(), PressureGate::Admission).is_empty());

    let mut ghost_snap = ready_snapshot();
    ghost_snap.ghost = true;
    assert!(!snapshot::pressure_refusals(&ghost_snap, PressureGate::Admission).is_empty());

    let mut no_zram = ready_snapshot();
    no_zram.zram.present = false;
    assert!(!snapshot::pressure_refusals(&no_zram, PressureGate::Admission).is_empty());

    let mut no_vram = ready_snapshot();
    no_vram.vram.present = false;
    assert!(!snapshot::pressure_refusals(&no_vram, PressureGate::Admission).is_empty());

    let mut no_disk = ready_snapshot();
    no_disk.disk.present = false;
    assert!(!snapshot::pressure_refusals(&no_disk, PressureGate::Admission).is_empty());

    let mut dead_daemon = ready_snapshot();
    dead_daemon.daemon_alive = false;
    assert!(!snapshot::pressure_refusals(&dead_daemon, PressureGate::Admission).is_empty());

    let mut bad_origin = ready_snapshot();
    bad_origin.origin_state = OriginState::Degraded;
    assert!(!snapshot::pressure_refusals(&bad_origin, PressureGate::Admission).is_empty());

    set_test_swaps(Some("Filename Type Size Used Priority\n"));
    assert!(stress_readiness().is_err());
    assert!(stress_continuation().is_err());
}
