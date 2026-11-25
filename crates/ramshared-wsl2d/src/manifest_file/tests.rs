// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/manifest_file/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::os::unix::fs::PermissionsExt;

type ManifestReadHook = Box<dyn Fn(&Path)>;

#[test]
fn manifest_fd_reader_refuses_fifo_without_waiting_for_a_writer() {
    const CHILD_PATH: &str = "RAMSHARED_TEST_MANIFEST_FIFO";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let path = Path::new(&path);
        let error = read_bounded_manifest_file(path, false).unwrap_err();
        assert!(error.contains("sealed regular file"), "{error}");
        std::fs::write(path.with_extension("refused"), b"reader-refused-fifo").unwrap();
        return;
    }
    let dir = std::env::temp_dir().join(format!("manifest-fifo-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("manifest");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "manifest_file::tests::manifest_fd_reader_refuses_fifo_without_waiting_for_a_writer",
        ])
        .env(CHILD_PATH, &path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let outcome = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    // A child that runs zero tests also exits successfully. Require evidence
    // written only after the actual reader returned the expected refusal.
    let refusal = std::fs::read(path.with_extension("refused")).ok();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(
        outcome.is_some_and(|status| status.success())
            && refusal.as_deref() == Some(b"reader-refused-fifo".as_slice()),
        "manifest reader waited for a FIFO writer instead of refusing the nonregular input"
    );
}

thread_local! {
    static MANIFEST_READ_HOOK: std::cell::RefCell<Option<ManifestReadHook>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) fn invoke_manifest_read_hook(path: &Path) {
    MANIFEST_READ_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(path);
        }
    });
}

#[test]
// TestName: manifest_fd_reader_accepts_one_bounded_regular_file
fn manifest_fd_reader_accepts_one_bounded_regular_file() {
    let dir = std::env::temp_dir().join(format!("ramshared-manifest-valid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("manifest.json");
    std::fs::write(&path, b"{\"schema\":3}\n").unwrap();
    assert_eq!(
        read_bounded_manifest_file(&path, false).unwrap(),
        b"{\"schema\":3}\n"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
// TestName: manifest_fd_reader_refuses_oversize_symlink_and_nonregular_inputs
fn manifest_fd_reader_refuses_oversize_symlink_and_nonregular_inputs() {
    let dir = std::env::temp_dir().join(format!("ramshared-manifest-types-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let oversized = dir.join("oversized");
    std::fs::write(
        &oversized,
        vec![b'x'; ORIGIN_MANIFEST_MAX_BYTES as usize + 1],
    )
    .unwrap();
    assert!(read_bounded_manifest_file(&oversized, false).is_err());

    let legitimate = dir.join("legitimate");
    std::fs::write(&legitimate, b"bounded").unwrap();
    let symlink = dir.join("symlink");
    std::os::unix::fs::symlink(&legitimate, &symlink).unwrap();
    assert!(read_bounded_manifest_file(&symlink, false).is_err());
    let parent_link = dir.join("parent-link");
    std::os::unix::fs::symlink(&dir, &parent_link).unwrap();
    assert!(read_bounded_manifest_file(&parent_link.join("legitimate"), false).is_err());
    assert!(read_bounded_manifest_file(&dir, false).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
// TestName: manifest_fd_reader_bounds_concurrent_append_at_max_plus_one
fn manifest_fd_reader_bounds_concurrent_append_at_max_plus_one() {
    use std::io::Write as _;

    let dir =
        std::env::temp_dir().join(format!("ramshared-manifest-append-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("manifest");
    std::fs::write(&path, vec![b'a'; ORIGIN_MANIFEST_MAX_BYTES as usize]).unwrap();
    MANIFEST_READ_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|opened_path| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(opened_path)
                .unwrap();
            file.write_all(&vec![b'b'; 1024 * 1024]).unwrap();
        }));
    });
    let error =
        read_bounded_manifest_file(&path, false).expect_err("an append after open must be refused");
    assert!(error.contains("bounded read limit"), "{error}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
// TestName: manifest_fd_reader_refuses_path_replacement_after_open
fn manifest_fd_reader_refuses_path_replacement_after_open() {
    let dir =
        std::env::temp_dir().join(format!("ramshared-manifest-replace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("manifest");
    std::fs::write(&path, b"original").unwrap();
    MANIFEST_READ_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|opened_path| {
            std::fs::remove_file(opened_path).unwrap();
            std::fs::write(opened_path, b"replaced").unwrap();
        }));
    });
    assert!(read_bounded_manifest_file(&path, false).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"replaced");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn manifest_fd_reader_refuses_link_and_same_length_mutation() {
    let mut accepted = Vec::new();
    for kind in ["link", "rewrite", "mode"] {
        let dir =
            std::env::temp_dir().join(format!("manifest-change-{}-{kind}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("manifest");
        std::fs::write(&path, b"original").unwrap();
        let file = File::options().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        MANIFEST_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |opened| match kind {
                "link" => std::fs::hard_link(opened, opened.with_file_name("alias")).unwrap(),
                "rewrite" => std::fs::write(opened, b"modified").unwrap(),
                _ => std::fs::set_permissions(opened, std::fs::Permissions::from_mode(0o666))
                    .unwrap(),
            }));
        });
        let result = read_bounded_manifest_file(&path, false);
        std::fs::remove_dir_all(&dir).unwrap();
        if result.is_ok() {
            accepted.push(kind);
        }
    }
    assert!(
        accepted.is_empty(),
        "changed metadata admitted: {accepted:?}"
    );
}
