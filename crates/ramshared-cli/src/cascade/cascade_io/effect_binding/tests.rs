// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/effect_binding/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::tests::{bound_device_fixture, lifecycle_binding_fixture};
use super::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

type ReadHook = Box<dyn FnOnce(&Path)>;
thread_local! {
    static BEFORE_OPEN: RefCell<Option<ReadHook>> = const { RefCell::new(None) };
    static BEFORE_READ: RefCell<Option<ReadHook>> = const { RefCell::new(None) };
}

pub(super) fn invoke_before_open(path: &Path) {
    BEFORE_OPEN.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() {
            callback(path);
        }
    });
}

pub(super) fn invoke_before_read(path: &Path) {
    BEFORE_READ.with(|hook| {
        if let Some(callback) = hook.borrow_mut().take() {
            callback(path);
        }
    });
}

struct Fixture {
    root: PathBuf,
    paths: RuntimePaths,
    binding: LifecycleBinding,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "binding-read-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let paths = RuntimePaths::under(&root);
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let binding = lifecycle_binding_fixture(vec![bound_device_fixture(
            "/dev/nbd0",
            ManagedDeviceKind::Nbd,
        )]);
        write_lifecycle_binding(&paths, &binding).unwrap();
        Self {
            root,
            paths,
            binding,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        BEFORE_OPEN.with(|hook| hook.borrow_mut().take());
        BEFORE_READ.with(|hook| hook.borrow_mut().take());
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn growing_lifecycle_binding_is_bounded_and_keeps_evidence() {
    let fixture = Fixture::new();
    let path = &fixture.paths.lifecycle_binding_file;
    fs::write(&fixture.paths.pid_file, b"retained ownership evidence").unwrap();
    BEFORE_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|path| {
            let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
            // Trailing whitespace is valid JSON; a decoder refusal is not a size bound.
            file.write_all(&vec![b' '; 1024 * 1024]).unwrap();
        }))
    });
    let error = read_lifecycle_binding(&fixture.paths)
        .expect_err("growth after metadata observation must not bypass the binding read limit");
    assert!(
        error.to_string().contains("bounded lifecycle binding read"),
        "{error}"
    );
    assert!(fs::metadata(path).unwrap().len() > LIFECYCLE_BINDING_MAX_BYTES);
    assert_eq!(
        fs::read(&fixture.paths.pid_file).unwrap(),
        b"retained ownership evidence"
    );
}

#[test]
fn lifecycle_binding_read_accepts_the_exact_bound_and_refuses_unsealed_inputs() {
    let fixture = Fixture::new();
    let path = &fixture.paths.lifecycle_binding_file;
    let mut payload = serde_json::to_vec(&fixture.binding).unwrap();
    payload.resize(LIFECYCLE_BINDING_MAX_BYTES as usize, b' ');
    fs::write(path, &payload).unwrap();
    let read = read_lifecycle_binding(&fixture.paths).unwrap();
    assert_eq!(read.daemon_instance_id, fixture.binding.daemon_instance_id);
    fs::hard_link(path, path.with_extension("alias")).unwrap();
    assert!(read_lifecycle_binding(&fixture.paths).is_err());
    fs::remove_file(path.with_extension("alias")).unwrap();
    payload.push(b' ');
    fs::write(path, &payload).unwrap();
    assert!(read_lifecycle_binding(&fixture.paths).is_err());
    fs::write(path, b"").unwrap();
    assert!(read_lifecycle_binding(&fixture.paths).is_err());
    fs::write(path, b"{}").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_lifecycle_binding(&fixture.paths).is_err());
    fs::remove_file(path).unwrap();
    fs::create_dir(path).unwrap();
    assert!(read_lifecycle_binding(&fixture.paths).is_err());
}

#[test]
fn binding_read_failure_prevents_recovery_effects() {
    struct NoEffects(RefCell<Vec<String>>);
    impl CommandRunner for NoEffects {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(command_label(command, args));
            Err(CascadeError::UnsafeContainment(
                "unexpected recovery effect".into(),
            ))
        }
    }
    let fixture = Fixture::new();
    fs::write(&fixture.paths.pid_file, b"retained peer identity").unwrap();
    let marker = &fixture.paths.forensics_markers[0];
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(marker, b"retained forensic evidence").unwrap();
    BEFORE_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|path| {
            fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(&vec![b' '; LIFECYCLE_BINDING_MAX_BYTES as usize])
                .unwrap();
        }))
    });
    let runner = NoEffects(RefCell::new(Vec::new()));
    let error = super::super::teardown::down_recover_released_peer_with(&runner, &fixture.paths)
        .unwrap_err();
    assert!(
        error.to_string().contains("bounded lifecycle binding read"),
        "{error}"
    );
    assert!(runner.0.borrow().is_empty());
    assert_eq!(
        fs::read(&fixture.paths.pid_file).unwrap(),
        b"retained peer identity"
    );
    assert_eq!(fs::read(marker).unwrap(), b"retained forensic evidence");
    assert!(
        fs::metadata(&fixture.paths.lifecycle_binding_file)
            .unwrap()
            .len()
            > LIFECYCLE_BINDING_MAX_BYTES
    );
}

#[test]
fn lifecycle_binding_refuses_metadata_changes_after_open() {
    let mut accepted = Vec::new();
    for change in ["hardlink", "mode", "rewrite", "replace"] {
        let fixture = Fixture::new();
        let mut other = fixture.binding.clone();
        other.origin.configuration_sha256 = "c".repeat(64);
        let changed = serde_json::to_vec(&other).unwrap();
        assert_eq!(
            changed.len(),
            fs::metadata(&fixture.paths.lifecycle_binding_file)
                .unwrap()
                .len() as usize
        );
        BEFORE_READ.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path| match change {
                "hardlink" => fs::hard_link(path, path.with_extension("alias")).unwrap(),
                "mode" => fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap(),
                "rewrite" => fs::write(path, changed).unwrap(),
                "replace" => {
                    fs::rename(path, path.with_extension("original")).unwrap();
                    fs::write(path, changed).unwrap();
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
                }
                _ => unreachable!(),
            }))
        });
        if read_lifecycle_binding(&fixture.paths).is_ok() {
            accepted.push(change);
        }
        assert!(fixture.paths.lifecycle_binding_file.exists());
    }
    assert!(
        accepted.is_empty(),
        "changed binding metadata admitted: {accepted:?}"
    );
}

#[test]
fn lifecycle_binding_open_refuses_fifo_replacement() {
    const CHILD_PATH: &str = "RAMSHARED_TEST_BINDING_FIFO";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let path = PathBuf::from(path);
        let paths = RuntimePaths {
            lifecycle_binding_file: path.clone(),
            ..RuntimePaths::under(path.parent().unwrap())
        };
        BEFORE_OPEN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|path| {
                fs::rename(path, path.with_extension("original")).unwrap();
                assert!(
                    std::process::Command::new("mkfifo")
                        .arg(path)
                        .status()
                        .unwrap()
                        .success()
                );
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }))
        });
        assert!(read_lifecycle_binding(&paths).is_err());
        fs::write(path.with_extension("refused"), b"reader-refused-fifo").unwrap();
        return;
    }
    let fixture = Fixture::new();
    let path = &fixture.paths.lifecycle_binding_file;
    let original = fs::read(path).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cascade::cascade_io::effect_binding::binding_read_tests::lifecycle_binding_open_refuses_fifo_replacement"])
        .env(CHILD_PATH, path)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let outcome = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        sleep(Duration::from_millis(10));
    };
    let witness = fs::read(path.with_extension("refused")).ok();
    assert_eq!(fs::read(path.with_extension("original")).unwrap(), original);
    assert!(
        outcome.is_some_and(|status| status.success())
            && witness.as_deref() == Some(b"reader-refused-fifo".as_slice()),
        "binding open waited for a FIFO writer instead of refusing the replacement"
    );
}

#[test]
fn effect_binding_validates_pinned_fd_metadata_and_refuses_bad_bindings() {
    let fixture = Fixture::new();
    let regular_file = fixture.root.join("not-a-block-device");
    fs::write(&regular_file, b"plain").unwrap();
    let regular_meta = fs::metadata(&regular_file).unwrap();

    // Regular file fails pinned effect metadata check.
    assert!(validate_pinned_effect_metadata(&regular_meta, "43:0").is_err());

    // open_device_for_effect_with refuses open failures and non-block descriptors.
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    assert!(
        open_device_for_effect_with(&nbd, |_| Ok(()), |_| Err(std::io::Error::other("io")))
            .is_err()
    );
    assert!(
        open_device_for_effect_with(&nbd, |_| Ok(()), |_| fs::File::open(&regular_file)).is_err()
    );

    // EffectBoundDevice reports path and refuses missing/drifted device identity on revalidation.
    let bound = bind_legacy_device_for_effect(&nbd).unwrap();
    assert_eq!(bound.path(), "/dev/nbd0");
    let mut drifted_identity = bound_device_fixture("/dev/nbd250", ManagedDeviceKind::Nbd);
    drifted_identity.dev_t = "99:99".into();
    let drifted = EffectBoundDevice {
        identity: drifted_identity,
        effect_path: "/dev/nbd250".into(),
    };
    assert!(drifted.revalidate_named_identity().is_err());

    // write_lifecycle_binding refuses invalid schema/cardinality before touching disk.
    let mut bad_binding = fixture.binding.clone();
    bad_binding.devices.clear();
    assert!(write_lifecycle_binding(&fixture.paths, &bad_binding).is_err());
}
