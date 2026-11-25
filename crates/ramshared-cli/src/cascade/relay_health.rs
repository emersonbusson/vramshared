// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/relay_health.rs
// Native WSL relay health observation (SPEC zero-script-native-runtime).
// The sealed `wsl-relay-health.sh --check` probe is replaced by the same
// read-only process classification in Rust: no shell, no `bash`, and no
// environment overrides that a production host could inject.
#![allow(unsafe_code)]

use std::fs;
use std::io;
use std::path::Path;

use ramshared_tier::nbd_readiness::Gate;

const MIN_AGE_SECONDS: u64 = 600;
const MAX_CANDIDATES: usize = 128;
const ESRCH: i32 = 3;

/// Read a `/proc/<pid>/<file>` text entry. If the read fails because the
/// process exited mid-scan (`ENOENT`/`ESRCH`) AND `/proc/<pid>` itself is
/// freshly proven absent (`NotFound`), returns `Ok(None)` so a short-lived
/// unrelated process never causes a false `RELAY_GATE_UNKNOWN`. Any error
/// while `/proc/<pid>` still exists remains fail-closed (`Err(())`).
fn read_proc_string(base: &Path, rel: &Path) -> Result<Option<String>, ()> {
    match fs::read_to_string(base.join(rel)) {
        Ok(text) => Ok(Some(text)),
        Err(err) if is_verified_vanished(base, &err) => Ok(None),
        Err(_) => Err(()),
    }
}

fn read_proc_bytes(base: &Path, rel: &Path) -> Result<Option<Vec<u8>>, ()> {
    match fs::read(base.join(rel)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if is_verified_vanished(base, &err) => Ok(None),
        Err(_) => Err(()),
    }
}

fn is_verified_vanished(base: &Path, err: &io::Error) -> bool {
    let vanished_err = err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(ESRCH);
    vanished_err
        && matches!(
            fs::symlink_metadata(base),
            Err(meta_err) if meta_err.kind() == io::ErrorKind::NotFound
        )
}

/// `--check` semantics: a clean host is `Pass`, an orphaned relay candidate is
/// `Fail`, and an unreadable or malformed state is `Unknown`.
pub(crate) fn observe(current: &Path) -> Gate {
    if !current.join("scripts/safety/wsl-relay-health.sh").is_file() {
        return Gate::Unknown;
    }
    let Ok(uptime_seconds) = read_uptime_seconds(Path::new("/proc/uptime")) else {
        return Gate::Unknown;
    };
    match check(
        Path::new("/proc"),
        Path::new("/run/WSL"),
        uptime_seconds,
        clock_ticks_per_second(),
    ) {
        Ok(true) => Gate::Pass,
        Ok(false) => Gate::Fail,
        Err(()) => Gate::Unknown,
    }
}

fn check(
    proc_root: &Path,
    run_root: &Path,
    uptime_seconds: u64,
    clock_ticks: u64,
) -> Result<bool, ()> {
    if clock_ticks == 0 {
        return Err(());
    }
    let entries = fs::read_dir(proc_root).map_err(|_| ())?;
    let mut candidates = 0usize;
    for entry in entries {
        let entry = entry.map_err(|_| ())?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if classify(pid, proc_root, run_root, uptime_seconds, clock_ticks)?.is_some() {
            candidates += 1;
            if candidates > MAX_CANDIDATES {
                return Err(());
            }
        }
    }
    Ok(candidates == 0)
}

/// `Ok(Some(age))` is an orphaned candidate, `Ok(None)` is unrelated, and
/// `Err(())` is a `Relay` process whose state cannot be proven.
fn classify(
    pid: u32,
    proc_root: &Path,
    run_root: &Path,
    uptime_seconds: u64,
    clock_ticks: u64,
) -> Result<Option<u64>, ()> {
    let base = proc_root.join(pid.to_string());
    let Some(comm) = read_proc_string(&base, Path::new("comm"))? else {
        return Ok(None);
    };
    if comm.trim_end() != "Relay" {
        return Ok(None);
    }
    let Some(cmdline) = read_proc_bytes(&base, Path::new("cmdline"))? else {
        return Ok(None);
    };
    if cmdline != b"/init\0" {
        return Ok(None);
    }
    let Some(status) = read_proc_string(&base, Path::new("status"))? else {
        return Ok(None);
    };
    let parent = parent_pid(&status)?;
    if parent != 1 && parent != 2 {
        return Ok(None);
    }
    let children_rel = Path::new("task").join(pid.to_string()).join("children");
    let Some(children) = read_proc_string(&base, &children_rel)? else {
        return Ok(None);
    };
    if !children.trim().is_empty() {
        return Ok(None);
    }
    if run_root.join(format!("{pid}_interop")).exists() {
        return Ok(None);
    }
    let Some(stat) = read_proc_string(&base, Path::new("stat"))? else {
        return Ok(None);
    };
    let start_ticks = stat_start_ticks(&stat)?;
    let started_seconds = start_ticks / clock_ticks;
    if uptime_seconds < started_seconds {
        return Err(());
    }
    let age_seconds = uptime_seconds - started_seconds;
    if age_seconds < MIN_AGE_SECONDS {
        return Ok(None);
    }
    Ok(Some(age_seconds))
}

fn parent_pid(status: &str) -> Result<u32, ()> {
    let mut found = None;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("PPid:") else {
            continue;
        };
        if found.is_some() {
            return Err(());
        }
        found = Some(rest.trim().parse::<u32>().map_err(|_| ())?);
    }
    found.ok_or(())
}

/// `/proc/<pid>/stat` has `pid (comm) state ppid ... starttime`; the comm can
/// contain spaces and parentheses, so split on the final `) ` first.
fn stat_start_ticks(stat: &str) -> Result<u64, ()> {
    let rest = stat.rsplit_once(") ").ok_or(())?.1;
    rest.split_whitespace()
        .nth(19)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(())
}

fn read_uptime_seconds(path: &Path) -> Result<u64, ()> {
    let text = fs::read_to_string(path).map_err(|_| ())?;
    text.split_whitespace()
        .next()
        .and_then(|raw| raw.split('.').next())
        .and_then(|integer| integer.parse::<u64>().ok())
        .ok_or(())
}

fn clock_ticks_per_second() -> u64 {
    // SAFETY: `sysconf` has no pointer arguments and is thread-safe.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 { ticks as u64 } else { 100 }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::path::PathBuf;

    fn root(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ramshared-relay-{}-{id}-{name}",
            std::process::id()
        ))
    }

    fn relay_fixture(proc_root: &Path, pid: u32, start_ticks: u64, cmdline: &[u8]) {
        let base = proc_root.join(pid.to_string());
        fs::create_dir_all(base.join("task").join(pid.to_string())).unwrap();
        fs::write(base.join("comm"), b"Relay\n").unwrap();
        fs::write(base.join("cmdline"), cmdline).unwrap();
        fs::write(base.join("status"), b"PPid:\t1\n").unwrap();
        fs::write(
            base.join("task").join(pid.to_string()).join("children"),
            b"",
        )
        .unwrap();
        let zeros = "0 ".repeat(19);
        fs::write(
            base.join("stat"),
            format!("{pid} (Relay) {zeros}{start_ticks}"),
        )
        .unwrap();
    }

    #[test]
    fn a_clean_proc_root_passes() {
        let proc_root = root("clean");
        fs::create_dir_all(&proc_root).unwrap();
        assert_eq!(
            check(&proc_root, &root("run"), 1000, 100),
            Ok(true),
            "no Relay process means a clean gate"
        );
        let _ = fs::remove_dir_all(&proc_root);
    }

    #[test]
    fn an_old_orphan_fails_and_a_young_one_is_ignored() {
        let proc_root = root("orphan");
        let run_root = root("run-orphan");
        fs::create_dir_all(&run_root).unwrap();
        relay_fixture(&proc_root, 1234, 0, b"/init\0");
        assert_eq!(check(&proc_root, &run_root, 1000, 100), Ok(false));

        fs::write(proc_root.join("1234/stat"), {
            let zeros = "0 ".repeat(19);
            format!("1234 (Relay) {zeros}90000")
        })
        .unwrap();
        assert_eq!(
            check(&proc_root, &run_root, 1000, 100),
            Ok(true),
            "100s is below the 600s candidate age"
        );
        let _ = fs::remove_dir_all(&proc_root);
        let _ = fs::remove_dir_all(&run_root);
    }

    #[test]
    fn a_malformed_relay_state_is_unknown() {
        let proc_root = root("malformed");
        fs::create_dir_all(proc_root.join("77/task/77")).unwrap();
        fs::write(proc_root.join("77/comm"), b"Relay\n").unwrap();
        fs::write(proc_root.join("77/cmdline"), b"/init\0").unwrap();
        assert_eq!(check(&proc_root, &root("run-bad"), 1000, 100), Err(()));
        let _ = fs::remove_dir_all(&proc_root);
    }

    #[test]
    fn observe_is_unknown_without_the_sealed_script() {
        let dir = root("observe-missing");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(observe(&dir), Gate::Unknown);
        // A present script does not guarantee readable /proc: short-lived or
        // inaccessible processes legitimately yield Unknown. Positive and
        // malformed observations are exercised by the hermetic check fixtures.
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classification_branches_ignore_non_candidates() {
        let proc_root = root("branches");
        let run_root = root("run-branches");
        fs::create_dir_all(&run_root).unwrap();

        // Not Relay: ignored.
        fs::create_dir_all(proc_root.join("10")).unwrap();
        fs::write(proc_root.join("10/comm"), b"bash\n").unwrap();
        // Relay with a foreign cmdline: ignored.
        relay_fixture(&proc_root, 11, 0, b"/init --flag\0");
        // Relay with a live child: ignored.
        relay_fixture(&proc_root, 12, 0, b"/init\0");
        fs::write(proc_root.join("12/task/12/children"), b"99\n").unwrap();
        // Relay with the interop marker: ignored.
        relay_fixture(&proc_root, 13, 0, b"/init\0");
        fs::write(run_root.join("13_interop"), b"").unwrap();
        // Relay parented by init but young: ignored.
        relay_fixture(&proc_root, 14, 90_000, b"/init\0");

        assert_eq!(check(&proc_root, &run_root, 1000, 100), Ok(true));
        let _ = fs::remove_dir_all(&proc_root);
        let _ = fs::remove_dir_all(&run_root);
    }

    #[test]
    fn start_ticks_and_uptime_parsers_are_total() {
        let zeros = "0 ".repeat(19);
        assert_eq!(
            stat_start_ticks(&format!("9 (Relay) {zeros}4321")),
            Ok(4321)
        );
        assert!(stat_start_ticks("9 (Relay)").is_err());
        assert!(stat_start_ticks("9 (Relay) 0 1 2").is_err());
        assert_eq!(parent_pid("PPid:\t2\n").unwrap(), 2);
        assert!(parent_pid("PPid:\t2\nPPid:\t3\n").is_err());
        assert!(parent_pid("Name:\tRelay\n").is_err());

        let uptime = root("uptime");
        fs::write(&uptime, b"1234.56 99.0\n").unwrap();
        assert_eq!(read_uptime_seconds(&uptime), Ok(1234));
        assert!(read_uptime_seconds(&root("missing-uptime")).is_err());
        let _ = fs::remove_file(&uptime);
    }

    #[test]
    fn vanished_proc_entry_is_ignored_while_live_missing_file_stays_unknown() {
        let proc_root = root("vanished");
        let run_root = root("run-vanished");
        fs::create_dir_all(&proc_root).unwrap();
        fs::create_dir_all(&run_root).unwrap();

        // Vanished PID: `/proc/999` directory itself does not exist -> Ok(None).
        assert_eq!(classify(999, &proc_root, &run_root, 1000, 100), Ok(None));
        let absent = proc_root.join("999");
        assert_eq!(read_proc_string(&absent, Path::new("comm")), Ok(None));
        assert_eq!(read_proc_bytes(&absent, Path::new("cmdline")), Ok(None));

        // Live PID directory without `comm` -> fail-closed Err(()).
        fs::create_dir_all(proc_root.join("888")).unwrap();
        assert_eq!(classify(888, &proc_root, &run_root, 1000, 100), Err(()));
        // Live PID with `Relay` comm but missing `cmdline` -> fail-closed Err(()).
        fs::write(proc_root.join("888/comm"), b"Relay\n").unwrap();
        assert_eq!(classify(888, &proc_root, &run_root, 1000, 100), Err(()));

        let _ = fs::remove_dir_all(&proc_root);
        let _ = fs::remove_dir_all(&run_root);
    }
}
