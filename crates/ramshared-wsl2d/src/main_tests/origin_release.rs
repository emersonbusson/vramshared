// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/origin_release.rs
//! Product-origin IPC release outcomes across retained child shutdown.
use super::*;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

const CHILD: &str = "RAMSHARED_ORIGIN_RELEASE_TEST_CHILD";
#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Accepted,
    Refused,
    Timeout,
}
impl Outcome {
    fn test(self) -> &'static str {
        match self {
            Self::Accepted => {
                "tests::origin_release::origin_shutdown_accepts_release_only_after_worker_exit"
            }
            Self::Refused => {
                "tests::origin_release::origin_shutdown_preserves_rejected_release_until_worker_exit"
            }
            Self::Timeout => {
                "tests::origin_release::origin_shutdown_waits_for_release_timeout_and_worker_exit"
            }
        }
    }
}
struct Probe {
    fd: i32,
    identity: std::fs::Metadata,
    socket: PathBuf,
    resources: worker_custody::WorkerResources,
}
#[derive(Default)]
struct State {
    held: AtomicBool,
    drained: AtomicBool,
    drains: AtomicUsize,
    starts: AtomicUsize,
    claims: AtomicUsize,
    disables: AtomicUsize,
    probe: Option<Probe>,
}
struct Permit(Arc<State>);
impl Drop for Permit {
    fn drop(&mut self) {
        let probe = self.0.probe.as_ref().unwrap();
        assert!(!same_descriptor(probe.fd, &probe.identity) && !probe.socket.exists());
        assert!(probe.resources.exited.load(Ordering::Acquire));
        assert!(
            !probe.resources.stop_active.load(Ordering::Acquire)
                && !probe.resources.reaper_active.load(Ordering::Acquire)
        );
        self.0.held.store(false, Ordering::Release);
    }
}
struct Starter {
    state: Arc<State>,
    jobs: SyncSender<std::sync::mpsc::SyncSender<WMsg>>,
    draining: SyncSender<()>,
    claimed: Option<SyncSender<()>>,
    drain: Option<Receiver<SwapoffOutcome>>,
    initial: Option<ChildWorkerProcess>,
    outcome: Outcome,
    disabled: Option<SyncSender<()>>,
    peer_release: Option<Receiver<()>>,
    peer: SyncSender<std::thread::JoinHandle<()>>,
}
impl NbdRuntimeStarter for Starter {
    fn lock_memory(&mut self, force: bool, future: bool) -> Result<(), Box<dyn std::error::Error>> {
        assert!(!force && !future);
        Ok(())
    }
    fn start_acceptor(
        &mut self,
        _: UnixListener,
        exports: Arc<Vec<ramshared_block::handshake::Export>>,
        _: u16,
        jobs: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(exports[0].size, 8192);
        self.jobs.send(jobs)?;
        Ok(())
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        if self.state.drained.load(Ordering::Acquire) {
            0
        } else {
            8
        }
    }
    fn nbd_swap_is_explicitly_absent(&mut self, _: &str) -> bool {
        false
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        assert!(self.state.drained.load(Ordering::Acquire));
        assert!(!self.state.held.swap(true, Ordering::AcqRel));
        assert_eq!(self.state.claims.fetch_add(1, Ordering::AcqRel), 0);
        self.claimed.take().unwrap().send(()).unwrap();
        Some(ShutdownPermit::injected(Permit(Arc::clone(&self.state))))
    }
    fn publish_demote(&mut self, status: &DemoteStatusFile) {
        if status.total > 0 && !status.in_progress {
            self.state.drained.store(true, Ordering::Release);
        }
    }
    fn elapsed_us(&mut self, _: Instant) -> u64 {
        0
    }
    fn spawn_swapoff(&mut self, _: &str) -> Receiver<SwapoffOutcome> {
        assert_eq!(self.state.drains.fetch_add(1, Ordering::AcqRel), 0);
        self.draining.send(()).unwrap();
        self.drain.take().unwrap()
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("shutdown never activates swap")
    }
    fn startup_budget(
        &mut self,
        _: bool,
    ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
        panic!("origin must not query sparse GPU budget")
    }
    fn origin_cache_target_bytes(&mut self, _: u64) -> Result<u64, Box<dyn std::error::Error>> {
        Ok(4096)
    }
    fn spawn_isolated_gpu_worker(
        &mut self,
        _: &WorkerSpawnParams,
    ) -> worker_startup::InitialStartResult {
        assert_eq!(self.state.starts.fetch_add(1, Ordering::AcqRel), 0);
        let (socket, peer) = UnixStream::pair().unwrap();
        let state = Arc::clone(&self.state);
        let outcome = self.outcome;
        let disabled = self.disabled.take().unwrap();
        let release = self.peer_release.take().unwrap();
        self.peer
            .send(std::thread::spawn(move || {
                run_peer(peer, state, outcome, disabled, release)
            }))
            .unwrap();
        let mut client = IpcCacheClient::new(socket, Duration::from_millis(50), 4096);
        let process = self.initial.take().unwrap();
        if let Err(message) = client.perform_handshake() {
            return Err(worker_startup::InitialStartFailure {
                message,
                retained_child: Some(Box::new(process)),
            });
        }
        Ok((
            client,
            IsolatedWorkerSupervisor {
                child: Some(WorkerChildHandle::Process(process)),
            },
        ))
    }
    fn teardown_retry_delay(&mut self) -> Duration {
        Duration::ZERO
    }
}

fn run_peer(
    mut peer: UnixStream,
    state: Arc<State>,
    outcome: Outcome,
    disabled: SyncSender<()>,
    release: Receiver<()>,
) {
    use ramshared_block::gpu_cache_worker::*;
    use std::io::{Read, Write};
    loop {
        let mut bytes = [0; FRAME_HEADER_LEN];
        if peer.read_exact(&mut bytes).is_err() {
            break;
        }
        let request = FrameHeader::decode(&bytes).unwrap();
        assert!(request.payload_len <= 4096);
        let mut payload = vec![0; request.payload_len as usize];
        peer.read_exact(&mut payload).unwrap();
        let response_type = match request.msg_type {
            MSG_HANDSHAKE_REQ => MSG_HANDSHAKE_RESP,
            MSG_HEARTBEAT_REQ => MSG_HEARTBEAT_RESP,
            MSG_DISABLE_REQ => MSG_DISABLE_RESP,
            MSG_READ_REQ => MSG_READ_RESP,
            MSG_UPDATE | MSG_PROMOTE | MSG_INVALIDATE_REQ => continue,
            _ => panic!("unexpected cache frame"),
        };
        if request.msg_type == MSG_DISABLE_REQ {
            assert!(state.held.load(Ordering::Acquire));
            assert_eq!(state.disables.fetch_add(1, Ordering::AcqRel), 0);
            disabled.send(()).unwrap();
            if outcome == Outcome::Timeout {
                let _ = release.recv_timeout(Duration::from_secs(20));
                break;
            }
        }
        let response = FrameHeader {
            msg_type: response_type,
            status: if request.msg_type == MSG_READ_REQ {
                STATUS_MISS
            } else if request.msg_type == MSG_DISABLE_REQ && outcome == Outcome::Refused {
                STATUS_ERROR
            } else {
                STATUS_OK
            },
            correlation_id: request.correlation_id,
            offset: match request.msg_type {
                MSG_DISABLE_REQ => 0,
                MSG_READ_REQ => request.offset,
                _ => 4096,
            },
            payload_len: 0,
            aux: 0,
        };
        if peer.write_all(&response.encode()).is_err() {
            break;
        }
    }
}

fn request(
    jobs: &std::sync::mpsc::SyncSender<WMsg>,
    command: Command,
    handle: u64,
    data: Vec<u8>,
) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    let length = if command == Command::Flush {
        0
    } else {
        BLOCK_SIZE
    };
    jobs.send(WMsg::Job(ramshared_wsl2d::conn::Job {
        export: 0,
        req: ramshared_block::Request {
            flags: if command == Command::Write {
                ramshared_block::protocol::NBD_CMD_FLAG_FUA
            } else {
                0
            },
            cmd: command,
            handle,
            offset: 0,
            len: length,
        },
        payload: data,
        credit: fixture_credit(),
        reply: tx,
    }))
    .unwrap();
    let reply = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(&reply.reply[4..8], &[0, 0, 0, 0]);
    assert_eq!(
        u64::from_be_bytes(reply.reply[8..16].try_into().unwrap()),
        handle
    );
    reply.data
}

fn same_descriptor(fd: i32, expected: &std::fs::Metadata) -> bool {
    std::fs::metadata(format!("/proc/self/fd/{fd}"))
        .is_ok_and(|m| m.dev() == expected.dev() && m.ino() == expected.ino())
}

fn run_origin(mut starter: Starter, file: File, socket: &Path) -> Result<(), String> {
    run_origin_nbd_with_startup(
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
        socket.to_str().unwrap().into(),
        false,
        "/dev/ramshared-origin-release-test".into(),
        2,
        &mut starter,
    )
    .map_err(|e| e.to_string())
}

fn exercise_drain_io(
    jobs: &std::sync::mpsc::SyncSender<WMsg>,
    started: &Receiver<()>,
    state: &State,
    origin: &Path,
) {
    request(jobs, Command::Write, 1, vec![0x51; 4096]);
    assert_eq!(request(jobs, Command::Read, 2, vec![]), vec![0x51; 4096]);
    jobs.send(WMsg::Shutdown).unwrap();
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    request(jobs, Command::Write, 3, vec![0x62; 4096]);
    request(jobs, Command::Flush, 4, vec![]);
    assert_eq!(request(jobs, Command::Read, 5, vec![]), vec![0x62; 4096]);
    assert!(!state.held.load(Ordering::Acquire));
    assert_eq!(&std::fs::read(origin).unwrap()[..4096], &[0x62; 4096]);
}

fn create_origin(origin: &Path) -> File {
    use std::io::Write;
    let mut file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(origin)
        .unwrap();
    file.write_all(&[17; 8192]).unwrap();
    file.sync_all().unwrap();
    file
}

fn journey(marker: &Path, outcome: Outcome) {
    let root = marker.parent().unwrap();
    let origin = root.join("origin.img");
    let file = create_origin(&origin);
    let fd = file.as_raw_fd();
    let identity = file.metadata().unwrap();
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let custody = Arc::new(worker_custody::WorkerCustody::default());
    let resources = custody.reserve().unwrap().bind(child);
    let observation = resources.child.lock().unwrap();
    let (jobs_tx, jobs_rx) = sync_channel(1);
    let (started_tx, started_rx) = sync_channel(1);
    let (drain_tx, drain_rx) = sync_channel(1);
    let (claim_tx, claim_rx) = sync_channel(1);
    let (done_tx, done_rx) = sync_channel(1);
    let socket = root.join("origin.sock");
    let state = Arc::new(State {
        probe: Some(Probe {
            fd,
            identity: identity.clone(),
            socket: socket.clone(),
            resources: resources.clone(),
        }),
        ..State::default()
    });
    let (disabled_tx, disabled_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let (peer_tx, peer_rx) = sync_channel(1);
    let thread_state = Arc::clone(&state);
    let process = ChildWorkerProcess::from_resources(resources.clone());
    let thread_socket = socket.clone();
    let starter = Starter {
        state: thread_state,
        jobs: jobs_tx,
        draining: started_tx,
        claimed: Some(claim_tx),
        drain: Some(drain_rx),
        initial: Some(process),
        outcome,
        disabled: Some(disabled_tx),
        peer_release: Some(release_rx),
        peer: peer_tx,
    };
    let daemon = std::thread::spawn(move || {
        let _ = done_tx.send(run_origin(starter, file, &thread_socket));
    });
    let jobs = jobs_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let peer = peer_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        exercise_drain_io(&jobs, &started_rx, &state, &origin);
    }));
    let _ = jobs.try_send(WMsg::Shutdown);
    drain_tx.try_send(SwapoffOutcome::Drained).unwrap();
    claim_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    disabled_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    // Accepted/refused: beyond the real collection budget. Timeout: while
    // the unchanged IPC release budget is still pending.
    let observe = if outcome == Outcome::Timeout {
        Duration::from_millis(250)
    } else {
        Duration::from_secs(6)
    };
    let early = done_rx.recv_timeout(observe);
    let pending = matches!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
    let held = state.held.load(Ordering::Acquire);
    let retained = same_descriptor(fd, &identity) && socket.exists();
    drop(observation);
    drop(input);
    let result = match early {
        Ok(result) => result,
        Err(_) => done_rx.recv_timeout(Duration::from_secs(7)).unwrap(),
    };
    daemon.join().unwrap();
    let _ = release_tx.send(());
    peer.join().unwrap();
    let mut child = ChildWorkerProcess::from_resources(resources.clone());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !(child.stop_is_quiescent() && child.try_wait().unwrap()) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    checked.unwrap();
    assert!(
        pending && held && retained,
        "runtime abandoned origin/permit before actual worker exit: pending={pending} held={held} retained={retained}"
    );
    assert_eq!(
        result,
        if outcome == Outcome::Accepted {
            Ok(())
        } else {
            Err("origin cache release was not acknowledged: cache release acknowledgement was unavailable".into())
        }
    );
    assert_eq!(state.starts.load(Ordering::Acquire), 1);
    assert_eq!(state.claims.load(Ordering::Acquire), 1);
    assert_eq!(state.disables.load(Ordering::Acquire), 1);
    assert_eq!(state.drains.load(Ordering::Acquire), 1);
    assert!(
        !state.held.load(Ordering::Acquire) && !socket.exists() && !same_descriptor(fd, &identity)
    );
    let data = std::fs::read(&origin).unwrap();
    assert_eq!(&data[..4096], &[0x62; 4096]);
    assert_eq!(&data[4096..], &[17; 4096]);
    assert!(custody.reserve().is_ok());
    std::fs::write(marker, b"complete").unwrap();
}

fn run(outcome: Outcome) {
    if let Some(marker) = std::env::var_os(CHILD) {
        journey(Path::new(&marker), outcome);
        return;
    }
    let root = std::env::temp_dir().join(format!(
        "ramshared-origin-release-{}-{}",
        std::process::id(),
        UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let marker = root.join("completed");
    let log = root.join("child.log");
    let output = File::options()
        .create_new(true)
        .write(true)
        .open(&log)
        .unwrap();
    let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
        .args(["--exact", outcome.test(), "--nocapture"])
        .env(CHILD, &marker)
        .process_group(0)
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            // SAFETY: this group contains only this nonstorage Linux test and
            // its own stdin-held shell, never a product or Windows launcher.
            let group = i32::try_from(child.id()).unwrap();
            let killed = unsafe { libc::kill(-group, libc::SIGKILL) };
            assert!(
                killed == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            );
            let status = child.wait().unwrap();
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let content = std::fs::read_to_string(&log).unwrap();
    let completed = std::fs::read(&marker).ok();
    std::fs::remove_dir_all(root).unwrap();
    eprint!("{content}");
    assert!(
        status.success() && completed.as_deref() == Some(b"complete"),
        "actual origin shutdown failed: {status}\n{content}"
    );
}

#[test]
fn origin_shutdown_preserves_rejected_release_until_worker_exit() {
    run(Outcome::Refused);
}
#[test]
fn origin_shutdown_waits_for_release_timeout_and_worker_exit() {
    run(Outcome::Timeout);
}
#[test]
fn origin_shutdown_accepts_release_only_after_worker_exit() {
    run(Outcome::Accepted);
}
