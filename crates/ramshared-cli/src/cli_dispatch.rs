// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cli_dispatch.rs
//! CLI command dispatch and concrete system action wiring.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus};

use crate::command::{CliCommand, SwapRebalanceMode, parse_cli_command};
use crate::monitor::MonitorOptions;
use crate::readiness::{
    Decision, print_recommendations, print_text_report, recommendations_for, render_doctor_json,
    render_json, run_check,
};
use crate::resource_config::{self, ConfigMode};
use crate::{
    cascade, diagnose, installed_image, monitor, release_coherence, runtime, service, setup,
    stress, supervisor, usage, workload,
};

pub(crate) trait CliActionRunner {
    fn check(&mut self, json: bool, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
    fn doctor(&mut self, json: bool, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
    fn up(&mut self, args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
    fn release_sync(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
    fn migrate_legacy_cascade(
        &mut self,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn ensure_origin(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
    fn ensure_guardian(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
    fn boot(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
    fn down(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
    fn down_recover_released_peer(
        &mut self,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn down_plan(&mut self, json: bool, stdout: &mut dyn Write, stderr: &mut dyn Write)
    -> ExitCode;
    fn swap_rebalance(
        &mut self,
        mode: &SwapRebalanceMode,
        target: &str,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn status(&mut self, json: bool, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
    fn config(
        &mut self,
        mode: ConfigMode,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn monitor(
        &mut self,
        options: &MonitorOptions,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn diagnose(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn setup(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn service(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn install(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn package(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn stress(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn run_workload(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn session(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn supervise(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode;
    fn recover(&mut self, resume: bool, stdout: &mut dyn Write, stderr: &mut dyn Write)
    -> ExitCode;
    fn recover_nbd_peer(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode;
}

pub(crate) struct SystemCliActions;

impl CliActionRunner for SystemCliActions {
    fn check(&mut self, json: bool, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        let report = run_check();
        let output = if json {
            writeln!(stdout, "{}", render_json(&report))
        } else {
            print_text_report(&report, stdout)
        };
        finish_report(output, report.decision(), "check report", stderr)
    }

    fn doctor(&mut self, json: bool, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        let report = run_check();
        let recommendations = recommendations_for(&report);
        let output = if json {
            writeln!(stdout, "{}", render_doctor_json(&report, &recommendations))
        } else {
            print_text_report(&report, stdout)
                .and_then(|()| print_recommendations(&recommendations, stdout))
        };
        finish_report(output, report.decision(), "doctor report", stderr)
    }

    fn up(&mut self, args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        finish_up_with(
            args,
            should_auto_wrap_systemd_scope(
                &|k| std::env::var(k),
                Path::new("/run/systemd/system").exists(),
            ),
            |wrap_args, err| dispatch_systemd_scope(wrap_args, err),
            cascade::up_with_args,
            release_coherence::sync_selected_release,
            stdout,
            stderr,
        )
    }

    fn release_sync(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        finish_release_sync(release_coherence::sync_selected_release(), stdout, stderr)
    }

    fn migrate_legacy_cascade(
        &mut self,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        to_exit(cascade::migrate_legacy_cascade(), stderr)
    }

    fn ensure_origin(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        announce_prepared(
            cascade::ensure_sealed_origin_attached(),
            "ensure-origin",
            "sealed origin device is attached",
            stdout,
            stderr,
        )
    }

    fn ensure_guardian(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        announce_prepared(
            cascade::ensure_sealed_guardian_running(),
            "ensure-guardian",
            "sealed guardian producer is running",
            stdout,
            stderr,
        )
    }

    fn boot(&mut self, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        finish_boot_result(&cascade::boot::boot(), stdout, stderr)
    }

    fn down(&mut self, _stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        to_exit(cascade::down(), stderr)
    }

    fn down_recover_released_peer(
        &mut self,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        to_exit(cascade::down_recover_released_peer(), stderr)
    }

    fn down_plan(
        &mut self,
        json: bool,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        to_exit(cascade::down_plan(json), stderr)
    }

    fn swap_rebalance(
        &mut self,
        mode: &SwapRebalanceMode,
        target: &str,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        finish_swap_rebalance_with(
            mode,
            target,
            cascade::swap_rebalance_plan,
            cascade::swap_rebalance_apply,
            stderr,
        )
    }

    fn status(&mut self, json: bool, _stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        to_exit(cascade::status(json), stderr)
    }

    fn config(
        &mut self,
        mode: ConfigMode,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        resource_config::run(mode, stdout, stderr)
    }

    fn monitor(
        &mut self,
        options: &MonitorOptions,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        to_exit(monitor::run(options), stderr)
    }

    fn diagnose(
        &mut self,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        match diagnose::run(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                let _ = writeln!(stderr, "{e}");
                ExitCode::from(e.exit_code())
            }
        }
    }

    fn setup(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        // New surface: every refusal is a PRD-mapped customer exit, not an
        // errno. `setup::run` owns the strict parse and the observation wiring.
        ExitCode::from(setup::run(args, stdout, stderr))
    }

    fn service(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        // New surface: every refusal is a PRD-mapped customer exit, not an
        // errno. `service::run` owns the strict parse, boot-surface custody and
        // the enablement effect wiring.
        ExitCode::from(service::run(args, stdout, stderr))
    }

    fn install(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        ExitCode::from(crate::customer::install_transaction::run(
            args, stdout, stderr,
        ))
    }

    fn package(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        ExitCode::from(crate::customer::package_hook::run(args, stdout, stderr))
    }

    fn stress(
        &mut self,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        match stress::parse_stress_args(args) {
            Ok(opts) => to_exit(stress::run(&opts), stderr),
            Err(error) => parse_refusal_exit(error, stderr),
        }
    }

    fn run_workload(
        &mut self,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        to_exit(workload::run(args), stderr)
    }

    fn session(
        &mut self,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        to_exit(workload::session(args), stderr)
    }

    fn supervise(
        &mut self,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        to_exit(supervisor::run(args), stderr)
    }

    fn recover(
        &mut self,
        resume: bool,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> ExitCode {
        finish_recover_with(
            resume,
            workload::recover_resume,
            workload::recover_status,
            stdout,
            stderr,
        )
    }

    fn recover_nbd_peer(&mut self, _stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
        finish_recover_nbd_peer_with(
            should_auto_wrap_systemd_scope(
                &|k| std::env::var(k),
                Path::new("/run/systemd/system").exists(),
            ),
            |err| dispatch_systemd_scope_for(&["recover", "--nbd-peer"], &[], err),
            cascade::recover_nbd_peer,
            stderr,
        )
    }
}

pub(crate) fn run_from_args<R: CliActionRunner>(
    args: &[String],
    actions: &mut R,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    match parse_cli_command(args) {
        Ok(CliCommand::Version) => {
            let _ = writeln!(
                stdout,
                "{}\n(Author: Emerson Busson - https://www.linkedin.com/in/emersonbusson)",
                monitor::version_status_lines()
            );
            ExitCode::SUCCESS
        }
        Ok(CliCommand::BuildInfo) => {
            let _ = writeln!(stdout, "{}", installed_image::build_info_lines());
            ExitCode::SUCCESS
        }
        Ok(CliCommand::Run { args }) => actions.run_workload(&args, stdout, stderr),
        Ok(CliCommand::Session { args }) => actions.session(&args, stdout, stderr),
        Ok(CliCommand::Supervise { args }) => actions.supervise(&args, stdout, stderr),
        Ok(CliCommand::Recover { resume }) => actions.recover(resume, stdout, stderr),
        Ok(CliCommand::RecoverNbdPeer) => actions.recover_nbd_peer(stdout, stderr),
        Ok(CliCommand::Check { json }) => actions.check(json, stdout, stderr),
        Ok(CliCommand::Doctor { json }) => actions.doctor(json, stdout, stderr),
        Ok(CliCommand::Up { args }) => actions.up(&args, stdout, stderr),
        Ok(CliCommand::MigrateLegacyCascade) => actions.migrate_legacy_cascade(stdout, stderr),
        Ok(CliCommand::EnsureOrigin) => actions.ensure_origin(stdout, stderr),
        Ok(CliCommand::EnsureGuardian) => actions.ensure_guardian(stdout, stderr),
        Ok(CliCommand::Boot) => actions.boot(stdout, stderr),
        Ok(CliCommand::Down) => actions.down(stdout, stderr),
        Ok(CliCommand::DownRecoverReleasedPeer) => {
            actions.down_recover_released_peer(stdout, stderr)
        }
        Ok(CliCommand::DownPlan { json }) => actions.down_plan(json, stdout, stderr),
        Ok(CliCommand::SwapRebalance { mode, target }) => {
            actions.swap_rebalance(&mode, &target, stdout, stderr)
        }
        Ok(CliCommand::ReleaseSync) => actions.release_sync(stdout, stderr),
        Ok(CliCommand::Status { json }) => actions.status(json, stdout, stderr),
        Ok(CliCommand::Config { mode }) => actions.config(mode, stdout, stderr),
        Ok(CliCommand::Monitor { options }) => actions.monitor(&options, stdout, stderr),
        Ok(CliCommand::Diagnose { args }) => actions.diagnose(&args, stdout, stderr),
        Ok(CliCommand::Setup { args }) => actions.setup(&args, stdout, stderr),
        Ok(CliCommand::Service { args }) => actions.service(&args, stdout, stderr),
        Ok(CliCommand::Install { args }) => actions.install(&args, stdout, stderr),
        Ok(CliCommand::Package { args }) => actions.package(&args, stdout, stderr),
        Ok(CliCommand::Stress { args }) => actions.stress(&args, stdout, stderr),
        Ok(CliCommand::Help) => {
            print_usage(stderr);
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            print_usage(stderr);
            ExitCode::from(2)
        }
    }
}

pub(crate) fn finish_recover_nbd_peer_with<E: fmt::Display>(
    should_wrap: bool,
    wrap_fn: impl FnOnce(&mut dyn Write) -> ExitCode,
    recover_fn: impl FnOnce() -> Result<(), E>,
    stderr: &mut dyn Write,
) -> ExitCode {
    if should_wrap {
        return wrap_fn(stderr);
    }
    to_exit(recover_fn(), stderr)
}

pub(crate) fn finish_up_with<E: fmt::Display>(
    args: &[String],
    should_wrap: bool,
    wrap_fn: impl FnOnce(&[String], &mut dyn Write) -> ExitCode,
    up_fn: impl FnOnce(&[String]) -> Result<(), E>,
    sync_fn: impl FnOnce() -> Option<release_coherence::JointMoveReport>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    if let Some(json) = runtime::lifecycle_plan::plan_json_request(args) {
        return to_exit(runtime::lifecycle_plan::render(json, stdout), stderr);
    }
    if should_wrap {
        return wrap_fn(args, stderr);
    }
    let result = up_fn(args);
    // RF-3 / DT-3: a successful `up` must leave the product processes on
    // the release it just activated. The move is planned from one
    // observation and only restarts units that were already active; a
    // unit that is down is never started here. The selector move itself is
    // atomic across units, so a joint-move failure is reported, not
    // escalated into an `up` failure — the cascade is already correct.
    if result.is_ok()
        && let Some(report) = sync_fn()
    {
        write_joint_move_report(&report, stdout);
    }
    to_exit(result, stderr)
}

pub(crate) fn finish_release_sync(
    report: Option<release_coherence::JointMoveReport>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    match report {
        Some(report) => release_sync_after_move(&report, stdout, stderr),
        None => release_sync_without_selector(stdout),
    }
}

pub(crate) fn finish_boot_result(
    result: &Result<cascade::BootConfig, cascade::BootError>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    // NFR-6: the refusal reason is visible on the invoking surface
    // (journal via `StandardOutput=journal`) and stable enough for
    // `status` to surface later.
    let _ = writeln!(stdout, "{}", boot_announcement(result));
    to_exit(result.as_ref().map(|_| ()), stderr)
}

pub(crate) fn finish_swap_rebalance_with<E: fmt::Display>(
    mode: &SwapRebalanceMode,
    target: &str,
    plan_fn: impl FnOnce(&str, bool) -> Result<(), E>,
    apply_fn: impl FnOnce(&str) -> Result<(), E>,
    stderr: &mut dyn Write,
) -> ExitCode {
    let result = match mode {
        SwapRebalanceMode::Plan { json } => plan_fn(target, *json),
        SwapRebalanceMode::Apply => apply_fn(target),
    };
    to_exit(result, stderr)
}

pub(crate) fn finish_recover_with(
    resume: bool,
    resume_fn: impl FnOnce() -> Result<(), String>,
    status_fn: impl FnOnce() -> Result<String, String>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    if resume {
        return to_exit(resume_fn(), stderr);
    }
    match status_fn() {
        Ok(status) => recover_status_exit(status, stdout, stderr),
        Err(error) => to_exit::<String>(Err(error), stderr),
    }
}

pub(crate) fn to_exit<E: fmt::Display>(r: Result<(), E>, stderr: &mut dyn Write) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(stderr, "{e}");
            ExitCode::from(1)
        }
    }
}

/// Map a readiness decision onto a process exit code. Pure.
///
/// This is the whole of "a blocked report is a non-zero exit"; keeping it here
/// means `check` and `doctor` cannot drift apart on it.
pub(crate) fn decision_exit(decision: Decision) -> ExitCode {
    match decision {
        Decision::Ready => ExitCode::SUCCESS,
        Decision::Blocked => ExitCode::from(1),
    }
}

/// Finish a report: a failed write is a failure, otherwise the readiness
/// decision is. The write is the only effect; the mapping is pure.
pub(crate) fn finish_report<E: fmt::Display>(
    output: Result<(), E>,
    decision: Decision,
    what: &str,
    stderr: &mut dyn Write,
) -> ExitCode {
    if let Err(error) = output {
        let _ = writeln!(stderr, "failed to write {what}: {error}");
        return ExitCode::from(1);
    }
    decision_exit(decision)
}

/// Announce a prepared-state effect: the success line on stdout, or the error
/// on stderr. Collapses the `to_exit` plus ad-hoc success `writeln!` pair the
/// ensure-* verbs all repeated.
pub(crate) fn announce_prepared<E: fmt::Display>(
    result: Result<(), E>,
    tag: &str,
    success: &str,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    if result.is_ok() {
        let _ = writeln!(stdout, "[{tag}] {success}");
        return ExitCode::SUCCESS;
    }
    to_exit(result, stderr)
}

/// The line `boot` prints, whichever way the cascade answered. Pure.
pub(crate) fn boot_announcement(
    result: &Result<cascade::BootConfig, cascade::BootError>,
) -> String {
    match result {
        Ok(config) => format!(
            "[boot] cascade active (vram={}MiB zram={}MiB headroom={}MiB)",
            config.vram_mib, config.zram_mib, config.min_vram_headroom_mib
        ),
        Err(error) => format!("[boot] REFUSED: {error}"),
    }
}

/// Joint-move already happened: report every unit, exit on any residual.
pub(crate) fn release_sync_after_move(
    report: &release_coherence::JointMoveReport,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let failed = write_joint_move_report(report, stdout);
    if failed == 0 {
        return ExitCode::SUCCESS;
    }
    let _ = writeln!(
        stderr,
        "[release-sync] {failed} unit(s) did not reach the selected release"
    );
    ExitCode::from(1)
}

/// No release selector means there is nothing to move onto. That is not a
/// partial fleet; it is an empty one.
pub(crate) fn release_sync_without_selector(stdout: &mut dyn Write) -> ExitCode {
    let _ = writeln!(
        stdout,
        "[release-sync] no selected release; nothing to move"
    );
    ExitCode::SUCCESS
}

/// Print a recovered status record, mapping a failed write to an exit code.
pub(crate) fn recover_status_exit(
    status: String,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    match writeln!(stdout, "{status}") {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(stderr, "failed to write recovery status: {error}");
            ExitCode::from(1)
        }
    }
}

/// A refused argument shape is an operator mistake, not a runtime failure:
/// exit 2 distinguishes it from an effect that returned an error.
pub(crate) fn parse_refusal_exit(error: impl fmt::Display, stderr: &mut dyn Write) -> ExitCode {
    let _ = writeln!(stderr, "error: {error}");
    ExitCode::from(2)
}

/// Render the joint-move report on the invoking surface and return how many
/// units the move could not put on the selected release. DT-7 vocabulary is
/// fixed: `restarted | already_current | left_inactive | unobserved | failed`.
pub(crate) fn write_joint_move_report(
    report: &release_coherence::JointMoveReport,
    stdout: &mut dyn Write,
) -> usize {
    for line in report.format_lines() {
        let _ = writeln!(stdout, "[release-sync] {line}");
    }
    report.failed_units().len()
}

pub(crate) fn should_auto_wrap_systemd_scope<F>(env_lookup: &F, systemd_running: bool) -> bool
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    if !systemd_running {
        return false;
    }
    if env_lookup("RAMSHARED_NO_AUTO_SCOPE").is_ok() {
        return false;
    }
    if env_lookup("_RAMSHARED_SCOPED").is_ok() {
        return false;
    }
    env_lookup("INVOCATION_ID").is_err()
}

pub(crate) fn systemd_scope_command_for(
    current_exe: &Path,
    subcommand: &[&str],
    args: &[String],
) -> Command {
    let mut cmd = Command::new("systemd-run");
    cmd.arg("--scope").arg("-q").arg("--").arg(current_exe);
    for arg in subcommand {
        cmd.arg(arg);
    }
    for arg in args {
        cmd.arg(arg);
    }
    cmd.env("_RAMSHARED_SCOPED", "1");
    cmd
}

#[cfg(test)]
pub(crate) fn systemd_scope_command(current_exe: &Path, args: &[String]) -> Command {
    systemd_scope_command_for(current_exe, &["up"], args)
}

pub(crate) fn systemd_scope_exit(
    status: Result<ExitStatus, std::io::Error>,
    stderr: &mut dyn Write,
) -> ExitCode {
    match status {
        Ok(status) => {
            if let Some(code) = status.code() {
                ExitCode::from(code as u8)
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            let _ = writeln!(stderr, "failed to spawn systemd-run --scope: {error}");
            ExitCode::from(1)
        }
    }
}

pub(crate) fn dispatch_systemd_scope_for(
    subcommand: &[&str],
    args: &[String],
    stderr: &mut dyn Write,
) -> ExitCode {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            let _ = writeln!(
                stderr,
                "failed to resolve current binary path for systemd scope: {error}"
            );
            return ExitCode::from(1);
        }
    };
    let mut cmd = systemd_scope_command_for(&current_exe, subcommand, args);
    systemd_scope_exit(cmd.status(), stderr)
}

pub(crate) fn dispatch_systemd_scope(args: &[String], stderr: &mut dyn Write) -> ExitCode {
    dispatch_systemd_scope_for(&["up"], args, stderr)
}

pub(crate) fn print_usage(stderr: &mut dyn Write) {
    // The advertised surface lives in `usage` so the help text and the
    // parsers cannot drift apart. See `usage::SURFACES` and its contract tests.
    usage::write_usage(stderr);
}
