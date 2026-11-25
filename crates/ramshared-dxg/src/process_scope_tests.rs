// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-dxg/src/process_scope_tests.rs
//! Privileged native namespace regression; no driver or GPU allocation.
use super::{AdapterLuid, BudgetProcess, BudgetQueryScope, DxgError, query_memory_snapshot};
use std::cell::Cell;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TEST: &str = "process_scope_tests::retained_scope_rejects_same_pid_in_another_namespace";
const FIXTURE: &str = "RAMSHARED_DXG_NATIVE_NAMESPACE_FIXTURE";

#[test]
#[ignore = "requires root and nested Linux PID namespaces"]
fn retained_scope_rejects_same_pid_in_another_namespace() {
    if std::env::var_os(FIXTURE).is_some() {
        namespace_fixture();
        return;
    }
    let executable = std::env::current_exe().unwrap_or_else(|error| panic!("test image: {error}"));
    let mut child = Command::new("unshare")
        .args(["--pid", "--fork", "--kill-child", "--mount-proc"])
        .arg(executable)
        .args([
            TEST,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FIXTURE, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("namespace supervisor: {error}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child
            .try_wait()
            .unwrap_or_else(|error| panic!("fixture wait: {error}"))
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("namespace fixture exceeded its absolute deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("fixture output: {error}"));
    assert!(
        output.status.success(),
        "native namespace fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn namespace_fixture() {
    assert_eq!(std::process::id(), 1, "outer fixture must own PID 1");
    let scope = BudgetQueryScope::current_process_local(0);
    assert!(
        scope.is_current_process_local(),
        "legitimate creator scope refused"
    );
    let called = Cell::new(false);
    assert!(sample(&scope, &called).is_ok() && called.get());
    // SAFETY: after fork, the child uses only native namespace/process/metadata
    // syscalls, stack data and _exit. It performs no driver call, allocation,
    // formatting, unwinding or operation on an inherited Rust lock.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork of namespace fixture failed");
    if child == 0 {
        // SAFETY: this fork child has one thread; native failures terminate it
        // with a distinct setup status. The outer namespace retains custody.
        unsafe { enter_alias(scope) };
    }
    let mut status = 0;
    // SAFETY: child is the exact unreaped child above and status is writable.
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(
        libc::WIFEXITED(status),
        "namespace child did not exit normally"
    );
    let code = libc::WEXITSTATUS(status);
    if code == 37 {
        eprintln!("namespace_alias_query_effects=accepted");
    }
    assert_eq!(
        code, 0,
        "retained scope authorized a different PID-1 incarnation; setup failures use 40..=43"
    );
}

unsafe fn enter_alias(scope: BudgetQueryScope) -> ! {
    // SAFETY: this function is reached only in the single-threaded fork child.
    // Each native call receives valid scalar arguments or live stack storage.
    unsafe {
        if libc::unshare(libc::CLONE_NEWPID) != 0 {
            libc::_exit(40);
        }
        let inner = libc::fork();
        if inner < 0 {
            libc::_exit(41);
        }
        if inner == 0 {
            if libc::getpid() != 1 || scope.process != BudgetProcess::Linux(1) {
                libc::_exit(42);
            }
            let called = Cell::new(false);
            let result = sample(&scope, &called);
            let refused = matches!(result, Err(DxgError::Malformed("query_scope")))
                && !called.get()
                && !scope.belongs_to_current_process();
            libc::_exit(if refused { 0 } else { 37 });
        }
        let mut status = 0;
        if libc::waitpid(inner, &mut status, 0) != inner || !libc::WIFEXITED(status) {
            libc::_exit(43);
        }
        libc::_exit(libc::WEXITSTATUS(status));
    }
}

fn sample(
    scope: &BudgetQueryScope,
    called: &Cell<bool>,
) -> Result<super::BudgetSnapshot, DxgError> {
    query_memory_snapshot(9, AdapterLuid { low: 7, high: 8 }, scope, |query| {
        called.set(true);
        query.budget = 100;
        Ok(())
    })
}
