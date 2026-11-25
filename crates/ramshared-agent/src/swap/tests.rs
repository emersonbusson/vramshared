// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/swap/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn nbd_args_tcp_has_timeout_no_persist() {
    let ep = NbdEndpoint::Tcp {
        host: "10.0.0.1".into(),
        port: 10809,
    };
    let a = nbd_args(&ep, "s0", "/dev/nbd0");
    assert_eq!(
        a,
        vec![
            "-N",
            "s0",
            "10.0.0.1",
            "10809",
            "/dev/nbd0",
            "-timeout",
            "30"
        ]
    );
    assert!(!a.iter().any(|x| x == "-persist"), "DT-14: never -persist");
}

#[test]
fn nbd_args_unix_uses_unix_flag() {
    let ep = NbdEndpoint::Unix {
        path: "/run/r.sock".into(),
    };
    let a = nbd_args(&ep, "s2", "/dev/nbd2");
    assert_eq!(
        a,
        vec![
            "-N",
            "s2",
            "-unix",
            "/run/r.sock",
            "/dev/nbd2",
            "-timeout",
            "30"
        ]
    );
    assert!(!a.iter().any(|x| x == "-persist"));
}

#[test]
fn swapon_args_emits_prio_only_when_set() {
    assert_eq!(
        swapon_args("/dev/nbd0", Some(-5)),
        vec!["-p", "-5", "/dev/nbd0"]
    );
    assert_eq!(swapon_args("/dev/nbd0", None), vec!["/dev/nbd0"]);
}

#[test]
fn attach_swap_with_nbd_client_fails() {
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let mut rec = Recorder::failing("nbd-client");
    let res = attach_swap_with(
        &ep,
        "export",
        "/dev/nbd0",
        None,
        |c, a, t| rec.run(c, a, t),
        |_| true,
    );
    assert_eq!(
        res,
        Err(SwapFailure {
            error: SwapError::Other("nbd-client: mock error".into()),
            retained: false,
        })
    );
}

/// Records every command and the deadline it was given, so a test can
/// prove which effects ran, in what order, and that each one was bounded
/// (AUD-10: "record acquired resources after every effect"; AUD-11).
struct Recorder {
    log: Vec<String>,
    budgets: Vec<(String, Duration)>,
    fail_on: &'static str,
    failed: bool,
    failure: SwapError,
}

impl Recorder {
    fn failing(fail_on: &'static str) -> Self {
        Self {
            log: Vec::new(),
            budgets: Vec::new(),
            fail_on,
            failed: false,
            failure: SwapError::Other("mock error".into()),
        }
    }

    fn timing_out(fail_on: &'static str) -> Self {
        Self {
            failure: SwapError::Timeout("timed out after 1 ms; process group contained".into()),
            ..Self::failing(fail_on)
        }
    }

    fn run(
        &mut self,
        cmd: &str,
        args: &[String],
        timeout: Duration,
    ) -> std::result::Result<(), SwapError> {
        self.log.push(format!("{cmd} {}", args.join(" ")));
        self.budgets.push((cmd.to_string(), timeout));
        if cmd == self.fail_on && !self.failed {
            self.failed = true;
            return Err(self.failure.clone());
        }
        Ok(())
    }

    /// The release step is `nbd-client -d <dev>`.
    fn was_released(&self, dev: &str) -> bool {
        self.log
            .iter()
            .any(|c| c == &format!("nbd-client -d {dev}"))
    }
}

#[test]
fn attach_swap_with_mkswap_fails_and_releases_the_connection() {
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let mut rec = Recorder::failing("mkswap");
    let res = attach_swap_with(
        &ep,
        "export",
        "/dev/nbd0",
        None,
        |c, a, t| rec.run(c, a, t),
        |_| true,
    );
    let err = res.expect_err("mkswap failure must fail the attach");
    assert_eq!(
        err,
        SwapFailure {
            error: SwapError::Other("mkswap: mock error; released the nbd connection".into()),
            retained: false,
        }
    );
    assert!(
        rec.was_released("/dev/nbd0"),
        "a failed mkswap must disconnect the device just acquired; log={:?}",
        rec.log
    );
}

#[test]
fn attach_swap_with_swapon_fails_and_releases_the_connection() {
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let mut rec = Recorder::failing("swapon");
    let res = attach_swap_with(
        &ep,
        "export",
        "/dev/nbd0",
        None,
        |c, a, t| rec.run(c, a, t),
        |_| true,
    );
    let err = res.expect_err("swapon failure must fail the attach");
    assert_eq!(
        err,
        SwapFailure {
            error: SwapError::Other("swapon: mock error; released the nbd connection".into()),
            retained: false,
        }
    );
    assert!(
        rec.was_released("/dev/nbd0"),
        "a failed swapon must disconnect the device just acquired; log={:?}",
        rec.log
    );
}

#[test]
fn attach_swap_with_release_failure_reports_a_held_connection() {
    // A failed mkswap whose rollback also fails must name both things:
    // the original cause and that the connection is still held (AUD-10).
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let mut cmds: Vec<String> = Vec::new();
    let res = attach_swap_with(
        &ep,
        "export",
        "/dev/nbd0",
        None,
        |cmd, args, _timeout| {
            cmds.push(format!("{cmd} {}", args.join(" ")));
            match cmd {
                "mkswap" => Err(SwapError::Other("mock error".into())),
                "nbd-client" if args.first().map(String::as_str) == Some("-d") => {
                    Err(SwapError::Other("disconnect refused".into()))
                }
                _ => Ok(()),
            }
        },
        |_| false,
    );
    let err = res.expect_err("must fail");
    assert_eq!(
        err,
        SwapFailure {
            error: SwapError::Other(
                "mkswap: mock error; nbd connection NOT released: disconnect refused".into()
            ),
            retained: true,
        }
    );
    assert_eq!(
        cmds,
        vec![
            "nbd-client -N export -unix /sock /dev/nbd0 -timeout 30".to_string(),
            "mkswap /dev/nbd0".to_string(),
            "nbd-client -d /dev/nbd0".to_string(),
        ],
        "exactly one acquired resource, released once, in order"
    );
}

#[test]
fn attach_swap_with_nbd_client_fails_changes_nothing_else() {
    // Refusal without mutation: if the connect never happened there is
    // nothing to release, and mkswap/swapon must not run (AUD-10).
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let mut rec = Recorder::failing("nbd-client");
    let res = attach_swap_with(
        &ep,
        "export",
        "/dev/nbd0",
        None,
        |c, a, t| rec.run(c, a, t),
        |_| true,
    );
    assert_eq!(
        res,
        Err(SwapFailure {
            error: SwapError::Other("nbd-client: mock error".into()),
            retained: false,
        })
    );
    assert_eq!(
        rec.log.as_slice(),
        &["nbd-client -N export -unix /sock /dev/nbd0 -timeout 30".to_string()],
        "no later step and no release may run"
    );
}

#[test]
fn attach_swap_with_success() {
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let res = attach_swap_with(&ep, "export", "/dev/nbd0", None, |_, _, _| Ok(()), |_| true);
    assert_eq!(res, Ok(()));
}

#[test]
fn detach_swap_error_path() {
    let res = detach_swap("/dev/invalid_device_for_test");
    assert!(res.is_err());
    let err = res.unwrap_err();
    assert!(err.to_string().starts_with("swapoff: "));
    assert!(err.retained, "a failed swapoff always retains the device");
}

#[test]
fn detach_swap_keeps_the_device_when_disconnect_fails_and_the_connection_is_held() {
    // Ambiguity preserves the resource: a failed `nbd-client -d` with the
    // kernel still reporting a holder is not a finished detach (AUD-10).
    let mut rec = Recorder::failing("nbd-client");
    let res = detach_swap_with("/dev/nbd0", |c, a, t| rec.run(c, a, t), |_| false);
    let err = res.expect_err("a held connection must not be reported as detached");
    assert_eq!(
        err,
        SwapFailure {
            error: SwapError::Other(
                "nbd-client -d: mock error; connection still held (/dev/nbd0)".into()
            ),
            retained: true,
        }
    );
    assert_eq!(
        rec.log.as_slice(),
        &[
            "swapoff /dev/nbd0".to_string(),
            "nbd-client -d /dev/nbd0".to_string(),
        ]
    );
}

#[test]
fn detach_swap_treats_a_fallen_connection_as_absent() {
    // A disconnect that fails while the kernel already reports no holder
    // is the device having already fallen — that is success, not residue.
    let mut rec = Recorder::failing("nbd-client");
    let res = detach_swap_with("/dev/nbd0", |c, a, t| rec.run(c, a, t), |_| true);
    assert_eq!(res, Ok(()));
    assert_eq!(
        rec.log.as_slice(),
        &[
            "swapoff /dev/nbd0".to_string(),
            "nbd-client -d /dev/nbd0".to_string(),
        ]
    );
}

#[test]
fn detach_swap_requires_the_swap_absence_claim_first() {
    // `swapoff` stays mandatory and first: its failure never reaches the
    // disconnect, so a device that still holds swap is never silently
    // dropped (AUD-10).
    let mut rec = Recorder::failing("swapoff");
    let res = detach_swap_with("/dev/nbd0", |c, a, t| rec.run(c, a, t), |_| true);
    let err = res.expect_err("a failed swapoff must fail the detach");
    assert!(err.to_string().starts_with("swapoff: "), "got {err}");
    assert!(err.retained);
    assert_eq!(
        rec.log.as_slice(),
        &["swapoff /dev/nbd0".to_string()],
        "no disconnect may run while swap is still on"
    );
}

#[test]
fn detach_swapoff_timeout_retains_the_device() {
    // A contained `swapoff` is the ambiguous case: swap may still be on.
    // Reporting success would authorize a later disconnect of live swap.
    let mut rec = Recorder::timing_out("swapoff");
    let res = detach_swap_with("/dev/nbd0", |c, a, t| rec.run(c, a, t), |_| true);
    let err = res.expect_err("a timed-out swapoff must not claim absence");
    assert!(err.retained, "swap state is unknown: {err}");
    assert!(err.to_string().starts_with("swapoff: "), "got {err}");
    assert_eq!(
        rec.log.as_slice(),
        &["swapoff /dev/nbd0".to_string()],
        "no disconnect may run while swap state is unknown"
    );
}

#[test]
fn attach_timed_out_swapon_never_releases_a_possibly_live_swap() {
    // `swapon` can complete before the helper is contained. Rolling the
    // connection back there would tear down live swap (AUD-11).
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let mut rec = Recorder::timing_out("swapon");
    let res = attach_swap_with(
        &ep,
        "export",
        "/dev/nbd0",
        None,
        |c, a, t| rec.run(c, a, t),
        |_| true,
    );
    let err = res.expect_err("a timed-out swapon must fail the attach");
    assert!(err.retained, "the device must stay owned: {err}");
    assert!(
        !rec.was_released("/dev/nbd0"),
        "nothing may be disconnected under unknown swap state; log={:?}",
        rec.log
    );
    assert!(
        err.to_string().contains("unknown swap state"),
        "the operator must be told why nothing was rolled back: {err}"
    );
}

#[test]
fn attach_timed_out_mkswap_still_releases_the_connection() {
    // `mkswap` only writes a header: it cannot have enabled swap, so a
    // timeout there is safe to roll back.
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let mut rec = Recorder::timing_out("mkswap");
    let res = attach_swap_with(
        &ep,
        "export",
        "/dev/nbd0",
        None,
        |c, a, t| rec.run(c, a, t),
        |_| true,
    );
    let err = res.expect_err("a timed-out mkswap must fail the attach");
    assert!(!err.retained, "the rollback released the connection: {err}");
    assert!(rec.was_released("/dev/nbd0"), "log={:?}", rec.log);
}

#[test]
fn every_effect_carries_a_deadline_and_the_tearown_owns_the_long_one() {
    // The bound is part of the effect contract (AUD-11): no step may be
    // dispatched unbounded, and `swapoff` — the one effect that legitimately
    // runs long — keeps the generous teardown budget.
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let mut attach = Recorder::failing("nothing");
    attach_swap_with(
        &ep,
        "export",
        "/dev/nbd7",
        Some(-2),
        |c, a, t| attach.run(c, a, t),
        |_| true,
    )
    .expect("attach");
    let mut detach = Recorder::failing("nothing");
    detach_swap_with("/dev/nbd7", |c, a, t| detach.run(c, a, t), |_| true).expect("detach");

    let budgets = [attach.budgets.clone(), detach.budgets.clone()].concat();
    assert!(
        budgets.iter().all(|(_, t)| !t.is_zero()),
        "an effect dispatched with no deadline: {budgets:?}"
    );
    for (cmd, budget) in &budgets {
        let expected = if cmd == "swapoff" {
            SWAPOFF_TIMEOUT
        } else {
            EFFECT_TIMEOUT
        };
        assert_eq!(budget, &expected, "{cmd} budget");
    }
}

#[test]
fn every_effect_names_the_device_bound_at_entry() {
    // Device replacement between effects is refused by construction: the
    // device is bound once at entry and every later command names exactly
    // that one. Nothing re-resolves the path mid-sequence (AUD-10).
    let ep = NbdEndpoint::Unix {
        path: "/sock".into(),
    };
    let mut attach = Recorder::failing("nothing");
    attach_swap_with(
        &ep,
        "export",
        "/dev/nbd7",
        Some(-2),
        |c, a, t| attach.run(c, a, t),
        |_| true,
    )
    .expect("attach");
    let mut detach = Recorder::failing("nothing");
    detach_swap_with("/dev/nbd7", |c, a, t| detach.run(c, a, t), |_| true).expect("detach");
    for (label, entries) in [("attach", &attach.log), ("detach", &detach.log)] {
        assert!(
            entries.iter().all(|c| c.contains("/dev/nbd7")),
            "{label} touched another device: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .all(|c| !c.contains("/dev/nbd0") && !c.contains("/dev/nbd1")),
            "{label} rebound the device: {entries:?}"
        );
    }
}

#[test]
fn nbd_connection_gone_never_treats_absence_as_proof() {
    // Only a readable holder pid of 0 is absence. A missing or unreadable
    // sysfs file is an uninspectable device, and ambiguity preserves the
    // resource (AUD-10, kernel device-identity rule).
    assert!(!nbd_connection_gone_impl("/nonexistent/sys/block/nbd0/pid"));
}

#[test]
fn a_never_exiting_helper_is_contained_by_its_deadline() {
    // AUD-11 required test: a child that never returns must not hang the
    // agent. The bound kills its process group and returns.
    let started = std::time::Instant::now();
    let err = run("/bin/sleep", &["3600".into()], Duration::from_millis(200))
        .expect_err("a sleeping child must be contained");
    assert!(err.is_timeout(), "got {err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline did not contain the child: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_stuck_pipeline_holding_stdout_is_contained() {
    // AUD-11 required test: an inherited stdout pipe must not keep the
    // call open. The whole process group is contained, not just the leader.
    let started = std::time::Instant::now();
    let err = run(
        "/bin/sh",
        &["-c".into(), "sleep 3600 | cat".into()],
        Duration::from_millis(200),
    )
    .expect_err("a stuck pipeline must be contained");
    assert!(err.is_timeout(), "got {err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the pipe held the call open: {:?}",
        started.elapsed()
    );
}

#[test]
fn validate_swap_resize_too_small() {
    let err = validate_swap_resize(63 * 1024 * 1024, 1024 * 1024 * 1024)
        .expect_err("should fail below 64 MiB");
    assert_eq!(err, ResizeError::TooSmall(63 * 1024 * 1024));
}

#[test]
fn validate_swap_resize_exceeds_disk() {
    let err = validate_swap_resize(128 * 1024 * 1024, 64 * 1024 * 1024)
        .expect_err("should fail if size exceeds disk");
    assert_eq!(
        err,
        ResizeError::ExceedsDiskSpace {
            size: 128 * 1024 * 1024,
            max: 64 * 1024 * 1024
        }
    );
}

#[test]
fn validate_swap_resize_success() {
    validate_swap_resize(128 * 1024 * 1024, 1024 * 1024 * 1024)
        .expect("should succeed within bounds");
    validate_swap_resize(64 * 1024 * 1024, 64 * 1024 * 1024)
        .expect("should succeed at exact bounds");
}

#[test]
fn resize_error_display_names_every_variant() {
    assert_eq!(
        ResizeError::TooSmall(63 * 1024 * 1024).to_string(),
        "swap size 66060288 bytes is below 64 MiB minimum"
    );
    assert_eq!(
        ResizeError::ExceedsDiskSpace {
            size: 128 * 1024 * 1024,
            max: 64 * 1024 * 1024,
        }
        .to_string(),
        "swap size 134217728 bytes exceeds physical disk space 67108864 bytes"
    );
}

#[test]
fn swap_error_display_names_every_variant() {
    let cases = [
        (SwapError::DiskFull, "disk full (ENOSPC)"),
        (SwapError::PermissionDenied, "permission denied (EACCES)"),
        (SwapError::InvalidSize, "invalid size (ERANGE)"),
        (
            SwapError::Timeout("timed out after 30000 ms".into()),
            "timed out after 30000 ms",
        ),
        (
            SwapError::Other("nbd-client: boom".into()),
            "nbd-client: boom",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn run_maps_special_exit_codes_to_typed_swap_errors() {
    // Codes 28/13/34 are the ENOSPC/EACCES/ERANGE signatures the caller
    // classifies; any other non-zero exit keeps the exit status for the
    // operator and the step wrapper supplies the command name.
    for (code, expected) in [
        (28, SwapError::DiskFull),
        (13, SwapError::PermissionDenied),
        (34, SwapError::InvalidSize),
    ] {
        let err = run(
            "/bin/sh",
            &["-c".into(), format!("exit {code}")],
            EFFECT_TIMEOUT,
        )
        .expect_err("a non-zero exit must fail");
        assert_eq!(err, expected, "exit {code}");
    }
    let err = run("/bin/sh", &["-c".into(), "exit 7".into()], EFFECT_TIMEOUT)
        .expect_err("an unmapped exit must fail");
    assert_eq!(err, SwapError::Other("exit status: 7".into()));
    run("/bin/true", &[], EFFECT_TIMEOUT).expect("a zero exit must succeed");
}
