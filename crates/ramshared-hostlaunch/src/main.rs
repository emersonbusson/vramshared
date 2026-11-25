// SPDX-License-Identifier: GPL-2.0
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-hostlaunch/src/main.rs
//
// Copyright (c) 2026 RamShared contributors.
//
// `ramshared-host-launch` — the one windowless Windows host launcher.
//
// SPEC DT-11/DT-12: every non-interactive WSL-to-Windows product call goes
// through this program. It is a GUI-subsystem executable, so Windows never
// allocates a console for it, and it creates its child with `CREATE_NO_WINDOW`,
// inherits the standard handles, bounds the child, and forwards its exit code.
// `sudo.exe` and `powershell.exe` own their consoles; `-WindowStyle Hidden`
// hides neither. This is the single place that owns the console decision.
//
// The launcher has no product policy: it does not know what a guardian is, it
// never elevates by itself (`--sudo` only names the existing boundary), and it
// writes nothing. A missing launcher is refused by the caller with
// `VISIBLE_ELEVATION_REFUSED`; there is no direct-PowerShell fallback.

#![cfg_attr(windows, windows_subsystem = "windows")]
#![cfg_attr(windows, allow(unsafe_code))]

mod argv;
mod launch;

use std::fmt;
use std::process::ExitCode;

/// Default bound for one host call when the caller does not name one.
///
/// `0` is the explicit "no deadline" value; the scheduled-task action uses it
/// because the guardian producer is long-lived (`PT0S`). Every guest-initiated
/// host call names a real bound.
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling: a host call that outlives this is dead, not slow.
pub(crate) const MAX_TIMEOUT_MS: u64 = 300_000;

/// Conventional shell code for a refused invocation.
pub(crate) const USAGE_EXIT: u8 = 2;
/// The child could not be created.
pub(crate) const SPAWN_EXIT: u8 = 127;
/// The child timed out and was terminated by the launcher.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const TIMEOUT_EXIT: u8 = 124;
/// The wait itself failed after the child was created.
pub(crate) const WAIT_EXIT: u8 = 125;
/// This build has no Windows launch path.
pub(crate) const WINDOWS_ONLY_EXIT: u8 = 126;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LaunchError {
    MissingValue(&'static str),
    UnknownOption(String),
    DuplicateOption(&'static str),
    InvalidTimeout(String),
    EmptyProgram,
    NulInArgument,
    NotWindowsPath(String),
    /// A path-like target without an elevation boundary naming it. A bare
    /// name (`wsl.exe`) is still resolved through `PATH`; a relative path is
    /// not, because its base directory is whatever the launcher inherited.
    RelativeTargetWithoutSudo(String),
    SudoBoundaryNotWindowsPath(String),
    CommandLineTooLong(usize),
    #[cfg_attr(windows, allow(dead_code))]
    WindowsOnly,
    #[cfg_attr(not(windows), allow(dead_code))]
    Spawn(String),
    #[cfg_attr(not(windows), allow(dead_code))]
    Wait(String),
}

impl LaunchError {
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            Self::Spawn(_) => SPAWN_EXIT,
            Self::Wait(_) => WAIT_EXIT,
            Self::WindowsOnly => WINDOWS_ONLY_EXIT,
            Self::CommandLineTooLong(_) => USAGE_EXIT,
            Self::MissingValue(_)
            | Self::UnknownOption(_)
            | Self::DuplicateOption(_)
            | Self::InvalidTimeout(_)
            | Self::EmptyProgram
            | Self::NulInArgument
            | Self::NotWindowsPath(_)
            | Self::RelativeTargetWithoutSudo(_)
            | Self::SudoBoundaryNotWindowsPath(_) => USAGE_EXIT,
        }
    }
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(option) => write!(formatter, "{option} requires a value"),
            Self::UnknownOption(option) => write!(formatter, "unknown option {option}"),
            Self::DuplicateOption(option) => write!(formatter, "{option} was given twice"),
            Self::InvalidTimeout(value) => write!(
                formatter,
                "timeout {value} is not between 0 (no deadline) and {MAX_TIMEOUT_MS} ms"
            ),
            Self::EmptyProgram => write!(formatter, "the target program is empty"),
            Self::NulInArgument => write!(formatter, "an argument contains a NUL"),
            Self::NotWindowsPath(path) => {
                write!(formatter, "{path:?} is not a Windows or /mnt/<drive> path")
            }
            Self::RelativeTargetWithoutSudo(path) => write!(
                formatter,
                "{path:?} is a relative target; name it through --sudo or pass an absolute path"
            ),
            Self::SudoBoundaryNotWindowsPath(path) => write!(
                formatter,
                "--sudo {path:?} is not a Windows or /mnt/<drive> path"
            ),
            Self::CommandLineTooLong(units) => write!(
                formatter,
                "the command line is {units} UTF-16 units; the Windows bound is {}",
                argv::MAX_COMMAND_LINE_UNITS
            ),
            Self::WindowsOnly => {
                write!(formatter, "WINDOWS_ONLY: the host launcher runs on Windows")
            }
            Self::Spawn(reason) => write!(formatter, "CreateProcessW failed: {reason}"),
            Self::Wait(reason) => write!(formatter, "waiting for the child failed: {reason}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// One validated launch request.
///
/// `sudo` is the existing elevation boundary, never an elevation request: the
/// launcher runs that boundary as its child exactly like any other program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaunchRequest {
    pub(crate) sudo: Option<String>,
    pub(crate) timeout_ms: u64,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

impl LaunchRequest {
    /// The image `CreateProcessW` receives and the arguments after it.
    ///
    /// With `--sudo`, the elevation boundary is the image and the requested
    /// program is its first argument, which is the existing boundary's
    /// contract. Without it, the program itself is the image: a bare name is
    /// searched on `PATH`, a Windows or `/mnt/<drive>` path is translated, and
    /// a relative path is refused.
    pub(crate) fn resolve_child(&self) -> Result<(String, Vec<String>), LaunchError> {
        if let Some(boundary) = &self.sudo {
            let program = argv::to_windows_path(boundary)
                .map_err(|_| LaunchError::SudoBoundaryNotWindowsPath(boundary.clone()))?;
            let mut args = Vec::with_capacity(self.args.len() + 1);
            args.push(self.program.clone());
            args.extend(self.args.iter().cloned());
            return Ok((program, args));
        }
        if self.program.is_empty() {
            return Err(LaunchError::EmptyProgram);
        }
        if argv::is_windows_absolute(&self.program) || self.program.starts_with("/mnt/") {
            return Ok((argv::to_windows_path(&self.program)?, self.args.clone()));
        }
        if argv::looks_like_path(&self.program) {
            return Err(LaunchError::RelativeTargetWithoutSudo(self.program.clone()));
        }
        Ok((self.program.clone(), self.args.clone()))
    }

    /// The exact command line, validated for NUL and the UTF-16 bound.
    pub(crate) fn command_line(&self) -> Result<String, LaunchError> {
        let (program, args) = self.resolve_child()?;
        argv::build_command_line(&program, &args)
    }
}

/// Parse the launcher's strict option surface.
///
/// Options stop at the first positional or at `--`. Everything after the
/// program is a child argument verbatim, including strings that start with
/// `-`: the launcher never re-parses a child's command line.
pub(crate) fn parse_launch_args<I, S>(args: I) -> Result<LaunchRequest, LaunchError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut sudo = None;
    let mut timeout_ms = None;
    let mut program = None;
    let mut program_args = Vec::new();
    let mut terminated = false;
    let mut arguments = args.into_iter();

    while let Some(raw) = arguments.next() {
        let argument = raw.as_ref();
        if program.is_some() {
            program_args.push(argument.to_string());
            continue;
        }
        if terminated {
            if argument.is_empty() {
                return Err(LaunchError::EmptyProgram);
            }
            program = Some(argument.to_string());
            continue;
        }
        match argument {
            "--" => terminated = true,
            "--timeout-ms" => {
                if timeout_ms.is_some() {
                    return Err(LaunchError::DuplicateOption("--timeout-ms"));
                }
                let value = arguments
                    .next()
                    .ok_or(LaunchError::MissingValue("--timeout-ms"))?;
                let value = value.as_ref();
                if value.contains('\0') {
                    return Err(LaunchError::NulInArgument);
                }
                let parsed = value
                    .parse::<u64>()
                    .map_err(|_| LaunchError::InvalidTimeout(value.to_string()))?;
                if parsed > MAX_TIMEOUT_MS {
                    return Err(LaunchError::InvalidTimeout(value.to_string()));
                }
                timeout_ms = Some(parsed);
            }
            "--sudo" => {
                if sudo.is_some() {
                    return Err(LaunchError::DuplicateOption("--sudo"));
                }
                let value = arguments
                    .next()
                    .ok_or(LaunchError::MissingValue("--sudo"))?;
                let value = value.as_ref();
                if value.is_empty() {
                    return Err(LaunchError::MissingValue("--sudo"));
                }
                if value.contains('\0') {
                    return Err(LaunchError::NulInArgument);
                }
                sudo = Some(value.to_string());
            }
            other if other.starts_with("--") => {
                return Err(LaunchError::UnknownOption(other.to_string()));
            }
            other => {
                if other.is_empty() {
                    return Err(LaunchError::EmptyProgram);
                }
                program = Some(other.to_string());
            }
        }
    }

    let program = program.ok_or(LaunchError::EmptyProgram)?;
    if program.contains('\0') {
        return Err(LaunchError::NulInArgument);
    }
    if program_args.iter().any(|argument| argument.contains('\0')) {
        return Err(LaunchError::NulInArgument);
    }
    let request = LaunchRequest {
        sudo,
        timeout_ms: timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        program,
        args: program_args,
    };
    // Resolve once here so an invalid target is a usage refusal before any
    // Windows API sees it.
    request.command_line()?;
    Ok(request)
}

fn run_with_os_args<I>(args: I) -> Result<u32, LaunchError>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut collected = Vec::new();
    for raw in args {
        match raw.into_string() {
            Ok(value) => collected.push(value),
            Err(_) => return Err(LaunchError::NulInArgument),
        }
    }
    let request = parse_launch_args(collected)?;
    launch::run(&request)
}

fn run_from_environment() -> Result<u32, LaunchError> {
    run_with_os_args(std::env::args_os().skip(1))
}

/// Map one launch outcome to the process status.
///
/// A Windows child status is 32 bits and PowerShell/`wsl.exe` use the high bits
/// for some failures, so the value is preserved rather than truncated. A
/// refusal maps to its stable launcher exit code.
fn exit_status(result: Result<u32, LaunchError>) -> i32 {
    match result {
        Ok(code) => i32::from_ne_bytes(code.to_ne_bytes()),
        Err(error) => {
            eprintln!("ramshared-host-launch: {error}");
            i32::from(error.exit_code())
        }
    }
}

fn main() -> ExitCode {
    let status = exit_status(run_from_environment());
    // `ExitCode` carries one byte. Values that do not fit (a Windows NTSTATUS,
    // for example) go through `process::exit` so the exact 32 bits survive.
    if let Ok(code) = u8::try_from(status) {
        ExitCode::from(code)
    } else {
        std::process::exit(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_option_surface_is_strict() {
        assert_eq!(
            parse_launch_args(["--unknown", "--", "wsl.exe"]),
            Err(LaunchError::UnknownOption("--unknown".to_string()))
        );
        assert_eq!(
            parse_launch_args(["--sudo"]),
            Err(LaunchError::MissingValue("--sudo"))
        );
        assert_eq!(
            parse_launch_args(["--timeout-ms"]),
            Err(LaunchError::MissingValue("--timeout-ms"))
        );
        // 0 is the explicit unbounded value the long-lived guardian action
        // uses; above the ceiling is still refused.
        let unbounded = parse_launch_args(["--timeout-ms", "0", "--", "wsl.exe"])
            .unwrap_or_else(|error| panic!("an explicit no-deadline request: {error}"));
        assert_eq!(unbounded.timeout_ms, 0);
        assert_eq!(
            parse_launch_args(["--timeout-ms", "300001", "--", "wsl.exe"]),
            Err(LaunchError::InvalidTimeout("300001".to_string()))
        );
        assert_eq!(
            parse_launch_args(["--timeout-ms", "10", "--timeout-ms", "20", "--", "wsl.exe"]),
            Err(LaunchError::DuplicateOption("--timeout-ms"))
        );
        assert_eq!(parse_launch_args(["--"]), Err(LaunchError::EmptyProgram));
        assert_eq!(
            parse_launch_args(Vec::<String>::new()),
            Err(LaunchError::EmptyProgram)
        );
        assert_eq!(
            parse_launch_args(["--sudo", "/mnt/c/Windows/System32/sudo.exe"]),
            Err(LaunchError::EmptyProgram)
        );
        assert_eq!(
            parse_launch_args(["--", "wsl.exe", "-NoProfile"]),
            Ok(LaunchRequest {
                sudo: None,
                timeout_ms: DEFAULT_TIMEOUT_MS,
                program: "wsl.exe".to_string(),
                args: vec!["-NoProfile".to_string()],
            })
        );
        // The first positional ends option parsing: this is a child argument,
        // not an unknown launcher option.
        let request = parse_launch_args(["wsl.exe", "--mount"]).unwrap_or_else(|error| {
            panic!("option-shaped child arguments must be preserved: {error}")
        });
        assert_eq!(request.args, vec!["--mount".to_string()]);
        assert_eq!(request.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn a_relative_target_without_sudo_is_refused() {
        assert_eq!(
            parse_launch_args(["--", "./wsl.exe"]),
            Err(LaunchError::RelativeTargetWithoutSudo(
                "./wsl.exe".to_string()
            ))
        );
        // A bare name is a PATH search, not a relative path.
        assert!(parse_launch_args(["--", "wsl.exe"]).is_ok());
        // Under the existing elevation boundary the target is an argument the
        // boundary resolves; the launcher still never creates it directly.
        let request = parse_launch_args([
            "--sudo",
            "/mnt/c/Windows/System32/sudo.exe",
            "--",
            "./wsl.exe",
        ])
        .unwrap_or_else(|error| panic!("a sudo-bound target is admissible: {error}"));
        let (program, args) = request
            .resolve_child()
            .unwrap_or_else(|error| panic!("resolve: {error}"));
        assert_eq!(program, "C:\\Windows\\System32\\sudo.exe");
        assert_eq!(args, vec!["./wsl.exe".to_string()]);
    }

    #[test]
    fn the_elevation_boundary_must_be_a_windows_path() {
        assert_eq!(
            parse_launch_args(["--sudo", "/usr/bin/sudo", "--", "wsl.exe"]),
            Err(LaunchError::SudoBoundaryNotWindowsPath(
                "/usr/bin/sudo".to_string()
            ))
        );
    }

    #[test]
    fn the_child_command_line_keeps_the_host_path_opaque() {
        let request = parse_launch_args([
            "--timeout-ms",
            "10000",
            "--sudo",
            "/mnt/c/Windows/System32/sudo.exe",
            "--",
            "wsl.exe",
            "--mount",
            "--vhd",
            "C:\\safe\\origin.vhdx",
            "--bare",
        ])
        .unwrap_or_else(|error| panic!("admissible request: {error}"));
        let line = request
            .command_line()
            .unwrap_or_else(|error| panic!("command line: {error}"));
        assert_eq!(
            line,
            "C:\\Windows\\System32\\sudo.exe wsl.exe --mount --vhd C:\\safe\\origin.vhdx --bare"
        );
        assert_eq!(request.timeout_ms, 10_000);
        assert_eq!(line.matches("origin.vhdx").count(), 1);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_non_windows_build_refuses_the_launch() {
        let request = parse_launch_args(["--", "wsl.exe"])
            .unwrap_or_else(|error| panic!("admissible request: {error}"));
        assert_eq!(launch::run(&request), Err(LaunchError::WindowsOnly));
    }

    /// Every refusal has one stable status and one message that names what the
    /// operator must change. Two refusals sharing a message would make the
    /// launcher's stderr a guess.
    #[test]
    fn every_refusal_carries_a_stable_exit_code_and_message() {
        let cases: &[(LaunchError, u8, &str)] = &[
            (
                LaunchError::MissingValue("--sudo"),
                USAGE_EXIT,
                "--sudo requires a value",
            ),
            (
                LaunchError::UnknownOption("--wat".to_string()),
                USAGE_EXIT,
                "unknown option --wat",
            ),
            (
                LaunchError::DuplicateOption("--sudo"),
                USAGE_EXIT,
                "--sudo was given twice",
            ),
            (
                LaunchError::InvalidTimeout("0x10".to_string()),
                USAGE_EXIT,
                "timeout 0x10",
            ),
            (LaunchError::EmptyProgram, USAGE_EXIT, "empty"),
            (LaunchError::NulInArgument, USAGE_EXIT, "NUL"),
            (
                LaunchError::NotWindowsPath("./x".to_string()),
                USAGE_EXIT,
                "not a Windows",
            ),
            (
                LaunchError::RelativeTargetWithoutSudo("./x".to_string()),
                USAGE_EXIT,
                "relative target",
            ),
            (
                LaunchError::SudoBoundaryNotWindowsPath("/usr/bin/sudo".to_string()),
                USAGE_EXIT,
                "--sudo",
            ),
            (
                LaunchError::CommandLineTooLong(40_000),
                USAGE_EXIT,
                "40000 UTF-16 units",
            ),
            (LaunchError::WindowsOnly, WINDOWS_ONLY_EXIT, "WINDOWS_ONLY"),
            (
                LaunchError::Spawn("denied".to_string()),
                SPAWN_EXIT,
                "CreateProcessW failed: denied",
            ),
            (
                LaunchError::Wait("stalled".to_string()),
                WAIT_EXIT,
                "waiting for the child failed: stalled",
            ),
        ];
        for (error, code, needle) in cases {
            assert_eq!(error.exit_code(), *code, "{error}");
            let rendered = error.to_string();
            assert!(rendered.contains(needle), "{rendered:?} lacks {needle:?}");
        }
        // The launcher's errors are ordinary errors; callers may box them.
        let boxed: Box<dyn std::error::Error> = Box::new(LaunchError::WindowsOnly);
        assert!(boxed.to_string().contains("Windows"));
    }

    /// The remaining refusal branches: an empty positional, a NUL in any
    /// position, and the option shapes that cannot be resolved to a child.
    #[test]
    fn the_parser_refuses_the_shapes_windows_cannot_represent() {
        assert_eq!(
            parse_launch_args(["--", ""]),
            Err(LaunchError::EmptyProgram)
        );
        assert_eq!(parse_launch_args([""]), Err(LaunchError::EmptyProgram));
        assert_eq!(
            parse_launch_args(["--timeout-ms", "10\0", "--", "wsl.exe"]),
            Err(LaunchError::NulInArgument)
        );
        assert_eq!(
            parse_launch_args(["--sudo", "a", "--sudo", "b", "--", "wsl.exe"]),
            Err(LaunchError::DuplicateOption("--sudo"))
        );
        assert_eq!(
            parse_launch_args(["--sudo", "", "--", "wsl.exe"]),
            Err(LaunchError::MissingValue("--sudo"))
        );
        assert_eq!(
            parse_launch_args(["--sudo", "a\0", "--", "wsl.exe"]),
            Err(LaunchError::NulInArgument)
        );
        assert_eq!(
            parse_launch_args(["--", "wsl.exe\0"]),
            Err(LaunchError::NulInArgument)
        );
        assert_eq!(
            parse_launch_args(["--", "wsl.exe", "a\0"]),
            Err(LaunchError::NulInArgument)
        );

        // A request built without the parser still refuses an empty image.
        let empty = LaunchRequest {
            sudo: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            program: String::new(),
            args: Vec::new(),
        };
        assert_eq!(empty.command_line(), Err(LaunchError::EmptyProgram));

        // A drive-absolute image is translated, never run as a relative path.
        let absolute = parse_launch_args(["--", "C:\\Windows\\System32\\whoami.exe"])
            .unwrap_or_else(|error| panic!("an absolute image is admissible: {error}"));
        let (program, args) = absolute
            .resolve_child()
            .unwrap_or_else(|error| panic!("resolve: {error}"));
        assert_eq!(program, "C:\\Windows\\System32\\whoami.exe");
        assert!(args.is_empty());
    }

    /// A non-UTF-8 environment argument cannot be composed into a Windows
    /// command line. It is refused, never lossily converted.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_environment_argument_is_refused() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let bad = OsString::from_vec(vec![0xff, 0xfe]);
        assert_eq!(run_with_os_args([bad]), Err(LaunchError::NulInArgument));
    }

    /// The environment entrypoint reaches the parser and the launcher with the
    /// same argv the process received.
    #[cfg(not(windows))]
    #[test]
    fn the_environment_entrypoint_reaches_the_launcher() {
        use std::ffi::OsString;

        let args = [OsString::from("--"), OsString::from("wsl.exe")];
        assert_eq!(run_with_os_args(args), Err(LaunchError::WindowsOnly));
    }

    /// The process entrypoint itself is fail-closed: whatever argv the test
    /// harness owns, a non-Windows build can only refuse, never spawn.
    #[cfg(not(windows))]
    #[test]
    fn the_process_entrypoint_never_spawns_on_a_non_windows_build() {
        assert!(run_from_environment().is_err());
    }

    /// DT-11: the child's 32-bit status is forwarded exactly, and a refusal
    /// maps to its stable launcher code.
    #[test]
    fn the_exit_status_preserves_the_child_code_and_the_refusal_code() {
        assert_eq!(exit_status(Ok(0)), 0);
        assert_eq!(exit_status(Ok(7)), 7);
        assert_eq!(exit_status(Ok(u32::MAX)), -1);
        assert_eq!(
            exit_status(Err(LaunchError::EmptyProgram)),
            i32::from(USAGE_EXIT)
        );
        assert_eq!(
            exit_status(Err(LaunchError::WindowsOnly)),
            i32::from(WINDOWS_ONLY_EXIT)
        );
    }
}
