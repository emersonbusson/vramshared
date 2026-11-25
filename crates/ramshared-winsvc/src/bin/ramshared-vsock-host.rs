// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/bin/ramshared-vsock-host.rs
//! Lab-only console host for the native-vsock lease authority.
//!
//! This is not a product entrypoint and is not included in product bundles. It
//! exists so the live AF_HYPERV handshake/lease/heartbeat path can be
//! qualified without installing the Windows SCM package. It calls the same
//! `spawn_configured_listener` used by the service.

use ramshared_winsvc::vsock_lease::{KeyProtection, VsockConfig};
use std::path::PathBuf;

const DEFAULT_SECONDS: u64 = 900;
const MAX_SECONDS: u64 = 3600;

#[derive(Debug, PartialEq, Eq)]
struct Args {
    config: PathBuf,
    seconds: u64,
    check: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut config: Option<PathBuf> = None;
    let mut seconds = DEFAULT_SECONDS;
    let mut check = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--config requires an absolute path".to_string())?;
                config = Some(PathBuf::from(value));
            }
            "--seconds" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--seconds requires a value".to_string())?;
                seconds = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --seconds value: {value}"))?;
            }
            "--check" => check = true,
            other => return Err(format!("unknown option: {other}")),
        }
        index += 1;
    }
    let config = config.ok_or_else(|| {
        "usage: ramshared-vsock-host --config <absolute-vsock.toml> [--seconds 1..=3600] [--check]"
            .to_string()
    })?;
    if !config.is_absolute() {
        return Err("--config must be an absolute path".into());
    }
    if seconds == 0 || seconds > MAX_SECONDS {
        return Err(format!("--seconds must be 1..={MAX_SECONDS}"));
    }
    Ok(Args {
        config,
        seconds,
        check,
    })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    run_with_args(&args)
}

fn run_with_args(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let parsed =
        parse_args(args).map_err(|message| -> Box<dyn std::error::Error> { message.into() })?;
    let config =
        VsockConfig::load(&parsed.config)?.ok_or_else(|| "vsock table is absent".to_string())?;
    let key = config.read_key()?;
    let protection = match config.key_protection {
        KeyProtection::Plain => "plain",
        KeyProtection::DpapiMachine => "dpapi_machine",
    };
    println!(
        "VSOCK_CHECK=OK port={} protection={} secret_bytes={} key={}",
        config.port,
        protection,
        key.len(),
        config.key_file.display()
    );
    if parsed.check {
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        let _ = (parsed, key);
        Err("the lab AF_HYPERV host requires Windows".into())
    }
    #[cfg(windows)]
    {
        use ramshared_winsvc::vsock_lease::{StopState, spawn_configured_listener};
        use std::time::Duration;
        let product = std::env::current_exe()?.with_file_name("ramshared-winsvc.exe");
        let mut task = spawn_configured_listener(Some(&parsed.config), &product)?
            .ok_or("configured control-plane task is absent")?;
        println!(
            "VSOCK_HOST=RUNNING port={} seconds={} config={}",
            config.port,
            parsed.seconds,
            parsed.config.display()
        );
        std::thread::sleep(Duration::from_secs(parsed.seconds));
        let mut pending_reported = false;
        while task.wait_for_stop(Duration::from_secs(1))? == StopState::Pending {
            if !pending_reported {
                println!("VSOCK_HOST=STOP_PENDING retaining_transport_thread=true");
                pending_reported = true;
            }
        }
        println!("VSOCK_HOST=STOPPED");
        Ok(())
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ramshared-vsock-host: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn check_requires_an_absolute_config_path() {
        assert!(parse_args(&args(&["--check"])).is_err());
        assert!(parse_args(&args(&["--config", "relative.toml"])).is_err());
        let absolute = std::env::temp_dir().join("ramshared-vsock-host-check.toml");
        assert_eq!(
            parse_args(&args(&["--config", absolute.to_str().unwrap(), "--check"])).unwrap(),
            Args {
                config: absolute,
                seconds: DEFAULT_SECONDS,
                check: true,
            }
        );
    }

    #[test]
    fn seconds_are_bounded() {
        let absolute = std::env::temp_dir().join("ramshared-vsock-host-seconds.toml");
        let path = absolute.to_str().unwrap();
        assert!(parse_args(&args(&["--config", path, "--seconds", "0"])).is_err());
        assert!(parse_args(&args(&["--config", path, "--seconds", "3601"])).is_err());
        assert_eq!(
            parse_args(&args(&["--config", path, "--seconds", "600"]))
                .unwrap()
                .seconds,
            600
        );
    }

    #[test]
    fn unknown_options_are_refused() {
        let absolute = std::env::temp_dir().join("ramshared-vsock-host-unknown.toml");
        assert!(parse_args(&args(&["--config", absolute.to_str().unwrap(), "--what"])).is_err());
        assert!(parse_args(&args(&["--config"])).is_err());
        assert!(
            parse_args(&args(&[
                "--config",
                absolute.to_str().unwrap(),
                "--seconds"
            ]))
            .is_err()
        );
        assert!(
            parse_args(&args(&[
                "--config",
                absolute.to_str().unwrap(),
                "--seconds",
                "abc"
            ]))
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_with_args_validates_check_mode_absent_vsock_table_and_windows_host_refusal() {
        use std::os::unix::fs::PermissionsExt;

        struct TempDirGuard(PathBuf);
        impl Drop for TempDirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let dir = TempDirGuard(std::env::temp_dir().join(format!(
            "ramshared-vsock-host-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )));
        std::fs::create_dir_all(&dir.0).unwrap();

        let key_path = dir.0.join("vsock.key");
        std::fs::write(&key_path, vec![0xA5u8; 32]).unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let cfg_path = dir.0.join("vsock.toml");
        std::fs::write(
            &cfg_path,
            format!(
                "[vsock]\nport = 2761\nkey_file = \"{}\"\n",
                key_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(run_with_args(&args(&["--config", cfg_path.to_str().unwrap(), "--check"])).is_ok());

        let err = run_with_args(&args(&["--config", cfg_path.to_str().unwrap()]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires Windows"), "unexpected error: {err}");

        let no_vsock = dir.0.join("no-vsock.toml");
        std::fs::write(&no_vsock, "[win_drive]\nsize_bytes = 1\n").unwrap();
        std::fs::set_permissions(&no_vsock, std::fs::Permissions::from_mode(0o600)).unwrap();
        let missing = run_with_args(&args(&["--config", no_vsock.to_str().unwrap(), "--check"]))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("absent"), "unexpected error: {missing}");
    }
}
