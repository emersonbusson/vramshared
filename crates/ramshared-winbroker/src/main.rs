// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winbroker/src/main.rs
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Service { config: Option<PathBuf> },
    Console { config: PathBuf },
}

fn is_absolute_config(path: &Path) -> bool {
    if path.is_absolute() {
        return true;
    }
    let value = path.to_string_lossy();
    let bytes = value.as_bytes();
    value.starts_with(r"\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

fn parse_cli<I, S>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    match args.as_slice() {
        [] => Ok(Command::Service { config: None }),
        [flag, config] if flag == "--config" => {
            let path = PathBuf::from(config);
            if !is_absolute_config(&path) {
                return Err("--config must be absolute".into());
            }
            Ok(Command::Service { config: Some(path) })
        }
        [command, flag, config] if command == "console" && flag == "--config" => {
            let path = PathBuf::from(config);
            if !is_absolute_config(&path) {
                return Err("--config must be absolute".into());
            }
            Ok(Command::Console { config: path })
        }
        _ => Err("usage: ramshared-winbroker [console --config <absolute>]".into()),
    }
}

fn run<I, S>(args: I) -> Result<(), (i32, String)>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    match parse_cli(args) {
        Ok(Command::Service { config }) => {
            #[cfg(windows)]
            {
                let Some(config) = config else {
                    return Err((2, "SCM entry requires --config <absolute>".into()));
                };
                ramshared_winbroker::service::set_service_config(config)
                    .and_then(|()| {
                        ramshared_winbroker::service::dispatch().map_err(|e| e.to_string())
                    })
                    .map_err(|error| (2, format!("SCM dispatch failed: {error}")))
            }
            #[cfg(not(windows))]
            {
                let _ = config;
                Err((2, "RamSharedBroker SCM entry requires Windows".into()))
            }
        }
        Ok(Command::Console { config }) => {
            let bytes = std::fs::read(&config)
                .map_err(|error| (2, format!("config read failed: {error}")))?;
            let config = ramshared_winbroker::BrokerConfigV1::from_toml(&bytes)
                .map_err(|error| (2, format!("config invalid: {error}")))?;
            #[cfg(windows)]
            {
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                ramshared_winbroker::service::run_console(config, stop)
                    .map_err(|error| (3, format!("console failed: {error}")))
            }
            #[cfg(not(windows))]
            {
                let _ = config;
                Err((2, "RamSharedBroker console entry requires Windows".into()))
            }
        }
        Err(error) => Err((2, error)),
    }
}

fn main() {
    if let Err((code, error)) = run(std::env::args().skip(1)) {
        eprintln!("{error}");
        std::process::exit(code);
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, parse_cli, run};
    use std::path::{Path, PathBuf};

    #[test]
    fn cli_rejects_relative_config() {
        assert!(parse_cli(["console", "--config", "broker.toml"]).is_err());
        assert!(parse_cli(["--config", "broker.toml"]).is_err());
    }

    #[test]
    fn cli_has_no_tcp_listen_option() {
        assert!(parse_cli(["console", "--listen", "127.0.0.1:7700"]).is_err());
    }

    #[test]
    fn cli_has_no_install_mutation() {
        assert!(parse_cli(["install", "--config", r"C:\broker.toml"]).is_err());
    }

    #[test]
    fn cli_accepts_service_and_console_with_absolute_config_paths() {
        assert_eq!(
            parse_cli::<[&str; 0], &str>([]),
            Ok(Command::Service { config: None })
        );
        assert_eq!(
            parse_cli(["--config", r"C:\ProgramData\RamShared\broker.toml"]),
            Ok(Command::Service {
                config: Some(PathBuf::from(r"C:\ProgramData\RamShared\broker.toml")),
            })
        );
        assert_eq!(
            parse_cli([
                "console",
                "--config",
                r"\\localhost\pipe\ramshared\broker.toml"
            ]),
            Ok(Command::Console {
                config: PathBuf::from(r"\\localhost\pipe\ramshared\broker.toml"),
            })
        );
        assert!(super::is_absolute_config(Path::new(
            "/etc/ramshared/broker.toml"
        )));
        assert!(!super::is_absolute_config(Path::new(
            "relative/broker.toml"
        )));
    }

    #[test]
    fn run_reports_exit_code_2_for_invalid_args_missing_or_bad_config_and_non_windows() {
        let missing = run(["--unknown"]);
        assert_eq!(missing.as_ref().map_err(|(c, _)| *c), Err(2));

        let service = run::<[&str; 0], &str>([]);
        assert_eq!(service.as_ref().map_err(|(c, _)| *c), Err(2));

        let missing_file = run(["console", "--config", "/nonexistent/ramshared-broker.toml"]);
        assert_eq!(missing_file.as_ref().map_err(|(c, _)| *c), Err(2));

        let dir =
            std::env::temp_dir().join(format!("ramshared-winbroker-main-{}", std::process::id()));
        assert!(std::fs::create_dir_all(&dir).is_ok());

        let bad_toml = dir.join("bad.toml");
        assert!(std::fs::write(&bad_toml, "not valid toml ===\n").is_ok());
        let bad_res = run(["console", "--config", bad_toml.to_string_lossy().as_ref()]);
        assert_eq!(bad_res.as_ref().map_err(|(c, _)| *c), Err(2));

        let valid_toml = dir.join("valid.toml");
        assert!(std::fs::write(&valid_toml, include_bytes!("../broker.example.toml")).is_ok());
        #[cfg(not(windows))]
        {
            let valid_res = run(["console", "--config", valid_toml.to_string_lossy().as_ref()]);
            assert_eq!(valid_res.as_ref().map_err(|(c, _)| *c), Err(2));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
