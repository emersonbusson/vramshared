// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/origin_boot.rs
//! Real startup signals and notification datagrams; injected storage ingress.
use super::*;
use ramshared_wsl2d::systemd_notify::{NotifyError, NotifySocket};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixDatagram;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

const CHILD: &str = "RAMSHARED_ORIGIN_BOOT_TEST_CHILD";
#[derive(Clone, Copy, PartialEq)]
enum Case {
    Signal,
    Interrupt,
    AcceptedCancellation,
    Ready,
    Error,
}
impl Case {
    fn name(self) -> &'static str {
        match self {
            Self::Signal => {
                "tests::origin_boot::origin_startup_handles_real_sigterm_before_worker_return"
            }
            Self::Interrupt => {
                "tests::origin_boot::origin_startup_handles_real_sigint_before_worker_return"
            }
            Self::AcceptedCancellation => {
                "tests::origin_boot::origin_ready_is_suppressed_after_acceptor_cancellation"
            }
            Self::Ready => "tests::origin_boot::origin_ready_follows_acceptor_and_origin_io",
            Self::Error => "tests::origin_boot::origin_startup_error_never_publishes_ready",
        }
    }
}
struct Starter {
    root: PathBuf,
    case: Case,
    fd: i32,
    identity: std::fs::Metadata,
    jobs: Option<SyncSender<SyncSender<WMsg>>>,
}
impl NbdRuntimeStarter for Starter {
    fn take_notify_socket(&mut self) -> Result<Option<NotifySocket>, NotifyError> {
        // Actual datagram; only admission is a lab adapter. The production
        // default retains from_env's root-owner check.
        NotifySocket::from_path(&self.root.join("notify.sock"), false).map(Some)
    }
    fn lock_memory(&mut self, force: bool, future: bool) -> Result<(), Box<dyn std::error::Error>> {
        assert!(!force && !future);
        Ok(())
    }
    fn start_shutdown_bridge(
        &mut self,
        jobs: SyncSender<WMsg>,
    ) -> Result<Option<NbdShutdownBridge>, Box<dyn std::error::Error>> {
        ProductionNbdRuntimeStarter.start_shutdown_bridge(jobs)
    }
    fn start_acceptor(
        &mut self,
        _: UnixListener,
        exports: Arc<Vec<ramshared_block::handshake::Export>>,
        _: u16,
        jobs: SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert!(
            !matches!(self.case, Case::Signal | Case::Interrupt),
            "cancelled startup published an acceptor"
        );
        assert_eq!(exports[0].size, 8192);
        std::fs::write(self.root.join("acceptor"), b"prepared")?;
        if self.case == Case::AcceptedCancellation {
            wait_file(&self.root.join("acceptor_release"));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !crate::shutdown_signals::SHUTDOWN.load(Ordering::SeqCst) {
                assert!(
                    Instant::now() < deadline,
                    "shutdown signal was not observed"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if self.case == Case::Error {
            Err("injected acceptor refusal".into())
        } else {
            self.jobs.take().unwrap().send(jobs)?;
            Ok(())
        }
    }
    fn origin_cache_target_bytes(&mut self, _: u64) -> Result<u64, Box<dyn std::error::Error>> {
        Ok(4096)
    }
    fn spawn_isolated_gpu_worker(
        &mut self,
        _: &WorkerSpawnParams,
    ) -> worker_startup::InitialStartResult {
        assert!(same_file(self.fd, &self.identity));
        std::fs::write(self.root.join("worker"), b"pending").unwrap();
        wait_file(&self.root.join("release"));
        assert!(same_file(self.fd, &self.identity));
        Err(worker_startup::InitialStartFailure {
            message: "controlled constructor result".into(),
            retained_child: None,
        })
    }
    fn begin_isolated_worker_start(
        &mut self,
        _: &WorkerSpawnParams,
    ) -> std::io::Result<worker_startup::Startup> {
        assert!(matches!(
            self.case,
            Case::Ready | Case::AcceptedCancellation
        ));
        Err(std::io::ErrorKind::WouldBlock.into())
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        assert!(matches!(
            self.case,
            Case::Ready | Case::AcceptedCancellation
        ));
        0
    }
    fn nbd_swap_is_explicitly_absent(&mut self, _: &str) -> bool {
        assert!(matches!(
            self.case,
            Case::Ready | Case::AcceptedCancellation
        ));
        true
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        assert!(matches!(
            self.case,
            Case::Ready | Case::AcceptedCancellation
        ));
        std::fs::write(self.root.join("permit"), b"claimed").unwrap();
        Some(ShutdownPermit::injected(()))
    }
    fn publish_demote(&mut self, _: &DemoteStatusFile) {
        assert!(matches!(
            self.case,
            Case::Ready | Case::AcceptedCancellation
        ));
    }
    fn elapsed_us(&mut self, _: Instant) -> u64 {
        assert!(matches!(
            self.case,
            Case::Ready | Case::AcceptedCancellation
        ));
        0
    }
    fn spawn_swapoff(&mut self, _: &str) -> Receiver<SwapoffOutcome> {
        panic!("test must not drain")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("test must not activate")
    }
}

fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "missing fixture event: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn same_file(fd: i32, identity: &std::fs::Metadata) -> bool {
    std::fs::metadata(format!("/proc/self/fd/{fd}"))
        .is_ok_and(|m| m.dev() == identity.dev() && m.ino() == identity.ino())
}
fn request(jobs: &SyncSender<WMsg>, cmd: Command, payload: Vec<u8>) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    jobs.send(WMsg::Job(ramshared_wsl2d::conn::Job {
        export: 0,
        req: ramshared_block::Request {
            flags: if cmd == Command::Write {
                ramshared_block::protocol::NBD_CMD_FLAG_FUA
            } else {
                0
            },
            cmd,
            handle: 1,
            offset: 0,
            len: if cmd == Command::Flush { 0 } else { 4096 },
        },
        payload,
        credit: fixture_credit(),
        reply: tx,
    }))
    .unwrap();
    let reply = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(&reply.reply[4..8], &[0; 4]);
    reply.data
}
fn exercise_io(root: PathBuf, jobs: Receiver<SyncSender<WMsg>>) {
    let jobs = jobs.recv_timeout(Duration::from_secs(5)).unwrap();
    wait_file(&root.join("ready_ack"));
    request(&jobs, Command::Write, vec![0x49; 4096]);
    request(&jobs, Command::Flush, vec![]);
    assert_eq!(request(&jobs, Command::Read, vec![]), vec![0x49; 4096]);
    jobs.send(WMsg::Shutdown).unwrap();
}
fn child(root: &Path, case: Case) {
    use std::io::Write;
    let mut file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(root.join("origin.img"))
        .unwrap();
    file.write_all(&[53; 8192]).unwrap();
    file.sync_all().unwrap();
    let fd = file.as_raw_fd();
    let identity = file.metadata().unwrap();
    let (jobs_tx, jobs_rx) = sync_channel(1);
    let controller = if case == Case::Ready {
        let directory = root.to_path_buf();
        Some(std::thread::spawn(move || exercise_io(directory, jobs_rx)))
    } else {
        None
    };
    let mut starter = Starter {
        root: root.to_path_buf(),
        case,
        fd,
        identity: identity.clone(),
        jobs: Some(jobs_tx),
    };
    let result = run_origin_nbd_with_startup(
        UnavailableVramProvider,
        Some(OpenedOrigin::Wsl(
            FileOrigin::from_file(file),
            SealedOrigin {
                logical_capacity_mib: 1,
                partuuid: "11111111-2222-4333-8444-555555555555".into(),
                origin_vhdx: "test-authority".into(),
            },
        )),
        8192,
        root.join("origin.sock").to_str().unwrap().into(),
        false,
        "/dev/ramshared-origin-boot-test".into(),
        2,
        &mut starter,
    )
    .map_err(|e| e.to_string());
    if let Some(controller) = controller {
        controller.join().unwrap();
    }
    assert_eq!(
        result,
        if case == Case::Error {
            Err("injected acceptor refusal".into())
        } else {
            Ok(())
        }
    );
    assert!(!same_file(fd, &identity) && !root.join("origin.sock").exists());
    assert_eq!(
        root.join("permit").exists(),
        matches!(case, Case::Ready | Case::AcceptedCancellation)
    );
    std::fs::write(root.join("complete"), b"complete").unwrap();
}

fn observe_pending(
    child: &mut std::process::Child,
    root: &Path,
    receiver: &UnixDatagram,
    case: Case,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("worker").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "fixture exited before constructor boundary"
        );
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut buffer = [0; 64];
    assert_eq!(
        receiver.recv(&mut buffer).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    if matches!(case, Case::Signal | Case::Interrupt) {
        let pid = i32::try_from(child.id()).unwrap();
        // SAFETY: this owned unreaped child is the isolated Linux test;
        // it launches no subprocess, GPU, storage device or Windows helper.
        let signal = if case == Case::Signal {
            libc::SIGTERM
        } else {
            libc::SIGINT
        };
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            child.try_wait().unwrap().is_none(),
            "signal terminated pending startup"
        );
    }
}
fn await_ready(socket: &UnixDatagram, child: &mut std::process::Child, root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut data = [0; 64];
    loop {
        match socket.recv(&mut data) {
            Ok(n) => {
                assert_eq!(&data[..n], b"READY=1");
                assert!(root.join("acceptor").exists());
                std::fs::write(root.join("ready_ack"), b"observed").unwrap();
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("{e}"),
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "fixture exited before READY"
        );
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn await_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            return child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn parent(case: Case) {
    let root = std::env::temp_dir().join(format!(
        "ramshared-boot-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let receiver = UnixDatagram::bind(root.join("notify.sock")).unwrap();
    receiver.set_nonblocking(true).unwrap();
    let log = root.join("child.log");
    let output = File::options()
        .write(true)
        .create_new(true)
        .open(&log)
        .unwrap();
    let mut process = ProcessCommand::new(std::env::current_exe().unwrap())
        .args(["--exact", case.name(), "--nocapture"])
        .env(CHILD, &root)
        .env_remove("NOTIFY_SOCKET")
        .env_remove("WATCHDOG_USEC")
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap();
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        observe_pending(&mut process, &root, &receiver, case);
    }));
    std::fs::write(root.join("release"), b"settled").unwrap();
    let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if checked.is_ok() && case == Case::Ready {
            await_ready(&receiver, &mut process, &root);
        } else if checked.is_ok() && case == Case::AcceptedCancellation {
            wait_file(&root.join("acceptor"));
            let pid = i32::try_from(process.id()).unwrap();
            // SAFETY: the same owned unreaped Linux fixture, without any
            // storage device, GPU child or Windows launcher.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
            std::fs::write(root.join("acceptor_release"), b"settled").unwrap();
        }
    }));
    if (checked.is_err() || ready.is_err()) && process.try_wait().unwrap().is_none() {
        // Only this owned nonstorage Linux fixture, never a product peer,
        // GPU process or opaque native launcher.
        process.kill().unwrap();
    }
    let status = await_exit(&mut process);
    let content = std::fs::read_to_string(log).unwrap();
    eprint!("{content}");
    let complete = root.join("complete").exists();
    let accepted = root.join("acceptor").exists();
    let data = std::fs::read(root.join("origin.img")).unwrap_or_default();
    let mut notification = [0; 64];
    let no_extra_ready = receiver
        .recv(&mut notification)
        .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock);
    drop(receiver);
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        checked.is_ok() && ready.is_ok() && status.success() && complete,
        "startup fixture failed: {status}\n{content}"
    );
    assert!(no_extra_ready, "startup cancellation/error published READY");
    assert_eq!(accepted, !matches!(case, Case::Signal | Case::Interrupt));
    assert_eq!(
        &data[..4096],
        if case == Case::Ready {
            &[0x49; 4096]
        } else {
            &[53; 4096]
        }
    );
    assert_eq!(&data[4096..], &[53; 4096]);
}
fn run(case: Case) {
    if let Some(root) = std::env::var_os(CHILD) {
        child(Path::new(&root), case);
    } else {
        parent(case);
    }
}
#[test]
fn origin_startup_handles_real_sigterm_before_worker_return() {
    run(Case::Signal);
}
#[test]
fn origin_ready_follows_acceptor_and_origin_io() {
    run(Case::Ready);
}
#[test]
fn origin_startup_error_never_publishes_ready() {
    run(Case::Error);
}

#[test]
fn origin_startup_handles_real_sigint_before_worker_return() {
    run(Case::Interrupt);
}

#[test]
fn origin_ready_is_suppressed_after_acceptor_cancellation() {
    run(Case::AcceptedCancellation);
}
