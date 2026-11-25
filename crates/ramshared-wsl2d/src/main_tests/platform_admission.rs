// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/platform_admission.rs
use super::*;

fn ublk_args(force: bool, backend: BackendKind) -> AppArgs {
    let mut args = daemon_argv(&["ramsharedd", "--transport", "ublk", "--backend", "ram"]);
    if force {
        args.push("--force".into());
    }
    args.push("--accept-wsl2-nbd".into());
    let mut args = AppArgs::parse_from(&args).unwrap();
    args.backend = backend;
    args
}

#[test]
fn standalone_ublk_refuses_wsl_or_unknown_before_pid_custody() {
    struct Runner {
        calls: usize,
    }
    impl DaemonActionRunner for Runner {
        fn execute(&mut self, _: DaemonAction) -> Result<(), Box<dyn std::error::Error>> {
            self.calls += 1;
            Ok(())
        }
    }
    for measured in [Some(true), None] {
        for force in [false, true] {
            for backend in [BackendKind::Ram, BackendKind::Vram, BackendKind::Auto] {
                let pid = Cell::new(0);
                let watchdog = Cell::new(0);
                let mut runner = Runner { calls: 0 };
                let result = run_with(
                    ublk_args(force, backend),
                    &mut runner,
                    |args| select_daemon_action_for(args, measured),
                    || {
                        pid.set(pid.get() + 1);
                        Ok(())
                    },
                    |_| watchdog.set(watchdog.get() + 1),
                );
                assert!(
                    result.is_err(),
                    "standalone ublk admitted WSL/unknown: {measured:?}, force={force}"
                );
                assert_eq!((pid.get(), watchdog.get(), runner.calls), (0, 0, 0));
            }
        }
    }
    let pid = Cell::new(0);
    let watchdog = Cell::new(0);
    let mut runner = Runner { calls: 0 };
    let result = run_with(
        ublk_args(false, BackendKind::Ram),
        &mut runner,
        |args| select_daemon_action_for(args, Some(false)),
        || {
            pid.set(pid.get() + 1);
            Ok(())
        },
        |_| watchdog.set(watchdog.get() + 1),
    );
    assert!(result.is_ok());
    assert_eq!((pid.get(), watchdog.get(), runner.calls), (1, 0, 1));
    assert!(matches!(
        select_daemon_action_for(ublk_args(false, BackendKind::Ram), Some(false)),
        Ok(DaemonAction::Ublk(_))
    ));
}
