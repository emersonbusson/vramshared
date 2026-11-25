// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/tests/workload_extra.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn systemd_scope_parsing_runner_validation_and_query_error_paths() {
    assert_eq!(
        parse_meminfo("SwapTotal: 100 kB\n").unwrap_err(),
        "MemTotal missing from /proc/meminfo"
    );
    assert_eq!(
        scope_status_query_failure(Some(1), b"  Unit missing \n"),
        "exact scope status query failed: 1: Unit missing"
    );
    assert_eq!(
        scope_status_query_failure(Some(2), b"   "),
        "exact scope status query failed: 2"
    );
    assert_eq!(
        scope_status_query_failure(None, b""),
        "exact scope status query failed: signal"
    );
    assert_eq!(
        parse_scope_status("a.scope", "bad-line").unwrap_err(),
        "malformed exact scope status response"
    );
    assert_eq!(
        parse_scope_status("a.scope", "Bogus=1").unwrap_err(),
        "unexpected exact scope status field Bogus"
    );
    assert!(parse_scope_status("a.scope", "Id=b.scope\nInvocationID=x\nLoadState=loaded\nActiveState=active\nSubState=running\nResult=success").is_err());
    assert!(parse_scope_status("a.scope", "Id=a.scope").is_err());

    let runner = SystemScopeRunner;
    let err = runner
        .launch("a.scope", &["--scope".into()], &["true".into()])
        .err()
        .expect("missing `--` terminator must fail");
    assert!(err.contains("omit the command terminator"));
}

#[test]
fn ledger_lock_path_validation_refuses_empty_and_parent_traversal() {
    assert_eq!(
        open_directory_path(Path::new(""), false).unwrap_err(),
        "ledger authority path is empty"
    );
    assert_eq!(
        open_directory_path(Path::new("../escape"), false).unwrap_err(),
        "ledger authority path contains an unsafe component"
    );
}

#[test]
fn recover_resume_at_refuses_wrong_boot_missing_health_and_corrupt_guest_gate() {
    let dir = fixture();
    let boot_id_path = dir.join("boot_id");
    fs::write(&boot_id_path, "00000000-0000-0000-0000-000000000000\n").unwrap();
    let paths = RecoveryPaths {
        supervisor_state: dir.join("supervisor.json"),
        boot_id: boot_id_path.clone(),
        guest_gate: dir.join("safe-mode.json"),
        host_gate_dir: dir.join("host"),
        pending: dir.join("pending.json"),
        resume_lease: dir.join("lease.json"),
        hardware_health: dir.join("health.json"),
    };
    assert_eq!(
        recover_resume_at(&paths).unwrap_err(),
        "recovery boot identity is not current"
    );

    let boot_id = current_boot_id().unwrap();
    fs::write(&boot_id_path, format!("{boot_id}\n")).unwrap();
    assert_eq!(
        recover_resume_at(&paths).unwrap_err(),
        "hardware health proof is missing"
    );

    let status = recover_status_at(&paths.guest_gate, &paths.resume_lease).unwrap();
    assert!(status.contains(r#""safe_mode":false"#));
    fs::remove_dir_all(dir).ok();
}
