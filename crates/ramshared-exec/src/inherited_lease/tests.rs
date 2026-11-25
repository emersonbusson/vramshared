// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/inherited_lease/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("local/tests/inherited_lease")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
    fn path(&self) -> std::path::PathBuf {
        self.0.join("probe.lock")
    }
    fn uid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn inherited_lease_refuses_redirects_and_unsafe_metadata() {
    let fixture = Fixture::new();
    let path = fixture.path();
    let held = acquire(&path, fixture.uid()).unwrap();
    validate(&held, &path, fixture.uid()).unwrap();
    assert!(acquire(&path, fixture.uid()).is_err());
    assert!(validate(&held, &path, fixture.uid().wrapping_add(1)).is_err());
    drop(held);
    for mode in [0o644, 0o666, 0o1600] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(acquire(&path, fixture.uid()).is_err());
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&path, fixture.0.join("alias")).unwrap();
    assert!(acquire(&path, fixture.uid()).is_err());
    std::fs::remove_file(fixture.0.join("alias")).unwrap();
    let link = fixture.0.join("link");
    symlink(&path, &link).unwrap();
    assert!(acquire(&link, fixture.uid()).is_err());
    let linked_parent = fixture.0.join("parent");
    symlink(&fixture.0, &linked_parent).unwrap();
    assert!(acquire(&linked_parent.join("probe.lock"), fixture.uid()).is_err());
    assert!(acquire(Path::new("relative.lock"), fixture.uid()).is_err());
    std::fs::write(&path, b"foreign data").unwrap();
    assert!(acquire(&path, fixture.uid()).is_err());
    std::fs::remove_file(&path).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::Fifo,
        Mode::RUSR | Mode::WUSR,
        0,
    )
    .unwrap();
    assert!(acquire(&path, fixture.uid()).is_err());
    std::fs::remove_file(&path).unwrap();
    std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(acquire(&path, fixture.uid()).is_err());
}

#[test]
fn inherited_lease_named_identity_is_rechecked_at_handoff() {
    let fixture = Fixture::new();
    let path = fixture.path();
    let held = acquire(&path, fixture.uid()).unwrap();
    let renamed = fixture.0.join("original.lock");
    std::fs::rename(&path, &renamed).unwrap();
    let replacement = acquire(&path, fixture.uid()).unwrap();
    assert!(validate(&held, &path, fixture.uid()).is_err());
    assert!(
        acquire(&renamed, fixture.uid()).is_err(),
        "failed validation must retain the original lock"
    );
    drop(replacement);
    drop(held);
    assert!(acquire(&renamed, fixture.uid()).is_ok());
}

#[test]
fn inherited_lease_blocks_replacement_until_actual_child_closes_stdin() {
    let fixture = Fixture::new();
    let path = fixture.path();
    let lease = acquire(&path, fixture.uid()).unwrap();
    // The child keeps inherited stdin (the lease); a distinct pipe is the
    // test's release gate. No sleeps substitute for a child ownership proof.
    let (read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
    let fd: std::os::fd::OwnedFd = read.into();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "read line <&2"])
        .stdin(Stdio::from(lease))
        .stderr(Stdio::from(fd));
    let mut child = command.spawn().unwrap();
    drop(command);
    assert!(child.try_wait().unwrap().is_none());
    assert!(acquire(&path, fixture.uid()).is_err());
    write.write_all(b"release\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(acquire(&path, fixture.uid()).is_ok());
}

#[test]
fn recorded_output_matches_actual_owned_native_child() {
    let owner = Box::leak(Box::new(
        crate::retained_process::ServingCommandOwner::default(),
    ));
    let result = owner
        .run_recorded(
            Command::new("/bin/sh").args(["-c", "printf '%s' \"$$\""]),
            std::time::Duration::from_secs(1),
            64,
        )
        .unwrap();
    assert_eq!(result.output.stdout, result.pid.to_string().as_bytes());
    assert_eq!(owner.state(), crate::retained_process::HelperState::Idle);
}
