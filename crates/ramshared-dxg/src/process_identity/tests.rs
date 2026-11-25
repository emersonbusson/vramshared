// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-dxg/src/process_identity/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use crate::BudgetQueryScope;
use std::os::unix::fs::MetadataExt;

fn stat(comm: &[u8], ticks: &[u8]) -> Vec<u8> {
    let mut bytes = b"123 (".to_vec();
    bytes.extend_from_slice(comm);
    bytes.extend_from_slice(b") S");
    for _ in 4..=21 {
        bytes.extend_from_slice(b" 0");
    }
    bytes.push(b' ');
    bytes.extend_from_slice(ticks);
    for _ in 23..=52 {
        bytes.extend_from_slice(b" 0");
    }
    bytes.push(b'\n');
    bytes
}

#[test]
fn native_current_process_has_stable_complete_incarnation() {
    let identity = LinuxProcessIdentity::current().unwrap();
    assert_eq!(identity.observed.pid, std::process::id());
    assert!(identity.observed.namespace_inode > 0);
    assert!(identity.observed.boot_id.iter().any(|byte| *byte != 0));
    let namespace = std::fs::metadata("/proc/self/ns/pid").unwrap();
    assert_eq!(
        (
            identity.observed.namespace_device,
            identity.observed.namespace_inode
        ),
        (namespace.dev(), namespace.ino())
    );
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let actual: String = identity
        .observed
        .boot_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(actual, boot.trim().replace('-', ""));
    assert_eq!(Some(identity), LinuxProcessIdentity::current());
}

#[test]
fn equal_pid_cannot_replace_generation_namespace_or_boot() {
    let scope = BudgetQueryScope::current_process_local(0);
    let identity = scope.process_identity.as_ref().unwrap().clone();
    assert!(scope.belongs_to_current_process());
    let mut changed: [LinuxProcessIdentity; 5] = std::array::from_fn(|_| identity.clone());
    changed[0].observed.pid ^= 1;
    changed[1].observed.start_ticks ^= 1;
    changed[2].observed.namespace_device ^= 1;
    changed[3].observed.namespace_inode ^= 1;
    changed[4].observed.boot_id[0] ^= 1;
    for identity in changed {
        assert!(
            !BudgetQueryScope {
                process_identity: Some(identity),
                ..scope.clone()
            }
            .belongs_to_current_process()
        );
    }
    assert!(
        !BudgetQueryScope {
            process_identity: None,
            ..scope.clone()
        }
        .is_current_process_local()
    );
}

#[test]
fn stat_parser_preserves_kernel_ticks_with_arbitrary_command_bytes() {
    for comm in [b"daemon".as_slice(), b"", b"space ) inner (\xff\nname"] {
        assert_eq!(
            parse_start_ticks(&stat(comm, b"18446744073709551615")),
            Some(u64::MAX)
        );
    }
    let mut bytes = stat(b"x", b"42");
    bytes.splice(..3, b"4294967295".iter().copied());
    assert_eq!(parse_start_ticks(&bytes), Some(42));
}

#[test]
fn zero_start_ticks_are_observed_rather_than_missing() {
    assert_eq!(parse_start_ticks(&stat(b"early-init", b"0")), Some(0));
}

#[test]
fn stat_parser_refuses_missing_truncated_or_overflowed_authority() {
    for ticks in [b"".as_slice(), b"-1", b"+1", b"1x", b"18446744073709551616"] {
        assert_eq!(parse_start_ticks(&stat(b"x", ticks)), None);
    }
    for bytes in [
        b"".as_slice(),
        b"123 no-command S 1\n",
        b"123 (x) S 1\n",
        b"123 (x S 1\n",
    ] {
        assert_eq!(parse_start_ticks(bytes), None);
    }
    for pid in [
        b"0".as_slice(),
        b"-1",
        b"4294967296",
        b"18446744073709551616",
    ] {
        let mut bytes = stat(b"x", b"42");
        bytes.splice(..3, pid.iter().copied());
        assert_eq!(parse_start_ticks(&bytes), None);
    }
    let mut bytes = stat(b"x", b"42");
    bytes.pop();
    assert_eq!(parse_start_ticks(&bytes), None);
    let mut bytes = stat(b"x", b"42");
    bytes[8] = b'?';
    assert_eq!(parse_start_ticks(&bytes), None);
}

#[test]
fn boot_identity_requires_exact_nonzero_canonical_kernel_bytes() {
    let text = b"01234567-89ab-cdef-0123-456789abcdef";
    let expected = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];
    assert_eq!(parse_boot_id(text), Some(expected));
    assert_eq!(
        parse_boot_id(&[text.as_slice(), b"\n"].concat()),
        Some(expected)
    );
    for malformed in [
        b"".as_slice(),
        b"00000000-0000-0000-0000-000000000000",
        b"01234567-89AB-cdef-0123-456789abcdef",
        b"01234567_89ab-cdef-0123-456789abcdef",
        b"01234567-89ab-cdef-0123-456789abcdeg",
        b"01234567-89ab-cdef-0123-456789abcde",
        b"01234567-89ab-cdef-0123-456789abcdef\0",
    ] {
        assert_eq!(parse_boot_id(malformed), None);
    }
}

#[test]
fn native_observation_refuses_wrong_filesystem_missing_and_truncated_records() {
    assert!(open_native(c"/dev/null", PROC_MAGIC).is_none());
    assert!(open_native(c"/proc/self/stat", NS_MAGIC).is_none());
    assert!(read_proc::<4096>(c"/definitely/missing/ramshared-process").is_none());
    assert!(read_proc::<2>(c"/proc/self/stat").is_none());
}

fn exited_child_pidfd() -> OwnedFd {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut child = Command::new("/bin/sh")
        .args(["-c", "read value"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let fd = open_pidfd(child.id()).unwrap();
    assert!(pidfd_is_alive(&fd));
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("native lifetime fixture did not exit");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    fd
}

#[test]
fn owned_pidfd_retains_exact_exit_evidence_and_close_on_exec() {
    let fd = exited_child_pidfd();
    assert!(!pidfd_is_alive(&fd));
    // SAFETY: fd is a live owned descriptor; F_GETFD takes no extra argument.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0, "native descriptor flag query failed");
    assert_ne!(flags & libc::FD_CLOEXEC, 0);
    assert!(open_pidfd(0).is_none());
    assert!(open_pidfd(u32::MAX).is_none());
}

#[test]
fn dead_creator_handle_refuses_even_equal_current_process_observations() {
    let scope = BudgetQueryScope::current_process_local(0);
    // Deliberately equal stamps model a numeric/tick collision. The retained
    // kernel object refers to an actual exited child, not the current process.
    let dead = LinuxProcessIdentity {
        observed: ProcessObservation::current().unwrap(),
        lifetime: Arc::new(exited_child_pidfd()),
    };
    assert_eq!(scope.process_identity.as_ref(), Some(&dead));
    let stale = BudgetQueryScope {
        process_identity: Some(dead),
        ..scope
    };
    assert!(!stale.belongs_to_current_process());
    let fresh = BudgetQueryScope::current_process_local(0);
    assert_eq!(stale, fresh);
    assert!(!stale.matches_current_scope(&fresh));
    assert!(!fresh.matches_current_scope(&stale));
    assert!(fresh.matches_current_scope(&fresh.clone()));
    let called = std::cell::Cell::new(false);
    let result =
        crate::query_memory_snapshot(9, crate::AdapterLuid { low: 7, high: 8 }, &stale, |_| {
            called.set(true);
            Ok(())
        });
    assert!(matches!(
        result,
        Err(crate::DxgError::Malformed("query_scope"))
    ));
    assert!(!called.get(), "dead creator reached a native query effect");
}
