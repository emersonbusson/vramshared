// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/authority_runtime.rs
//! Portable authority lifecycle tests and supervision.
use crate::runtime::InstallMode;
use crate::vsock_lease::{ControlPlaneTask, StopState, VsockConfig};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// CLI argument passed by SCM when launching `RamSharedWinSvc` in authority-only mode.
pub const AUTHORITY_SERVICE_ARG: &str = "authority-service";

/// Return the launch arguments for an SCM service registered in `mode`.
pub fn authority_launch_arguments() -> &'static [&'static str] {
    &[AUTHORITY_SERVICE_ARG]
}

/// Detect the configured `RamSharedWinSvc` launch mode from its SCM command line.
///
/// Only an exact whitespace-delimited `authority-service` token outside a file
/// path selects `InstallMode::AuthorityOnly`; everything else falls back to
/// `InstallMode::Storage`.
pub fn detect_installed_consumer_mode(command_line: &str) -> InstallMode {
    let mut rest = command_line.trim();
    if rest.is_empty() {
        return InstallMode::Storage;
    }
    // Skip argv[0] (either quoted `"C:\...\ramshared-winsvc.exe"` or unquoted token).
    if let Some(after_open) = rest.strip_prefix('"') {
        if let Some((_, after_close)) = after_open.split_once('"') {
            rest = after_close;
        } else {
            return InstallMode::Storage;
        }
    } else if let Some((_, after_first)) = rest.split_once(char::is_whitespace) {
        rest = after_first;
    } else {
        return InstallMode::Storage;
    }
    for token in rest.split_whitespace() {
        let cleaned = token.trim_matches('"');
        if cleaned == AUTHORITY_SERVICE_ARG {
            return InstallMode::AuthorityOnly;
        }
    }
    InstallMode::Storage
}

/// Fail-closed pre-mutation check for `InstallMode::AuthorityOnly`: the
/// `[vsock]` table in `config_path` must exist and its configured secret key must
/// be readable before touching SCM.
pub fn validate_authority_control_config(config_path: &Path) -> Result<VsockConfig, String> {
    let config = VsockConfig::load(config_path)
        .map_err(|error| format!("authority config invalid: {error}"))?
        .ok_or_else(|| {
            format!(
                "authority config missing [vsock] table: {}",
                config_path.display()
            )
        })?;
    let _key = config
        .read_key()
        .map_err(|error| format!("authority key unreadable: {error}"))?;
    Ok(config)
}

/// Observe the owned worker without stopping it, then join it after a stop request.
pub fn supervise<R, W>(
    task: &mut ControlPlaneTask,
    stop: &AtomicBool,
    ready: R,
    mut wait: W,
) -> io::Result<()>
where
    R: FnOnce() -> io::Result<()>,
    W: FnMut(Duration),
{
    if !stop.load(Ordering::Acquire) {
        ready()?;
    }
    while !stop.load(Ordering::Acquire) {
        if task.poll()? == StopState::Stopped {
            return Err(io::Error::other("control-plane task exited unexpectedly"));
        }
        wait(Duration::from_millis(250));
    }
    while task.wait_for_stop(Duration::from_secs(1))? == StopState::Pending {}
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        AUTHORITY_SERVICE_ARG, authority_launch_arguments, detect_installed_consumer_mode,
        supervise, validate_authority_control_config,
    };
    use crate::InstallMode;
    use crate::vsock_lease::ControlPlaneTask;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn stop_before_start_never_publishes_running_and_joins_worker() {
        let stopped = AtomicBool::new(true);
        let marks = AtomicUsize::new(0);
        let mut task = ControlPlaneTask::spawn("authority-prestop", |signal| {
            while !signal.is_requested() {
                signal.wait(Duration::from_millis(5));
            }
            Ok(())
        })
        .unwrap();
        supervise(
            &mut task,
            &stopped,
            || {
                marks.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(marks.load(Ordering::SeqCst), 0);
        assert_eq!(task.poll().unwrap(), crate::vsock_lease::StopState::Stopped);
    }

    #[test]
    fn running_authority_stops_and_joins_after_stop_request() {
        let stopped = AtomicBool::new(false);
        let marks = AtomicUsize::new(0);
        let mut task = ControlPlaneTask::spawn("authority-normal", |signal| {
            while !signal.is_requested() {
                signal.wait(Duration::from_millis(5));
            }
            Ok(())
        })
        .unwrap();
        supervise(
            &mut task,
            &stopped,
            || {
                marks.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_| {
                stopped.store(true, Ordering::Release);
            },
        )
        .unwrap();
        assert_eq!(marks.load(Ordering::SeqCst), 1);
        assert_eq!(task.poll().unwrap(), crate::vsock_lease::StopState::Stopped);
    }

    #[test]
    fn readiness_failure_retains_worker_until_its_owner_drops() {
        use std::sync::Arc;
        let stopped = AtomicBool::new(false);
        let worker_stopped = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&worker_stopped);
        let mut task = ControlPlaneTask::spawn("authority-ready-failure", move |signal| {
            while !signal.is_requested() {
                signal.wait(Duration::from_millis(5));
            }
            seen.store(true, Ordering::Release);
            Ok(())
        })
        .unwrap();
        let result = supervise(
            &mut task,
            &stopped,
            || Err(std::io::Error::other("ready failed")),
            |_| {},
        );
        assert!(result.unwrap_err().to_string().contains("ready failed"));
        drop(task);
        assert!(worker_stopped.load(Ordering::Acquire));
    }

    #[test]
    fn unexpected_worker_error_preserves_the_reason() {
        let stopped = AtomicBool::new(false);
        let mut task = ControlPlaneTask::spawn("authority-worker-error", |_| {
            Err(std::io::Error::other("worker failed"))
        })
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let result = supervise(
            &mut task,
            &stopped,
            || Ok(()),
            |_| {
                assert!(
                    std::time::Instant::now() < deadline,
                    "worker error not observed"
                );
                std::thread::yield_now();
            },
        );
        assert!(result.unwrap_err().to_string().contains("worker failed"));
    }

    #[test]
    fn unexpected_worker_exit_is_reported_as_failure() {
        let stopped = AtomicBool::new(false);
        let mut task = ControlPlaneTask::spawn("authority-unexpected", |_| Ok(())).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let result = supervise(
            &mut task,
            &stopped,
            || Ok(()),
            |_| {
                assert!(
                    std::time::Instant::now() < deadline,
                    "worker exit not observed"
                );
                std::thread::yield_now();
            },
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exited unexpectedly")
        );
    }

    #[test]
    fn detect_installed_consumer_mode_distinguishes_exact_token() {
        assert_eq!(
            detect_installed_consumer_mode(
                r#""C:\Program Files\RamShared\versions\0.15.0-abc\ramshared-winsvc.exe" authority-service"#
            ),
            InstallMode::AuthorityOnly
        );
        assert_eq!(
            detect_installed_consumer_mode(
                r#"C:\ramshared\bin\ramshared-winsvc.exe authority-service"#
            ),
            InstallMode::AuthorityOnly
        );
        assert_eq!(
            detect_installed_consumer_mode(
                r#""C:\Program Files\RamShared\versions\0.15.0-abc\ramshared-winsvc.exe" --config "C:\Program Files\RamShared\versions\0.15.0-abc\winsvc.toml""#
            ),
            InstallMode::Storage
        );
        assert_eq!(
            detect_installed_consumer_mode(
                r#""C:\authority-service\ramshared-winsvc.exe" --config "C:\winsvc.toml""#
            ),
            InstallMode::Storage
        );
        assert_eq!(detect_installed_consumer_mode(""), InstallMode::Storage);
        assert_eq!(authority_launch_arguments(), &[AUTHORITY_SERVICE_ARG]);
    }

    #[test]
    fn validate_authority_control_config_fails_closed_when_absent_or_unkeyed() {
        let dir =
            std::env::temp_dir().join(format!("ramshared-authority-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("missing.toml");
        assert!(
            validate_authority_control_config(&missing)
                .unwrap_err()
                .contains("authority config invalid")
        );

        let no_table = dir.join("no-table.toml");
        std::fs::write(&no_table, "[win_drive]\n").unwrap();
        assert!(
            validate_authority_control_config(&no_table)
                .unwrap_err()
                .contains("missing [vsock]")
        );

        let unkeyed = dir.join("unkeyed.toml");
        let missing_key = dir.join("missing.key");
        std::fs::write(
            &unkeyed,
            format!(
                "[vsock]\nport = 2761\nkey_file = \"{}\"\n",
                missing_key.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        assert!(
            validate_authority_control_config(&unkeyed)
                .unwrap_err()
                .contains("authority key unreadable")
        );

        let key_path = dir.join("vsock.key");
        std::fs::write(&key_path, [0x42u8; 32]).unwrap();
        let valid = dir.join("valid.toml");
        std::fs::write(
            &valid,
            format!(
                "[vsock]\nport = 2761\nkey_file = \"{}\"\n",
                key_path.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let loaded = validate_authority_control_config(&valid).unwrap();
        assert_eq!(loaded.port, 2761);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
