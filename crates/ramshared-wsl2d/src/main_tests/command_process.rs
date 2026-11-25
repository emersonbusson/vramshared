// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/command_process.rs
//! Command output, exit observation and exact process-group custody.

use super::*;
use std::thread::sleep;

#[test]
fn daemon_command_timeout_terminates_child_without_hang() {
    static OWNER: std::sync::OnceLock<command_runtime::Owner> = std::sync::OnceLock::new();
    let _scope = command_runtime::test_scope(OWNER.get_or_init(command_runtime::test_owner));
    let output = command_stdout_with_timeout(
        "head",
        &["-c", "131072", "/dev/zero"],
        Duration::from_secs(5),
    )
    .expect("a finite child that fills stdout must be drained before its deadline");
    assert_eq!(output.len(), 131072);

    assert_eq!(
        command_stdout_with_timeout("false", &[], Duration::from_secs(1)),
        None
    );
    assert_eq!(
        command_stdout_with_timeout(
            "definitely-not-a-ramshared-command",
            &[],
            Duration::from_secs(1)
        ),
        None
    );
    let started = Instant::now();
    assert_eq!(
        command_stdout_with_timeout("sleep", &["1"], Duration::from_millis(25)),
        None
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a timed-out child must be reaped, not left running"
    );
}

#[test]
// TestName: daemon_command_success_reaps_all_stdio_redirected_descendant
fn daemon_command_success_reaps_all_stdio_redirected_descendant() {
    static OWNER: std::sync::OnceLock<command_runtime::Owner> = std::sync::OnceLock::new();
    let _scope = command_runtime::test_scope(OWNER.get_or_init(command_runtime::test_owner));
    let output = command_stdout_with_timeout(
        "/bin/sh",
        &[
            "-c",
            "sleep 10 </dev/null >/dev/null 2>&1 & printf '%s\\n' \"$!\"",
        ],
        Duration::from_secs(1),
    )
    .expect("successful helper leader remains legitimate");
    let descendant = output
        .trim()
        .parse::<u32>()
        .expect("fixture prints its exact descendant PID");
    let descendant_path = std::path::PathBuf::from(format!("/proc/{descendant}"));
    let deadline = Instant::now() + Duration::from_secs(1);
    while descendant_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let gone = !descendant_path.exists();
    // The fixture descendant expires after ten seconds even on failure.
    // Its printed PID is not authority to signal after the leader was reaped.

    assert!(
        gone,
        "daemon helper left an all-stdio-redirected owned descendant alive"
    );
}

#[test]
// TestName: daemon_command_timeout_reaps_term_ignoring_fixture
fn daemon_command_timeout_reaps_term_ignoring_fixture() {
    static OWNER: std::sync::OnceLock<command_runtime::Owner> = std::sync::OnceLock::new();
    let _scope = command_runtime::test_scope(OWNER.get_or_init(command_runtime::test_owner));
    let started = Instant::now();
    assert_eq!(
        command_stdout_with_timeout(
            "/bin/sh",
            &["-c", "trap '' TERM; while :; do :; done"],
            Duration::from_millis(25),
        ),
        None
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
// TestName: daemon_command_contains_inherited_output_and_bounds_capture
fn daemon_command_contains_inherited_output_and_bounds_capture() {
    static OWNER: std::sync::OnceLock<command_runtime::Owner> = std::sync::OnceLock::new();
    let _scope = command_runtime::test_scope(OWNER.get_or_init(command_runtime::test_owner));
    let root = std::env::temp_dir().join(format!(
        "ramshared-daemon-command-child-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let program = root.join("inherited-output");
    std::fs::write(
        &program,
        "#!/bin/sh\n(sleep 1) &\nprintf '4096\\n'\nexit 0\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&program).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&program, permissions).unwrap();

    let started = Instant::now();
    assert_eq!(
        command_stdout_with_timeout(program.to_str().unwrap(), &[], Duration::from_millis(100),),
        None,
        "a descendant-held output pipe must fail closed",
    );
    assert!(
        started.elapsed() < Duration::from_millis(750),
        "descendant-held output outlived the bounded command deadline"
    );
    assert_eq!(
        command_stdout_with_timeout(
            "head",
            &["-c", "1048576", "/dev/zero"],
            Duration::from_secs(2),
        ),
        None,
        "command output storage must have a finite upper bound",
    );
    std::fs::remove_dir_all(root).unwrap();
}

thread_local! {
    static OBSERVATION_FAULT: Cell<bool> = const { Cell::new(false) };
}

pub(in super::super) fn command_observation_fault() -> bool {
    let fail = OBSERVATION_FAULT.with(Cell::get);
    if fail && let Some(root) = std::env::var_os("RAMSHARED_TEST_HELPER_CUSTODY_CHILD") {
        std::fs::write(Path::new(&root).join("fault-observed"), b"observed").unwrap();
    }
    fail
}

use std::io::Write as _;
use std::os::unix::net::UnixStream;
const CHILD_ROOT: &str = "RAMSHARED_TEST_HELPER_CUSTODY_CHILD";

#[test]
fn origin_identity_probe_does_not_execute_path_override() {
    const PROBE_ROOT: &str = "RAMSHARED_TEST_IDENTITY_PATH_CHILD";
    if let Some(root) = std::env::var_os(PROBE_ROOT) {
        let root = PathBuf::from(root);
        let result = block_identity_value(&root.join("empty-origin"), "UUID");
        assert!(
            result.is_err() && !root.join("path-helper-ran").exists(),
            "origin identity trusted a helper selected through PATH: {result:?}"
        );
        std::fs::write(root.join("probe-checked"), b"checked").unwrap();
        return;
    }
    let root = std::env::temp_dir().join(format!("oid-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("empty-origin"), b"").unwrap();
    let fake = root.join("blkid");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf ran > \"$RAMSHARED_TEST_IDENTITY_PATH_CHILD/path-helper-ran\"\nprintf '11111111-2222-3333-4444-555555555555\\n'\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let stderr = File::create(root.join("child.stderr")).unwrap();
    let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::command_process::origin_identity_probe_does_not_execute_path_override",
        ])
        .env(PROBE_ROOT, &root)
        .env("PATH", &root)
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
        }
        if Instant::now() >= deadline + Duration::from_secs(1) {
            break None;
        }
        sleep(Duration::from_millis(10));
    };
    let checked = root.join("probe-checked").exists();
    let untrusted_ran = root.join("path-helper-ran").exists();
    let detail = std::fs::read_to_string(root.join("child.stderr")).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        checked && status.is_some_and(|status| status.success()),
        "native identity helper selection failed: {status:?}; PATH override executed={untrusted_ran}; {detail}"
    );
}

fn serve_fixture_origin(listener: UnixListener, origin: File) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let backend = AuthoritativeOriginBackend::new(
            FileOrigin::from_file(origin),
            DisabledCache,
            65536,
            4096,
        )
        .unwrap();
        let mut backend = &backend;
        let (mut stream, _) = listener.accept().unwrap();
        loop {
            let mut command = [0];
            stream.read_exact(&mut command).unwrap();
            let mut page = [0; 4096];
            match command[0] {
                b'W' => {
                    stream.read_exact(&mut page).unwrap();
                    BlockBackend::write_at(&mut backend, 0, &page).unwrap();
                    BlockBackend::flush(&mut backend).unwrap();
                    stream.write_all(b"W").unwrap();
                }
                b'R' => {
                    BlockBackend::read_at(&mut backend, 0, &mut page).unwrap();
                    stream.write_all(&page).unwrap();
                }
                b'Q' => break,
                _ => panic!("invalid fixture command"),
            }
        }
    })
}

fn helper_custody_child(root: PathBuf) {
    let listener = UnixListener::bind(root.join("data.sock")).unwrap();
    let control = UnixListener::bind(root.join("control.sock")).unwrap();
    let origin = File::options()
        .create_new(true)
        .read(true)
        .write(true)
        .open(root.join("origin.bin"))
        .unwrap();
    origin.set_len(65536).unwrap();
    let serving = serve_fixture_origin(listener, origin);
    let (mut control, _) = control.accept().unwrap();
    let mut command = [0];
    control.read_exact(&mut command).unwrap();
    assert_eq!(command, *b"F");
    OBSERVATION_FAULT.with(|fault| fault.set(true));
    assert!(
        command_stdout_with_timeout("/bin/sleep", &["10"], Duration::from_millis(10)).is_none()
    );
    assert!(
        command_runtime::pending(),
        "the actual helper must remain owned"
    );
    assert!(command_runtime::try_claim_shutdown().is_none());
    let forbidden = root.join("must-not-spawn");
    assert!(
        command_stdout_with_timeout(
            "/bin/touch",
            &[forbidden.to_str().unwrap()],
            Duration::from_millis(10)
        )
        .is_none()
    );
    assert!(!forbidden.exists());
    control.write_all(b"P").unwrap();
    control.read_exact(&mut command).unwrap();
    assert_eq!(command, *b"C");
    OBSERVATION_FAULT.with(|fault| fault.set(false));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !command_runtime::reconcile() && Instant::now() < deadline {
        sleep(Duration::from_millis(10));
    }
    assert!(!command_runtime::pending());
    assert_eq!(
        command_stdout_with_timeout("/bin/echo", &["resumed"], Duration::from_secs(1)).as_deref(),
        Some("resumed\n")
    );
    control.write_all(b"C").unwrap();
    serving.join().unwrap();
}

fn read_fixture(data: &mut UnixStream, expected: &[u8; 4096]) -> std::io::Result<()> {
    let mut read = [0; 4096];
    data.write_all(b"R")?;
    data.read_exact(&mut read)?;
    assert_eq!(&read, expected);
    Ok(())
}
fn write_fixture(data: &mut UnixStream, page: &[u8; 4096]) -> std::io::Result<()> {
    let mut ack = [0];
    data.write_all(b"W")?;
    data.write_all(page)?;
    data.read_exact(&mut ack)?;
    assert_eq!(ack, *b"W");
    Ok(())
}
fn exercise_fixture(root: &Path) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !root.join("control.sock").exists() && Instant::now() < deadline {
        sleep(Duration::from_millis(10));
    }
    let mut data = UnixStream::connect(root.join("data.sock"))?;
    data.set_read_timeout(Some(Duration::from_secs(2)))?;
    data.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut control = UnixStream::connect(root.join("control.sock"))?;
    control.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut ack = [0];
    write_fixture(&mut data, &[0xa7; 4096])?;
    read_fixture(&mut data, &[0xa7; 4096])?;
    control.write_all(b"F")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !root.join("fault-observed").exists() && Instant::now() < deadline {
        sleep(Duration::from_millis(10));
    }
    assert!(
        root.join("fault-observed").exists(),
        "failure injection must execute"
    );
    read_fixture(&mut data, &[0xa7; 4096])?;
    control.read_exact(&mut ack)?;
    assert_eq!(ack, *b"P");
    write_fixture(&mut data, &[0x3b; 4096])?;
    read_fixture(&mut data, &[0x3b; 4096])?;
    control.write_all(b"C")?;
    control.read_exact(&mut ack)?;
    assert_eq!(ack, *b"C");
    read_fixture(&mut data, &[0x3b; 4096])?;
    data.write_all(b"Q")?;
    Ok(())
}

#[test]
fn helper_containment_failure_keeps_origin_serving() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        helper_custody_child(PathBuf::from(root));
        return;
    }
    let root = std::env::temp_dir().join(format!("hcu-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let stderr = File::create(root.join("child.stderr")).unwrap();
    let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::command_process::helper_containment_failure_keeps_origin_serving",
        ])
        .env(CHILD_ROOT, &root)
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let result = exercise_fixture(&root);
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if result.is_err() || Instant::now() >= deadline {
            let _ = child.kill();
        }
        if Instant::now() >= deadline + Duration::from_secs(1) {
            break None;
        }
        sleep(Duration::from_millis(10));
    };
    let persisted = std::fs::read(root.join("origin.bin"))
        .ok()
        .is_some_and(|bytes| bytes.get(..4096) == Some([0x3b; 4096].as_slice()));
    let detail = std::fs::read_to_string(root.join("child.stderr")).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        result.is_ok() && status.is_some_and(|s| s.success()) && persisted,
        "origin service lost during helper uncertainty: {result:?}, {status:?}; {detail}"
    );
}
