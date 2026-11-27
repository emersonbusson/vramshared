// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite.rs
#![allow(clippy::expect_used)]

pub(super) use super::legacy_migration::*;
pub(super) use super::*;
pub(super) use std::cell::{Cell, RefCell};
pub(super) use std::collections::VecDeque;
pub(super) use std::fs;
pub(super) use std::os::unix::fs::PermissionsExt;
pub(super) use std::process::Command;
pub(super) use std::sync::atomic::{AtomicUsize, Ordering};
pub(super) use std::time::Instant;

pub(super) static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

/// The exact attach label the product must produce. The shape is a literal
/// on purpose: these `ScriptedRunner` fixtures pin the argv independently
/// of the builder that emits it, so a change to `nbd_client_attach_args`
/// flag order or to the DT-14 flags fails here rather than silently.
/// The export name interpolates the one production constant, so the two
/// sides of an attach cannot drift on the name itself.
pub(super) fn expected_attach_label(socket: &str) -> String {
    format!("nbd-client -N {PRODUCTION_EXPORT_NAME} -unix {socket} /dev/nbd0 -timeout 30")
}

/// Same contract for the multi-connection form (`-C N` only above one).
pub(super) fn expected_attach_label_conns(socket: &str, conns: u32) -> String {
    format!(
        "nbd-client -N {PRODUCTION_EXPORT_NAME} -C {conns} -unix {socket} /dev/nbd0 -timeout 30"
    )
}

pub(super) fn nbd_attach_fixture() -> NbdAttachConfig<'static> {
    NbdAttachConfig {
        connections: 1,
        swap_dev: "/dev/nbd0",
        // The default posture swaps the tier in; the writeback mode is
        // the exception (the tier becomes zram's backing device).
        swap_the_tier: true,
        vram_prio: 100,
        disk_baseline_kib: 0,
        logical_capacity_kib: 64 * 1024,
        zram_device: "",
        origin_partuuid: "11111111-2222-4333-8444-555555555555",
        origin_ptuuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
        origin_partition_dev_t: "8:33",
        origin_parent_dev_t: "8:32",
        expected_swap_uuid: "99999999-8888-4777-8666-555555555555",
        host_manifest_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        configuration_sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    }
}

pub(super) fn up_args_fixture() -> UpArgs {
    UpArgs {
        accept_wsl2_nbd: false,
        zram_backing_dev: None,
        zram_writeback_tier: false,
        vram_mb: 64,
        zram_mb: 64,
        daemon: "ramsharedd".into(),
        force: false,
        connections: 1,
        transport: Transport::Nbd,
        swap_dev: "/dev/nbd0".into(),
        origin_path: "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555".into(),
        origin_partuuid: "11111111-2222-4333-8444-555555555555".into(),
        origin_ptuuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        origin_partition_dev_t: "8:33".into(),
        origin_parent_dev_t: "8:32".into(),
        expected_swap_uuid: "99999999-8888-4777-8666-555555555555".into(),
        host_manifest_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .into(),
        configuration_sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .into(),
        cache_cap_mib: 64,
        disk_baseline_kib: 0,
    }
}

pub(super) fn bound_device_fixture(path: &str, kind: ManagedDeviceKind) -> BoundDeviceIdentity {
    observe_bound_device(path, kind).expect("deterministic device identity fixture")
}

/// Kernel shape of a zram `comp_algorithm` line: every offered compressor,
/// with the active one bracketed. Tests use this so the verification reads
/// the same contract the kernel writes.
pub(super) fn pin_zram_comp_algorithm(paths: &RuntimePaths, device: &str, active: &str) {
    let name = device.rsplit('/').next().unwrap_or(device);
    let dir = paths.sys_block.join(name);
    fs::create_dir_all(&dir).expect("create comp_algorithm fixture dir");
    fs::write(
        dir.join("comp_algorithm"),
        format!("[{active}] lzo lzo-rle lz4 zstd deflate\n"),
    )
    .expect("write comp_algorithm fixture");
}

pub(super) fn lifecycle_binding_fixture(devices: Vec<BoundDeviceIdentity>) -> LifecycleBinding {
    LifecycleBinding {
        schema_version: LIFECYCLE_BINDING_SCHEMA,
        boot_id: "11111111-2222-4333-8444-555555555555".into(),
        daemon_invocation_id: "0123456789abcdef0123456789abcdef".into(),
        daemon_pid: 4242,
        daemon_start_ticks: 100,
        daemon_instance_id: "4242-100".into(),
        export_socket: BoundSocketIdentity {
            path: "/run/ramshared/wsl2d.sock".into(),
            filesystem_dev: 1,
            inode: 1,
        },
        origin: BoundOriginIdentity {
            partuuid: "11111111-2222-4333-8444-555555555555".into(),
            ptuuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            partition_dev_t: "8:33".into(),
            parent_dev_t: "8:32".into(),
            expected_swap_uuid: "99999999-8888-4777-8666-555555555555".into(),
            host_manifest_sha256:
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            configuration_sha256:
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        },
        devices,
    }
}

pub(super) struct TestDir {
    pub(super) path: PathBuf,
}

impl TestDir {
    pub(super) fn new() -> Self {
        let parent = std::env::temp_dir();
        for _ in 0..1024 {
            let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                "ramshared-cascade-io-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    panic!("create isolated cascade test directory {path:?}: {error}")
                }
            }
        }
        panic!("exhausted isolated cascade fixture names");
    }

    fn program(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path.join(name);
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let staged = self.path.join(format!(".{name}.{sequence}.staged"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .unwrap_or_else(|error| panic!("create staged test program {staged:?}: {error}"));
        use std::io::Write;
        file.write_all(source.as_bytes())
            .unwrap_or_else(|error| panic!("write staged test program {staged:?}: {error}"));
        file.sync_all()
            .unwrap_or_else(|error| panic!("sync staged test program {staged:?}: {error}"));
        drop(file);
        let mut permissions = fs::metadata(&staged)
            .unwrap_or_else(|error| panic!("stat staged test program {staged:?}: {error}"))
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&staged, permissions)
            .unwrap_or_else(|error| panic!("chmod staged test program {staged:?}: {error}"));
        fs::rename(&staged, &path)
            .unwrap_or_else(|error| panic!("publish test program {path:?}: {error}"));
        path
    }

    fn hold_writable_program(&self, name: &str, source: &str) -> fs::File {
        let path = self.program(name, source);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("open prior test program {path:?}: {error}"))
    }

    fn daemon_program(&self) -> PathBuf {
        // Portable single-process fixture whose `/proc/<pid>/comm` is
        // `ramsharedd` without symlinking `/bin/sleep` (multicall `uutils`
        // coreutils rejects unknown `argv[0]` names). Blocking on a piped
        // stdin keeps one process alive with zero child orphans until
        // SIGTERM/SIGKILL or drop.
        self.program("ramsharedd", "#!/bin/sh\nIFS= read -r _\n")
    }
}

pub(super) fn run_isolated_zram_setup_fixture() -> Result<(), String> {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .map_err(|error| format!("create isolated zram runtime: {error}"))?;
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/dev/zram7".into())),
        (
            "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
            Ok(String::new()),
        ),
        (
            "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
            Ok(String::new()),
        ),
        ("mkswap /dev/zram7".into(), Ok(String::new())),
        ("swapon -p 200 /dev/zram7".into(), Ok(String::new())),
    ]);
    set_live_device_snapshots([Ok(vec![bound_device_fixture(
        "/dev/zram7",
        ManagedDeviceKind::Zram,
    )])]);
    let device = setup_zram_with(&runner, &paths, 64, 200, None)
        .map_err(|error| format!("set up isolated zram fixture: {error}"))?;
    if device != "/dev/zram7" {
        return Err(format!("isolated zram fixture selected {device}"));
    }
    let recorded = read_bound_device_record(&paths.zram_dev_file)
        .map_err(|error| format!("read isolated zram record: {error}"))?;
    if recorded.path != "/dev/zram7" || recorded.kind != ManagedDeviceKind::Zram {
        return Err(format!("isolated zram record was {recorded:?}"));
    }
    let expected_calls = vec![
        "modprobe zram".to_string(),
        "zramctl --find".to_string(),
        "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string(),
        "bash -c echo 67108864 > /sys/block/zram7/disksize".to_string(),
        "mkswap /dev/zram7".to_string(),
        "swapon -p 200 /dev/zram7".to_string(),
    ];
    if runner.calls() != expected_calls {
        return Err("isolated zram fixture invoked the wrong command order".to_string());
    }
    Ok(())
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) fn as_program(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(super) fn run_fixture_script_bounded(
    script: &Path,
    timeout: Duration,
) -> Result<String, CascadeError> {
    let script = as_program(script);
    run_command_bounded_for("/bin/sh", &[script.as_str()], timeout)
}

pub(super) fn error_from<T>(result: Result<T, CascadeError>, expectation: &str) -> CascadeError {
    match result {
        Ok(_) => panic!("{expectation}"),
        Err(error) => error,
    }
}

pub(super) fn fixture_process_is_stopped(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .is_some_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, tail)| tail.starts_with('T') || tail.starts_with('t'))
        })
}

pub(super) struct FixtureDaemon(
    pub(super) std::process::Child,
    Option<std::process::ChildStdin>,
);

impl FixtureDaemon {
    pub(in crate::cascade::cascade_io) fn id(&self) -> u32 {
        self.0.id()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }

    fn child_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }
}

impl Drop for FixtureDaemon {
    fn drop(&mut self) {
        drop(self.1.take());
        // An already-reaped child must not be signalled again: its pid is
        // back in the free list and `kill` would address whoever reused it.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

pub(super) fn spawn_fixture_daemon(fixture: &TestDir, term_handler: &str) -> FixtureDaemon {
    let daemon_path = fixture.daemon_program();
    // Same lifetime contract as [`controlled_child`]: stdin stays open until
    // [`FixtureDaemon`]'s drop (or an explicit signal), avoiding any wall-clock
    // race under full-suite load.
    let mut child = {
        let mut attempts = 0u8;
        loop {
            match Command::new(&daemon_path)
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                Ok(child) => break child,
                Err(error) if error.raw_os_error() == Some(26) && attempts < 20 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("start fixture daemon: {error}"),
            }
        }
    };
    let stdin = child.stdin.take();
    let deadline = Instant::now() + Duration::from_millis(250);
    while child
        .try_wait()
        .unwrap_or_else(|error| panic!("inspect fixture daemon startup: {error}"))
        .is_none()
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    if child
        .try_wait()
        .unwrap_or_else(|error| panic!("inspect fixture daemon readiness: {error}"))
        .is_some()
    {
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        panic!("fixture daemon exited before its startup deadline");
    }
    match term_handler {
        "exit 0" => {}
        "" => {
            let raw_pid = i32::try_from(child.id())
                .unwrap_or_else(|_| panic!("fixture daemon PID is outside pidfd range"));
            let pid = Pid::from_raw(raw_pid)
                .unwrap_or_else(|| panic!("fixture daemon PID cannot bind a pidfd"));
            let pidfd = pidfd_open(pid, PidfdFlags::empty())
                .unwrap_or_else(|error| panic!("open fixture daemon pidfd: {error}"));
            pidfd_send_signal(&pidfd, Signal::STOP)
                .unwrap_or_else(|error| panic!("pause exact fixture daemon with pidfd: {error}"));
            let stopped_deadline = Instant::now() + Duration::from_millis(250);
            while !fixture_process_is_stopped(child.id()) && Instant::now() < stopped_deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if !fixture_process_is_stopped(child.id()) {
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
                panic!("fixture daemon did not enter its pidfd-bound stopped state");
            }
        }
        handler => panic!("unsupported fixture TERM handler {handler:?}"),
    }
    FixtureDaemon(child, stdin)
}

pub(super) fn write_daemon_identity_status(paths: &RuntimePaths, pid: u32, instance_id: &str) {
    let written_at_unix_ms = unix_time_ms().unwrap_or_default();
    // Pin the freshness clock to this write so the later identity check
    // measures the fixture's age against the timestamp it published, not
    // against how long the test thread happened to be descheduled.
    TEST_UNIX_TIME_MS.with(|clock| clock.set(Some(written_at_unix_ms)));
    write_daemon_identity_status_at(paths, pid, instance_id, written_at_unix_ms);
}

pub(super) fn write_daemon_identity_status_at(
    paths: &RuntimePaths,
    pid: u32,
    instance_id: &str,
    written_at_unix_ms: u64,
) {
    fs::write(
            &paths.cache_status_file,
            format!(
                r#"{{"schema_version":1,"daemon_instance_id":"{instance_id}","pid":{pid},"written_at_unix_ms":{written_at_unix_ms},"ok":true,"origin_state":"READY","cache_state":"ACTIVE","logical_capacity_kib":1024,"vram_cached_kib":0,"gpu_headroom_kib":null,"ssd_origin_written_kib":1024,"cache_fallback_reads":0,"cache_invalidations":0,"cache_releases":0,"cache_target_kib":0}}"#,
            ),
        )
        .unwrap_or_else(|error| panic!("write cache identity status: {error}"));
}

pub(super) fn seal_runtime_lifecycle(
    paths: &RuntimePaths,
    daemon_pid: u32,
    include_zram: bool,
) -> Vec<BoundDeviceIdentity> {
    fs::create_dir_all(&paths.runtime_dir).expect("create lifecycle fixture runtime");
    fs::write(&paths.socket, "fixture export socket").expect("write export socket fixture");
    fs::write(&paths.pid_file, daemon_pid.to_string()).expect("write daemon PID fixture");
    let daemon_instance_id =
        daemon_instance_id_from_pid(daemon_pid).expect("derive lifecycle fixture daemon identity");
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    write_nbd_path_record(&paths.swap_dev_file, "/dev/nbd0").expect("write NBD path record");

    let mut devices = vec![nbd];
    if include_zram {
        let zram = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
        write_bound_device_record(&paths.zram_dev_file, &zram).expect("write zram identity record");
        devices.push(zram);
    } else {
        remove_runtime_file(&paths.zram_dev_file);
    }
    devices.sort();

    let mut binding = lifecycle_binding_fixture(devices.clone());
    let start_ticks = daemon_instance_id
        .rsplit_once('-')
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .expect("parse lifecycle fixture daemon start time");
    binding.daemon_pid = daemon_pid;
    binding.daemon_start_ticks = start_ticks;
    binding.daemon_instance_id = daemon_instance_id;
    binding.export_socket = socket_identity(&paths.socket).expect("capture export socket fixture");
    write_lifecycle_binding(paths, &binding).expect("seal lifecycle fixture");
    // Production refreshes this status continuously. Publish the one-shot
    // fixture only after all potentially slow durable setup so a loaded
    // coverage run cannot manufacture stale evidence before validation.
    write_daemon_identity_status(paths, daemon_pid, &binding.daemon_instance_id);
    devices
}

pub(super) fn prepare_connect_identity(paths: &RuntimePaths, daemon_pid: u32) {
    fs::create_dir_all(&paths.runtime_dir).expect("create connect fixture runtime");
    fs::write(&paths.socket, "fixture export socket").expect("write connect socket fixture");
    let instance_id =
        daemon_instance_id_from_pid(daemon_pid).expect("derive connect fixture daemon identity");
    write_daemon_identity_status(paths, daemon_pid, &instance_id);
}

pub(super) fn set_swap_snapshots(
    snapshots: impl IntoIterator<Item = Result<&'static str, &'static str>>,
) {
    let snapshots = snapshots
        .into_iter()
        .map(|snapshot| snapshot.map(str::to_string).map_err(str::to_string))
        .collect::<VecDeque<_>>();
    if let Some(Ok(last)) = snapshots.back() {
        TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(last.clone()));
    }
    TEST_SWAPS_SEQUENCE.with(|queue| {
        *queue.borrow_mut() = snapshots;
    });
}

pub(super) fn set_live_device_snapshots(
    snapshots: impl IntoIterator<Item = Result<Vec<BoundDeviceIdentity>, &'static str>>,
) {
    TEST_LIVE_MANAGED_DEVICES.with(|queue| {
        *queue.borrow_mut() = snapshots
            .into_iter()
            .map(|snapshot| snapshot.map_err(str::to_string))
            .collect();
    });
}

pub(super) fn install_two_tier_down_snapshots(
    devices: &[BoundDeviceIdentity],
    active_nbd: bool,
    active_zram: bool,
) {
    const EMPTY: &str = "Filename Type Size Used Priority\n";
    const NBD: &str = "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n";
    const ZRAM: &str = "Filename Type Size Used Priority\n/dev/zram0 partition 1024 0 200\n";
    const BOTH: &str = "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n/dev/zram0 partition 1024 0 200\n";

    let initial = match (active_nbd, active_zram) {
        (true, true) => BOTH,
        (true, false) => NBD,
        (false, true) => ZRAM,
        (false, false) => EMPTY,
    };
    let mut swaps = vec![Ok(initial)];
    if active_nbd {
        swaps.push(Ok(initial));
        swaps.push(Ok(if active_zram { ZRAM } else { EMPTY }));
    }
    if active_zram {
        swaps.push(Ok(ZRAM));
        swaps.push(Ok(EMPTY));
    }
    swaps.extend(std::iter::repeat_n(Ok(EMPTY), 8));
    set_swap_snapshots(swaps);

    let all = devices.to_vec();
    let nbd = devices
        .iter()
        .filter(|device| device.kind == ManagedDeviceKind::Nbd)
        .cloned()
        .collect::<Vec<_>>();
    let mut live = vec![Ok(all.clone())];
    if active_nbd {
        live.push(Ok(all.clone()));
    }
    if active_zram {
        live.push(Ok(all.clone()));
    }
    live.extend([
        Ok(all),
        Ok(nbd.clone()),
        Ok(nbd),
        Ok(Vec::new()),
        Ok(Vec::new()),
    ]);
    set_live_device_snapshots(live);
}

pub(super) struct ScriptedRunner {
    pub(super) responses: RefCell<VecDeque<(String, Result<String, CascadeError>)>>,
    pub(super) calls: RefCell<Vec<String>>,
    pub(super) bounded_calls: RefCell<Vec<(String, Duration)>>,
}

impl ScriptedRunner {
    pub(in crate::cascade::cascade_io) fn new(
        responses: Vec<(String, Result<String, CascadeError>)>,
    ) -> Self {
        Self {
            responses: RefCell::new(responses.into()),
            calls: RefCell::new(Vec::new()),
            bounded_calls: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl CommandRunner for ScriptedRunner {
    fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
        let label = command_label(command, args);
        self.calls.borrow_mut().push(label.clone());
        let Some((expected, response)) = self.responses.borrow_mut().pop_front() else {
            return Err(CascadeError::Shell {
                cmd: label,
                msg: "unexpected test command".into(),
            });
        };
        assert_eq!(expected, label, "test command order");
        response
    }

    fn run_bounded(
        &self,
        command: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<String, CascadeError> {
        self.bounded_calls
            .borrow_mut()
            .push((command_label(command, args), timeout));
        self.run(command, args)
    }
}

pub(super) struct ParentSeams {
    _platform: super::swapoff_headroom::NativePlatformFixture,
}

impl ParentSeams {
    pub(in crate::cascade::cascade_io) fn install(swaps: &str, shell_responses: usize) -> Self {
        TEST_SWAPS.with(|cell| *cell.borrow_mut() = Some(swaps.to_string()));
        TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().clear());
        TEST_SWAPS_ERROR.with(|cell| *cell.borrow_mut() = None);
        TEST_LIVE_MANAGED_DEVICES.with(|queue| queue.borrow_mut().clear());
        TEST_NBD_DETACH_SETTLE_TIMEOUT.with(|cell| cell.set(None));
        SH_SCRIPT.with(|queue| {
            let mut queue = queue.borrow_mut();
            queue.clear();
            for _ in 0..shell_responses {
                queue.push_back(("*".into(), Ok(String::new())));
            }
        });
        Self {
            _platform: super::swapoff_headroom::NativePlatformFixture::new(),
        }
    }
}

impl Drop for ParentSeams {
    fn drop(&mut self) {
        TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
        TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().clear());
        TEST_SWAPS_ERROR.with(|cell| *cell.borrow_mut() = None);
        TEST_MEM_AVAILABLE.with(|cell| *cell.borrow_mut() = None);
        SH_SCRIPT.with(|queue| queue.borrow_mut().clear());
        TEST_LIVE_MANAGED_DEVICES.with(|queue| queue.borrow_mut().clear());
        TEST_NBD_DETACH_SETTLE_TIMEOUT.with(|cell| cell.set(None));
        TEST_UNIX_TIME_MS.with(|clock| clock.set(None));
    }
}

pub(super) fn set_nbd_detach_settle_timeout(timeout: Duration) {
    TEST_NBD_DETACH_SETTLE_TIMEOUT.with(|cell| cell.set(Some(timeout)));
}

pub(super) fn run_timeout_teardown_fixture_with_held_predecessor() -> Result<(), String> {
    let fixture = TestDir::new();
    let _held_predecessor = fixture.hold_writable_program("ramsharedd", "#!/bin/sh\nexit 99\n");
    let daemon = spawn_fixture_daemon(&fixture, "");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .map_err(|error| format!("create isolated runtime: {error}"))?;
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    install_two_tier_down_snapshots(&devices, false, false);
    let runner = ScriptedRunner::new(vec![
        ("zramctl -r /dev/zram0".into(), Ok(String::new())),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
    ]);

    let error = down_with_runtime(&runner, &paths)
        .err()
        .ok_or_else(|| "timeout fixture unexpectedly reported cleanup success".to_string())?;
    if !error.to_string().contains("did not exit") {
        return Err(format!("timeout fixture returned the wrong error: {error}"));
    }
    if !paths.pid_file.exists() || !paths.cache_status_file.exists() {
        return Err("timeout fixture removed forensic runtime evidence".to_string());
    }
    let expected_calls = vec![
        "zramctl -r /dev/zram0".to_string(),
        "nbd-client -d /dev/nbd0".to_string(),
    ];
    if runner.calls() != expected_calls {
        return Err("timeout fixture did not retain the swapoff-first teardown order".to_string());
    }
    Ok(())
}

/// A child the owning test holds for the whole of its body.
///
/// The previous fixture slept for ten seconds and returned a bare
/// `Child`. Under full-suite load the body outran that deadline, so the
/// lifecycle binding checked a pid that had already exited and reported
/// "daemon cache status differs" as if the production freshness rule had
/// failed. The lifetime is now far longer than any body, and
/// [`FixtureDaemon`]'s drop kills and reaps the child so a long lifetime
/// never leaks a process.
pub(super) fn controlled_child() -> FixtureDaemon {
    let mut command = Command::new("/bin/sleep");
    command.arg("3600");
    let child = bounded_process::configure_process_group(&mut command)
        .spawn()
        .unwrap_or_else(|error| panic!("spawn controlled child: {error}"));
    FixtureDaemon(child, None)
}

pub(super) struct UnprovenChild {
    pub(super) termination_requests: usize,
}

impl SpawnedChildContainment for UnprovenChild {
    fn terminate_group_and_reap(&mut self) -> Result<(), CascadeError> {
        self.termination_requests += 1;
        Err(CascadeError::UnsafeContainment(
            "fixture could not prove direct-child reap".into(),
        ))
    }
}

pub(super) struct RecordingNbdLifecycleExecutor {
    pub(super) calls: RefCell<Vec<String>>,
    pub(super) fail_swapoff: bool,
}

impl RecordingNbdLifecycleExecutor {
    fn new(fail_swapoff: bool) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            fail_swapoff,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl NbdLifecycleExecutor for RecordingNbdLifecycleExecutor {
    fn swapoff(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        self.calls
            .borrow_mut()
            .push(format!("swapoff {}", device.path));
        if self.fail_swapoff {
            return Err(CascadeError::Shell {
                cmd: format!("swapoff {}", device.path),
                msg: "manufactured swapoff refusal".into(),
            });
        }
        Ok(())
    }

    fn reset_zram(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        self.calls
            .borrow_mut()
            .push(format!("reset-zram {}", device.path));
        Ok(())
    }

    fn disconnect_nbd(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
        self.calls
            .borrow_mut()
            .push(format!("disconnect-nbd {}", device.path));
        Ok(())
    }

    fn stop_daemon(&self) -> Result<(), CascadeError> {
        self.calls.borrow_mut().push("stop-daemon".into());
        Ok(())
    }
}

pub(super) fn write_test_host_manifest(dir: &TestDir) -> (PathBuf, String) {
    let manifest = dir.path.join("origin-manifest.json");
    let contents = br#"{"origin_vhdx":"C:\\ProgramData\\RamShared\\ramshared-origin.vhdx","partuuid":"11111111-2222-4333-8444-555555555555"}"#;
    fs::write(&manifest, contents).expect("write manifest");
    let hash = Sha256::digest(contents)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (manifest, hash)
}

/// Test stand-in for the sealed launcher plus a Windows elevation
/// boundary. Not real host paths: the mocks never exec them, they only
/// record the argv vector.
pub(super) fn fake_elevation() -> Result<HostBoundary, CascadeError> {
    Ok(HostBoundary::new(
        WINDOWS_HOST_LAUNCHER,
        "/fake/windows/sudo.exe".to_string(),
    ))
}

pub(super) fn no_elevation() -> Result<HostBoundary, CascadeError> {
    panic!("this path must not resolve a host launch boundary");
}

pub(super) fn missing_elevation() -> Result<HostBoundary, CascadeError> {
    Err(CascadeError::Precondition(
        "no Windows elevation boundary".into(),
    ))
}

/// Write an admissible `guardian-config.json` for `ensure_sealed_guardian_running_from`.
pub(super) fn write_guardian_config(dir: &TestDir) -> PathBuf {
    let path = dir.path.join("guardian-config.json");
    fs::write(
            &path,
            format!(
                r#"{{"schema_version":2,"distro":"Fixture-Ubuntu","user_sid":"S-1-5-21-1-2-3-1001","task_name":"{task}","guardian_policy":{{}}}}"#,
                task = super::super::sealed_identity::GUARDIAN_TASK_NAME,
            ),
        )
        .expect("write guardian config fixture");
    path
}

#[path = "tests_suite/zram_setup_contracts.rs"]
mod zram_setup_contracts;
pub(super) use zram_setup_contracts::{
    zram_failure_predicates_read_the_kernel_wording,
    zram_setup_accepts_backing_dev_before_disksize,
    zram_setup_is_idempotent_when_the_tier_is_already_correct,
    zram_setup_preserves_the_device_when_the_abandoning_reset_fails,
    zram_setup_preserves_the_device_when_the_abandoning_reset_leaves_it,
    zram_setup_refuses_disksize_before_backing_dev,
    zram_setup_refuses_to_double_a_live_tier_with_a_different_binding,
    zram_setup_refuses_to_format_a_device_it_could_not_record,
    zram_setup_refuses_when_the_size_cannot_be_set,
    zram_setup_rolls_back_when_the_device_identity_diverges,
    zram_tier_created_by_the_product_survives_losing_its_backing,
};
#[path = "tests_suite/nbd_attach_and_isolation.rs"]
mod nbd_attach_and_isolation;
pub(super) use nbd_attach_and_isolation::{
    detached_nbd_evidence_requires_exact_idle_unheld_device_identity,
    sysfs_device_discovery_filters_live_devices_and_rejects_malformed_state,
    zram_setup_accepts_a_named_backing_device, zram_setup_refuses_a_missing_backing_device,
    zram_setup_refuses_when_allocation_cardinality_cannot_be_proven,
};
#[path = "tests_suite/device_identity_and_discovery.rs"]
mod device_identity_and_discovery;
pub(super) use device_identity_and_discovery::{
    bound_device_observation_enforces_sysfs_and_nbd_owner_policy,
    bounded_command_captures_stdout_and_rejects_nonzero,
    bounded_command_contains_descendant_that_inherits_output,
    bounded_command_times_out_and_reaps_its_direct_child,
    connect_nbd_preserves_primary_error_and_rolls_back_once,
    failed_readiness_terminates_only_spawned_child,
    production_effect_binding_refuses_non_block_files_after_exact_revalidation,
    transport_refusal_is_fail_closed_before_command, zram_output_requires_exact_block_identity,
};
#[path = "tests_suite/command_and_zram_fallback.rs"]
mod command_and_zram_fallback;
pub(super) use command_and_zram_fallback::{
    connect_nbd_records_exact_preprovisioned_swap_before_activation,
    connect_nbd_refusal_terminates_exact_daemon_without_detach,
    connect_nbd_refuses_invalid_target_and_zero_connections_before_runner,
    connect_nbd_uncertain_swapon_preserves_backend_and_daemon,
    zram_fallback_refuses_unexpected_device_without_swapon,
};
#[path = "tests_suite/malformed_zram_recovery.rs"]
mod malformed_zram_recovery;
pub(super) use malformed_zram_recovery::{
    malformed_zram_allocation_refuses_ambiguous_delta_without_reset,
    malformed_zram_allocation_refuses_unreadable_post_state_without_reset,
    malformed_zram_final_snapshot_failure_preserves_sealed_ownership_evidence,
    malformed_zram_reset_failure_preserves_sealed_ownership_evidence,
    malformed_zram_success_resets_exact_new_device_without_leak,
    zram_format_failure_rolls_back_only_the_new_exact_device,
    zram_zero_capacity_skips_commands_and_successful_setup_seals_exact_device,
};
#[path = "tests_suite/cascade_setup_rollback.rs"]
mod cascade_setup_rollback;
pub(super) use cascade_setup_rollback::{
    runtime_marker_and_pid_record_refuse_unsafe_identity,
    setup_new_cascade_keeps_zram_record_on_swapoff_refusal,
    setup_new_cascade_rolls_back_zram_after_nbd_failure,
    setup_new_cascade_uses_only_temp_runtime_and_direct_child_fixture,
    zram_setup_never_mutates_unbound_sysfs_fallback,
};
#[path = "tests_suite/nbd_teardown_and_clean_state.rs"]
mod nbd_teardown_and_clean_state;
#[path = "tests_suite/teardown_and_swapoff.rs"]
mod teardown_and_swapoff;
pub(super) use nbd_teardown_and_clean_state::down_with_runtime_preserves_swapoff_first_and_cleans_temp_state;
#[path = "tests_suite/daemon_pid_and_proc_adapters.rs"]
mod daemon_pid_and_proc_adapters;
pub(super) use daemon_pid_and_proc_adapters::{
    daemon_pid_requires_positive_pid_and_exact_identity,
    proc_identity_and_socket_metadata_adapters_share_fail_closed_validation,
    system_command_runner_uses_bounded_capture_for_success_and_spawn_refusal,
};
#[path = "tests_suite/host_origin_and_manifests.rs"]
mod host_origin_and_manifests;
pub(super) use host_origin_and_manifests::{
    ensure_origin_attached_rejects_unreadable_and_noncanonical_manifests_before_host_call,
    up_with_args_refuses_ublk_before_swap_or_device_access,
    up_with_config_refuses_missing_safety_net_before_runtime_setup,
};
#[path = "tests_suite/additional_io_contracts.rs"]
mod additional_io_contracts;
#[path = "tests_suite/legacy_migration_and_guards.rs"]
mod legacy_migration_and_guards;
#[path = "tests_suite/sealed_guardian_lifecycle.rs"]
mod sealed_guardian_lifecycle;
