// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/bounded_process/tests/support.rs
#![allow(clippy::expect_used)]
use super::super::*;
use super::*;

pub(super) fn fixture_process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

pub(super) fn wait_for_fixture_process_exit(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    while fixture_process_exists(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    !fixture_process_exists(pid)
}

pub(super) fn kill_exact_fixture_process(pid: u32) {
    let Some(pid) = Pid::from_raw(pid as i32) else {
        return;
    };
    let _ = rustix::process::kill_process(pid, Signal::KILL);
}

/// True when `pid` still holds a capture pipe write end.
///
/// `run_capture_command_with_fatal` wires stdout/stderr as pipes, so a
/// `pipe:[…]` link under `/proc/<pid>/fd/{1,2}` is the observable that the
/// process pins the capture open.
pub(super) fn fixture_pid_holds_capture_pipe(pid: u32) -> bool {
    for fd in [1, 2] {
        let Ok(target) = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")) else {
            continue;
        };
        if target.to_string_lossy().starts_with("pipe:") {
            return true;
        }
    }
    false
}

/// Waits until the leaked-pipe grandchild is an observed holder.
///
/// The capture deadline is established only after `on_spawn` returns, so
/// this wait is a precondition of the timeout, not a race against it. The
/// holder must be alive, must hold a capture pipe, and must sit outside
/// `leader_group` so the timeout's process-group SIGKILL cannot close it.
pub(super) fn wait_for_escaped_pipe_holder(pid_path: &Path, leader_group: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let leader_pgrp = i32::try_from(leader_group).unwrap_or(-1);
    loop {
        let holder = std::fs::read_to_string(pid_path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());
        let ready = holder.is_some_and(|pid| {
            // A real escaped holder has its own non-zero group, distinct
            // from the leader group the timeout will SIGKILL, and still
            // pins at least one capture pipe write end.
            proc_stat_pgrp(pid).is_some_and(|pgrp| pgrp > 0 && pgrp != leader_pgrp)
                && fixture_process_exists(pid)
                && fixture_pid_holds_capture_pipe(pid)
        });
        if ready {
            return;
        }
        if Instant::now() >= deadline {
            if let Some(pid) = holder {
                kill_exact_fixture_process(pid);
            }
            panic!("leaked-pipe fixture never became an escaped capture-pipe holder");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn kill_and_reap_exact_fixture_child(pid: u32) {
    let Some(pid) = Pid::from_raw(pid as i32) else {
        return;
    };
    let _ = kill_process_group(pid, Signal::KILL);
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG) {
            Ok(Some(_)) | Err(Errno::CHILD) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) | Err(_) => break,
        }
    }
}

pub(super) struct NeverReaped {
    pub(super) id: u32,
    pub(super) group_kills: Cell<usize>,
    pub(super) direct_kills: Cell<usize>,
}

pub(super) struct ScriptedTarget {
    pub(super) id: u32,
    pub(super) observations: VecDeque<io::Result<bool>>,
    pub(super) reap_error: Option<io::Error>,
    pub(super) group_errno: Option<i32>,
    pub(super) group_kills: usize,
    pub(super) direct_kills: usize,
}

impl ScriptedTarget {
    pub(super) fn new(observations: Vec<io::Result<bool>>, group_errno: Option<i32>) -> Self {
        Self {
            id: 43,
            observations: observations.into(),
            reap_error: None,
            group_errno,
            group_kills: 0,
            direct_kills: 0,
        }
    }

    pub(super) fn with_reap_error(mut self, error: io::Error) -> Self {
        self.reap_error = Some(error);
        self
    }
}

impl ReapTarget for ScriptedTarget {
    fn id(&self) -> u32 {
        self.id
    }

    fn signal_group_kill(&mut self) -> io::Result<()> {
        self.group_kills += 1;
        match self.group_errno {
            Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            None => Ok(()),
        }
    }

    fn signal_direct_kill(&mut self) -> io::Result<()> {
        self.direct_kills += 1;
        Ok(())
    }

    fn observe_exit(&mut self) -> io::Result<bool> {
        self.observations.pop_front().unwrap_or(Ok(true))
    }

    fn reap_observed(&mut self) -> io::Result<Option<ExitStatus>> {
        match self.reap_error.take() {
            Some(error) => Err(error),
            None => Ok(Some(ExitStatus::from_raw(0))),
        }
    }
}

pub(super) struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("fixture read failure"))
    }
}

pub(super) struct PanickingReader;

impl Read for PanickingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        panic!("fixture capture panic")
    }
}

impl ReapTarget for NeverReaped {
    fn id(&self) -> u32 {
        self.id
    }

    fn signal_group_kill(&mut self) -> io::Result<()> {
        self.group_kills.set(self.group_kills.get() + 1);
        Ok(())
    }

    fn signal_direct_kill(&mut self) -> io::Result<()> {
        self.direct_kills.set(self.direct_kills.get() + 1);
        Ok(())
    }

    fn observe_exit(&mut self) -> io::Result<bool> {
        Ok(false)
    }

    fn reap_observed(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(None)
    }
}

#[derive(Default)]
pub(super) struct RecordingFatal(pub(super) RefCell<Vec<String>>);

impl FatalContainment for RecordingFatal {
    fn contain(&self, detail: &str) {
        self.0.borrow_mut().push(detail.to_string());
    }
}

/// Reader that returns EOF only after `delay`, so a capture worker stays
/// blocked across one or two close-grace windows.
pub(super) struct DelayedEofReader {
    pub(super) delay: Duration,
}

impl Read for DelayedEofReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        std::thread::sleep(self.delay);
        Ok(0)
    }
}

/// A private process group the capture tests may SIGKILL without touching
/// any other session's processes.
pub(super) struct OwnedGroupFixture {
    child: Child,
}

impl OwnedGroupFixture {
    pub(super) fn spawn() -> Self {
        let mut command = Command::new("/bin/sleep");
        command.args(["30"]);
        configure_process_group(&mut command)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command
            .spawn()
            .expect("fixture group leader must start for capture tests");
        Self { child }
    }

    pub(super) fn group_id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for OwnedGroupFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads `pgrp` (field 3 after the comm) from `/proc/<pid>/stat`.
///
/// Parsing the file directly avoids `rustix::process::getpgid`, which
/// `debug_assert!(pgid > 0)` and therefore panics on kernel threads — they
/// report `pgrp = 0`, which is not a signalable process group at all.
pub(super) fn proc_stat_pgrp(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.get(stat.rfind(')')? + 2..)?;
    // after_comm starts at `state ppid pgrp ...`.
    let mut fields = after_comm.split_whitespace();
    fields.next()?; // state
    fields.next()?; // ppid
    fields.next()?.parse::<i32>().ok()
}

/// Returns a live process group this process may not signal, or `None`.
///
/// Candidates must satisfy every safety property at once: owned by another
/// uid, `pgid` a real process group (never 0 — kernel threads — and never
/// `1`, the POSIX broadcast, nor our own group), and a `sig == 0` permission
/// probe must return `EPERM`. Signal 0 delivers nothing, so an `EPERM`
/// answer proves the later `SIGKILL` cannot land on any process in that
/// group.
pub(super) fn foreign_unsignable_process_group() -> Option<Pid> {
    use std::os::unix::fs::MetadataExt;
    let me = rustix::process::geteuid().as_raw();
    let my_group = getpgid(None).ok()?.as_raw_nonzero().get();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(raw) = i32::try_from(pid) else {
            continue;
        };
        if raw == 1 {
            continue;
        }
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if meta.uid() == me {
            continue;
        }
        // pgrp 0 is a kernel thread, not an owned group. Reading it from
        // the stat file keeps that value away from `getpgid`, whose
        // debug assertion would fire on it.
        let Some(group_raw) = proc_stat_pgrp(pid) else {
            continue;
        };
        if group_raw <= 1 || group_raw == my_group {
            continue;
        }
        let Some(group) = Pid::from_raw(group_raw) else {
            continue;
        };
        // Signal 0 is a permission probe only; it never delivers.
        if test_kill_process_group(group).err() == Some(Errno::PERM) {
            return Some(group);
        }
    }
    None
}
