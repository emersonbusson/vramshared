// SPDX-License-Identifier: GPL-2.0
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-hostlaunch/src/argv.rs
//
// Copyright (c) 2026 RamShared contributors.
//
// Pure Windows command-line composition for the windowless host launcher.
//
// SPEC DT-11: the launcher owns the console decision, so it also owns the one
// encoding of the child command line. Nothing here touches a process, a device,
// or the filesystem — each function maps owned strings to owned strings and
// refuses the shapes Windows cannot represent.
//
// The quoting rules are the documented `CommandLineToArgvW` rules, not a
// "wrap in quotes" heuristic: backslashes are literal unless they precede a
// double quote, where they must be doubled, and the run of backslashes at the
// end of an argument must be doubled before the closing quote.

use crate::LaunchError;

/// `CreateProcessW` accepts at most 32,767 UTF-16 code units in a command line,
/// including the terminating NUL.
pub(crate) const MAX_COMMAND_LINE_UNITS: usize = 32_767;

/// True when `path` is a drive-absolute Windows path (`C:\...` or `C:/...`).
pub(crate) fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// True when `value` names a path rather than a bare program name. A bare name
/// is resolved by the Windows loader through `PATH`; anything with a separator
/// or a drive prefix is a path whose base directory the launcher cannot trust.
pub(crate) fn looks_like_path(value: &str) -> bool {
    value.contains('/') || value.contains('\\') || value.contains(':')
}

/// Quote one argument according to the `CommandLineToArgvW` rules.
///
/// An argument with no space, tab, or quote is passed through unchanged, which
/// keeps Windows paths (`C:\safe\origin.vhdx`) and bare program names readable
/// in the task XML and in the process command line.
pub(crate) fn quote_argument(argument: &str) -> Result<String, LaunchError> {
    if argument.contains('\0') {
        return Err(LaunchError::NulInArgument);
    }
    if !argument.is_empty()
        && !argument
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\t' | b'"'))
    {
        return Ok(argument.to_string());
    }

    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                // Double the pending backslashes and escape the quote itself.
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(other);
            }
        }
    }
    // A backslash run immediately before the closing quote is read as escaping
    // that quote, so it must be doubled.
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    Ok(quoted)
}

/// Compose the single command line `CreateProcessW` receives.
///
/// The program is the first token and is quoted by the same rules. The UTF-16
/// bound is checked on the composed line, because that is the unit the Windows
/// API measures.
pub(crate) fn build_command_line(program: &str, args: &[String]) -> Result<String, LaunchError> {
    let mut line = quote_argument(program)?;
    for argument in args {
        line.push(' ');
        line.push_str(&quote_argument(argument)?);
    }
    let units = line.encode_utf16().count();
    if units > MAX_COMMAND_LINE_UNITS {
        return Err(LaunchError::CommandLineTooLong(units));
    }
    Ok(line)
}

/// Translate a guest-visible Windows path into the form `CreateProcessW` takes.
///
/// `/mnt/<drive>/rest` becomes `<DRIVE>:\rest`; an already drive-absolute path
/// is normalized to backslashes. Relative paths, `.`/`..` components, and
/// non-Windows paths are refused: the launcher never resolves a child relative
/// to whatever directory it happened to inherit.
pub(crate) fn to_windows_path(path: &str) -> Result<String, LaunchError> {
    if path.contains('\0') {
        return Err(LaunchError::NulInArgument);
    }
    if path.is_empty() {
        return Err(LaunchError::NotWindowsPath(path.to_string()));
    }

    let (drive, rest) = if let Some(rest) = path.strip_prefix("/mnt/") {
        let bytes = rest.as_bytes();
        if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b'/' {
            return Err(LaunchError::NotWindowsPath(path.to_string()));
        }
        (char::from(bytes[0]), &rest[2..])
    } else if is_windows_absolute(path) {
        (path.as_bytes()[0] as char, &path[2..])
    } else {
        return Err(LaunchError::NotWindowsPath(path.to_string()));
    };

    let mut components = Vec::new();
    for component in rest.split(['\\', '/']) {
        if component.is_empty() {
            continue;
        }
        if component == "." || component == ".." {
            return Err(LaunchError::NotWindowsPath(path.to_string()));
        }
        components.push(component);
    }
    let drive = drive.to_ascii_uppercase();
    if components.is_empty() {
        return Ok(format!("{drive}:\\"));
    }
    Ok(format!("{drive}:\\{}", components.join("\\")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is the documented `CommandLineToArgvW` behaviour, including
    /// the backslash-run cases a naive `format!("\"{}\"")` gets wrong.
    #[test]
    fn windows_argument_quoting_matches_the_ms_rules() {
        const CASES: &[(&str, &str)] = &[
            ("", "\"\""),
            ("plain", "plain"),
            ("C:\\safe\\origin.vhdx", "C:\\safe\\origin.vhdx"),
            ("a b", "\"a b\""),
            ("a\tb", "\"a\tb\""),
            ("a\"b", "\"a\\\"b\""),
            ("\"", "\"\\\"\""),
            ("a\\b", "a\\b"),
            ("a\\ b", "\"a\\ b\""),
            ("C:\\path with space\\", "\"C:\\path with space\\\\\""),
            ("C:\\Program Files\\", "\"C:\\Program Files\\\\\""),
            ("a\\\"b", "\"a\\\\\\\"b\""),
            ("a\\\\\"; b", "\"a\\\\\\\\\\\"; b\""),
        ];
        for (input, expected) in CASES {
            match quote_argument(input) {
                Ok(quoted) => assert_eq!(quoted, *expected, "argument {input:?}"),
                Err(error) => panic!("argument {input:?} was refused: {error}"),
            }
        }
    }

    #[test]
    fn an_argument_with_nul_is_refused() {
        assert_eq!(
            quote_argument("safe\0injected"),
            Err(LaunchError::NulInArgument)
        );
        // The same refusal applies to the composed line, not just one token.
        assert_eq!(
            build_command_line("wsl.exe", &["--mount\0".to_string()]),
            Err(LaunchError::NulInArgument)
        );
    }

    #[test]
    fn a_mnt_drive_path_becomes_a_windows_path() {
        assert_eq!(
            to_windows_path("/mnt/c/ProgramData/RamShared/bin/ramshared-host-launch.exe"),
            Ok("C:\\ProgramData\\RamShared\\bin\\ramshared-host-launch.exe".to_string())
        );
        assert_eq!(to_windows_path("/mnt/d/"), Ok("D:\\".to_string()));
        assert_eq!(
            to_windows_path("c:/Windows/System32/sudo.exe"),
            Ok("C:\\Windows\\System32\\sudo.exe".to_string())
        );
        // Relative and traversal paths have no host meaning here.
        assert_eq!(
            to_windows_path("./wsl.exe"),
            Err(LaunchError::NotWindowsPath("./wsl.exe".to_string()))
        );
        assert_eq!(
            to_windows_path("/mnt/c/../Windows/System32/sudo.exe"),
            Err(LaunchError::NotWindowsPath(
                "/mnt/c/../Windows/System32/sudo.exe".to_string()
            ))
        );
        assert_eq!(
            to_windows_path("/usr/bin/sudo"),
            Err(LaunchError::NotWindowsPath("/usr/bin/sudo".to_string()))
        );
    }

    #[test]
    fn the_command_line_is_bounded_in_utf16_units() {
        let program = "wsl.exe".to_string();
        let fitting = "a".repeat(MAX_COMMAND_LINE_UNITS - program.len() - 1);
        assert!(build_command_line(&program, &[fitting]).is_ok());
        let oversized = "a".repeat(MAX_COMMAND_LINE_UNITS);
        assert_eq!(
            build_command_line(&program, &[oversized]),
            Err(LaunchError::CommandLineTooLong(
                MAX_COMMAND_LINE_UNITS + 1 + program.len()
            ))
        );
    }
}
