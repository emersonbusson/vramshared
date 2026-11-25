// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/origin_startup.rs
//! Startup rollback with actual origin/child custody; no kernel storage effects.
use super::*;
use std::os::unix::fs::MetadataExt;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

const CHILD: &str = "RAMSHARED_ORIGIN_STARTUP_TEST_CHILD";
const CASE: &str =
    "tests::origin_startup::origin_startup_errors_retain_origin_until_same_child_exits";

#[derive(Clone, Copy, Debug)]
enum Failure {
    Acceptor,
    Bridge,
    Drop,
    Bind,
    Geometry,
    Target,
    OversizedTarget,
}
#[derive(Default)]
struct Effects {
    workers: AtomicUsize,
    acceptors: AtomicUsize,
    bridges: AtomicUsize,
}
struct Starter {
    failure: Failure,
    effects: Arc<Effects>,
    initial: Option<ChildWorkerProcess>,
    boundary: Option<SyncSender<()>>,
}
impl Starter {
    fn boundary(&mut self) {
        self.boundary.take().unwrap().send(()).unwrap();
    }
}
impl NbdRuntimeStarter for Starter {
    fn lock_memory(&mut self, _: bool, future: bool) -> Result<(), Box<dyn std::error::Error>> {
        assert!(!future);
        Ok(())
    }
    fn start_acceptor(
        &mut self,
        _: UnixListener,
        _: Arc<Vec<ramshared_block::handshake::Export>>,
        _: u16,
        _: SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.effects.acceptors.fetch_add(1, Ordering::SeqCst);
        if matches!(self.failure, Failure::Acceptor) {
            self.boundary();
            Err("injected acceptor failure".into())
        } else {
            Ok(())
        }
    }
    fn start_shutdown_bridge(
        &mut self,
        _: SyncSender<WMsg>,
    ) -> Result<Option<NbdShutdownBridge>, Box<dyn std::error::Error>> {
        self.effects.bridges.fetch_add(1, Ordering::SeqCst);
        if matches!(self.failure, Failure::Bridge) {
            assert!(self.boundary.is_none());
            Err("injected bridge failure".into())
        } else {
            Ok(None)
        }
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        panic!("startup must not inspect swap")
    }
    fn nbd_swap_is_explicitly_absent(&mut self, _: &str) -> bool {
        panic!("startup must not invent absence")
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        panic!("startup must not claim a serving shutdown permit")
    }
    fn publish_demote(&mut self, _: &DemoteStatusFile) {
        panic!("startup must not publish demotion")
    }
    fn elapsed_us(&mut self, _: Instant) -> u64 {
        panic!("startup must not measure demotion")
    }
    fn spawn_swapoff(&mut self, _: &str) -> Receiver<SwapoffOutcome> {
        panic!("startup must not drain")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("startup must not activate")
    }
    fn startup_budget(
        &mut self,
        _: bool,
    ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
        panic!("origin must not query sparse budget")
    }
    fn origin_cache_target_bytes(&mut self, _: u64) -> Result<u64, Box<dyn std::error::Error>> {
        Ok(match self.failure {
            Failure::Target => 0,
            Failure::OversizedTarget => 12288,
            _ => 4096,
        })
    }
    fn spawn_isolated_gpu_worker(
        &mut self,
        _: &WorkerSpawnParams,
    ) -> worker_startup::InitialStartResult {
        assert_eq!(self.effects.workers.fetch_add(1, Ordering::SeqCst), 0);
        Err(worker_startup::InitialStartFailure {
            message: "controlled initial failure".into(),
            retained_child: self.initial.take().map(Box::new),
        })
    }
}

fn opened(file: File) -> OpenedOrigin {
    OpenedOrigin::Wsl(
        FileOrigin::from_file(file),
        SealedOrigin {
            logical_capacity_mib: 1,
            partuuid: "11111111-2222-4333-8444-555555555555".into(),
            origin_vhdx: "test-authority".into(),
        },
    )
}
fn origin_file(path: &Path) -> File {
    use std::io::Write;
    let mut file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(&[53; 8192]).unwrap();
    file.sync_all().unwrap();
    file
}
fn same_descriptor(fd: i32, identity: &std::fs::Metadata) -> bool {
    std::fs::metadata(format!("/proc/self/fd/{fd}"))
        .is_ok_and(|m| m.ino() == identity.ino() && m.dev() == identity.dev())
}
fn run_startup(file: File, socket: &Path, mut starter: Starter) -> Result<(), String> {
    let size = if matches!(starter.failure, Failure::Geometry) {
        0
    } else {
        8192
    };
    if matches!(starter.failure, Failure::Drop) {
        let runtime = origin_runtime::build(opened(file), size, 2048 * 1024 * 1024, &mut starter)
            .map_err(|e| e.to_string())?;
        starter.boundary();
        drop(runtime);
        return Ok(());
    }
    run_origin_nbd_with_startup(
        UnavailableVramProvider,
        Some(opened(file)),
        size,
        socket.to_str().unwrap().into(),
        false,
        "/dev/ramshared-origin-startup-test".into(),
        2,
        &mut starter,
    )
    .map_err(|e| e.to_string())
}

fn journey(root: &Path, failure: Failure) {
    let directory = root.join(format!("{failure:?}"));
    std::fs::create_dir(&directory).unwrap();
    let origin = directory.join("origin.img");
    let file = origin_file(&origin);
    let fd = file.as_raw_fd();
    let identity = file.metadata().unwrap();
    let socket = directory.join("origin.sock");
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let custody = Arc::new(worker_custody::WorkerCustody::default());
    let resources = custody.reserve().unwrap().bind(child);
    let observation = resources.child.lock().unwrap();
    let (boundary_tx, boundary_rx) = sync_channel(1);
    let (done_tx, done_rx) = sync_channel(1);
    let effects = Arc::new(Effects::default());
    let process = ChildWorkerProcess::from_resources(resources.clone());
    let starter = Starter {
        failure,
        effects: Arc::clone(&effects),
        initial: Some(process),
        boundary: Some(boundary_tx),
    };
    let thread_socket = socket.clone();
    let daemon = std::thread::spawn(move || {
        let _ = done_tx.send(run_startup(file, &thread_socket, starter));
    });
    boundary_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let early = done_rx.recv_timeout(Duration::from_millis(200));
    let pending = matches!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
    let retained = same_descriptor(fd, &identity);
    let socket_retained = matches!(failure, Failure::Drop) || socket.exists();
    drop(observation);
    drop(input);
    let result = early.unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(6)).unwrap());
    daemon.join().unwrap();
    let mut process = ChildWorkerProcess::from_resources(resources.clone());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !(process.stop_is_quiescent() && process.try_wait().unwrap()) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(effects.workers.load(Ordering::SeqCst), 1);
    let expected_endpoints = match failure {
        Failure::Acceptor => (1, 1),
        Failure::Drop => (0, 0),
        _ => unreachable!(),
    };
    assert_eq!(
        (
            effects.acceptors.load(Ordering::SeqCst),
            effects.bridges.load(Ordering::SeqCst)
        ),
        expected_endpoints,
        "a failed bridge must never publish the acceptor"
    );
    assert_eq!(
        result,
        match failure {
            Failure::Acceptor => Err("injected acceptor failure".into()),
            Failure::Drop => Ok(()),
            _ => unreachable!(),
        }
    );
    assert!(!same_descriptor(fd, &identity) && !socket.exists());
    assert_eq!(std::fs::read(origin).unwrap(), [53; 8192]);
    assert!(custody.reserve().is_ok());
    assert!(
        pending && retained && socket_retained,
        "{failure:?} discarded startup ownership: pending={pending} retained={retained} socket={socket_retained}"
    );
}

fn private_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "ramshared-origin-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    root
}

#[test]
fn origin_startup_errors_retain_origin_until_same_child_exits() {
    if let Some(root) = std::env::var_os(CHILD) {
        let mut failed = Vec::new();
        for failure in [Failure::Acceptor, Failure::Drop] {
            if std::panic::catch_unwind(|| journey(Path::new(&root), failure)).is_err() {
                failed.push(failure);
            }
        }
        assert!(failed.is_empty(), "startup cases failed: {failed:?}");
        std::fs::write(Path::new(&root).join("complete"), b"complete").unwrap();
        return;
    }
    let root = private_root("startup");
    let log = root.join("child.log");
    let output = File::options()
        .create_new(true)
        .write(true)
        .open(&log)
        .unwrap();
    let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
        .args(["--exact", CASE, "--nocapture"])
        .env(CHILD, &root)
        .process_group(0)
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let group = i32::try_from(child.id()).unwrap();
            // SAFETY: this unreaped child owns an isolated group containing
            // only this Linux fixture and its stdin-held shells. No product
            // daemon or Windows launcher belongs to this group.
            let killed = unsafe { libc::kill(-group, libc::SIGKILL) };
            assert!(
                killed == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            );
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let content = std::fs::read_to_string(&log).unwrap();
    let complete = std::fs::read(root.join("complete")).ok();
    std::fs::remove_dir_all(root).unwrap();
    eprint!("{content}");
    assert!(
        status.success() && complete.as_deref() == Some(b"complete"),
        "startup custody fixture failed: {status}\n{content}"
    );
}

#[test]
fn origin_startup_preconditions_refuse_before_worker_spawn() {
    let root = private_root("refusal");
    let mut failed = Vec::new();
    for failure in [
        Failure::Bind,
        Failure::Bridge,
        Failure::Geometry,
        Failure::Target,
        Failure::OversizedTarget,
    ] {
        let directory = root.join(format!("{failure:?}"));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("origin.sock");
        let foreign =
            matches!(failure, Failure::Bind).then(|| UnixListener::bind(&socket).unwrap());
        let identity = foreign
            .as_ref()
            .map(|_| std::fs::metadata(&socket).unwrap());
        let effects = Arc::new(Effects::default());
        let result = run_startup(
            origin_file(&directory.join("origin.img")),
            &socket,
            Starter {
                failure,
                effects: Arc::clone(&effects),
                initial: None,
                boundary: None,
            },
        );
        assert!(result.is_err(), "{failure:?} unexpectedly started");
        if matches!(failure, Failure::Bridge) {
            assert_eq!(result, Err("injected bridge failure".into()));
            assert_eq!(effects.bridges.load(Ordering::SeqCst), 1);
        }
        assert_eq!(effects.acceptors.load(Ordering::SeqCst), 0);
        if let Some(identity) = identity {
            let current = std::fs::metadata(&socket).unwrap();
            assert_eq!(
                (current.dev(), current.ino()),
                (identity.dev(), identity.ino())
            );
        }
        if effects.workers.load(Ordering::SeqCst) != 0 {
            failed.push(failure);
        }
        drop(foreign);
    }
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        failed.is_empty(),
        "worker started before startup refusal: {failed:?}"
    );
}
