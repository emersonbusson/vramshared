// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/ublk_server/join_tests.rs
#![allow(clippy::expect_used)]

use super::residency_worker::ServerHandleDt3VramResidency;
use super::*;
use crate::ResidencyConfig;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn regular_file_fixture(label: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("ramshared-{label}-{}-{nonce}", std::process::id()));
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("create regular-file fixture");
    file.set_len(ramshared_uring::page_size() as u64)
        .expect("size regular-file fixture");
    path
}

fn join_with_timeout<T: Send + 'static>(join: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = tx.send(join());
    });
    rx.recv_timeout(Duration::from_secs(2))
        .expect("ublk refusal fixture must terminate promptly")
}

pub(super) fn descriptor_bytes(cmd: u8, sectors: u32) -> [u8; ublk::UBLK_IO_DESC_SIZE] {
    let mut bytes = [0u8; ublk::UBLK_IO_DESC_SIZE];
    bytes[0..4].copy_from_slice(&u32::from(cmd).to_ne_bytes());
    bytes[4..8].copy_from_slice(&sectors.to_ne_bytes());
    bytes
}

pub(super) struct FakeQueue {
    completions: VecDeque<Vec<ramshared_uring::UblkCompletion>>,
    descriptor: [u8; ublk::UBLK_IO_DESC_SIZE],
    pub(super) buffers: Vec<Vec<u8>>,
    commits: Arc<Mutex<Vec<(u16, i32)>>>,
    submitted: bool,
}

impl FakeQueue {
    pub(super) fn new(
        descriptor: [u8; ublk::UBLK_IO_DESC_SIZE],
        completions: Vec<Vec<ramshared_uring::UblkCompletion>>,
        commits: Arc<Mutex<Vec<(u16, i32)>>>,
    ) -> Self {
        Self {
            completions: completions.into(),
            descriptor,
            buffers: vec![vec![0u8; 4096]],
            commits,
            submitted: false,
        }
    }
}

impl QueueServer for FakeQueue {
    fn submit_initial_fetch(&mut self) -> io::Result<()> {
        self.submitted = true;
        Ok(())
    }

    fn drain(&mut self) -> Vec<ramshared_uring::UblkCompletion> {
        self.completions.pop_front().unwrap_or_default()
    }

    fn wait_and_drain(&mut self) -> io::Result<Vec<ramshared_uring::UblkCompletion>> {
        if !self.submitted {
            return Err(io::Error::other("fetch not submitted"));
        }
        self.completions
            .pop_front()
            .ok_or_else(|| io::Error::other("manufactured completion exhaustion"))
    }

    fn io_desc_snapshot(&self, tag: u16) -> io::Result<[u8; ublk::UBLK_IO_DESC_SIZE]> {
        if usize::from(tag) < self.buffers.len() {
            Ok(self.descriptor)
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid tag"))
        }
    }

    fn buffer_mut(&mut self, tag: u16) -> io::Result<&mut [u8]> {
        self.buffers
            .get_mut(usize::from(tag))
            .map(Vec::as_mut_slice)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid tag"))
    }

    fn commit_and_fetch(&mut self, tag: u16, result: i32) -> io::Result<()> {
        self.commits
            .lock()
            .expect("commit mutex")
            .push((tag, result));
        Ok(())
    }
}

#[test]
fn ublk_entrypoints_refuse_regular_files_before_spawning_workers() {
    let path = regular_file_fixture("ublk-server");
    for result in [
        spawn_server(&path, 1, 4096, RamBackend::new(4096)).map(|_| ()),
        spawn_server_dt3(&path, 1, 4096, RamBackend::new(4096)).map(|_| ()),
        spawn_server_dt3_vram(&path, 1, 4096, 4096, 4096).map(|_| ()),
        spawn_server_dt3_vram_with_residency(
            &path,
            1,
            4096,
            4096,
            4096,
            "/dev/ublkb-ci-fixture".into(),
            ResidencyConfig::default(),
        )
        .map(|_| ()),
    ] {
        assert_eq!(
            result
                .expect_err("regular files must be refused before starting workers")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    fs::remove_file(path).expect("remove regular-file fixture");
}

#[test]
fn ublk_entrypoints_refuse_non_ublk_character_device_before_spawning_workers() {
    let path = Path::new("/dev/null");
    assert!(
        path.metadata()
            .expect("the standard null device exists")
            .file_type()
            .is_char_device()
    );

    assert!(spawn_server(path, 1, 4096, RamBackend::new(4096)).is_err());
    assert!(spawn_server_dt3(path, 1, 4096, RamBackend::new(4096)).is_err());
    assert!(spawn_server_dt3_vram(path, 1, 4096, 4096, 4096).is_err());
    assert!(
        spawn_server_dt3_vram_with_residency(
            path,
            1,
            4096,
            4096,
            4096,
            "/dev/ublkb-ci-fixture".into(),
            ResidencyConfig::default(),
        )
        .is_err()
    );
}

#[test]
fn server_entrypoint_terminates_on_non_ublk_character_device() {
    let path = Path::new("/dev/zero");
    assert!(
        path.metadata()
            .expect("the standard zero device exists")
            .file_type()
            .is_char_device()
    );

    let server = spawn_server(path, 1, ramshared_uring::page_size(), RamBackend::new(4096))
        .expect("the character-device refusal fixture builds the server");
    let result = join_with_timeout(move || server.join());

    assert!(result.is_err(), "a non-ublk device must refuse FETCH");
}

#[test]
fn queue_server_adapter_forwards_operations_without_a_ublk_device() {
    let page_size = ramshared_uring::page_size();
    let path = regular_file_fixture("ublk-adapter");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open adapter fixture");
    let mut server = ramshared_uring::UblkServer::new(file.as_raw_fd(), 1, page_size)
        .expect("create io_uring adapter fixture");

    assert!(QueueServer::drain(&mut server).is_empty());
    assert_eq!(
        QueueServer::io_desc_snapshot(&server, 0).expect("descriptor snapshot"),
        [0; ublk::UBLK_IO_DESC_SIZE]
    );
    assert_eq!(
        QueueServer::buffer_mut(&mut server, 0)
            .expect("tag buffer")
            .len(),
        page_size
    );

    QueueServer::submit_initial_fetch(&mut server).expect("submit initial fetch");
    let fetch = QueueServer::wait_and_drain(&mut server).expect("drain refused fetch");
    assert_eq!(fetch.len(), 1);
    assert!(fetch[0].result < 0);

    QueueServer::commit_and_fetch(&mut server, 0, EINVAL).expect("submit refusal");
    let commit = QueueServer::wait_and_drain(&mut server).expect("drain refused commit");
    assert_eq!(commit.len(), 1);
    assert!(commit[0].result < 0);

    drop(server);
    drop(file);
    fs::remove_file(path).expect("remove adapter fixture");
}

#[test]
fn cuda_errors_convert_to_other_io_errors_with_context() {
    let error = cuda_to_io(ramshared_cuda::CudaError::InvalidValue(
        "invalid test input".into(),
    ));

    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(error.to_string().contains("CUDA: invalid argument"));
}

#[test]
fn single_thread_server_loop_handles_idle_read_and_terminal_abort() {
    let commits = Arc::new(Mutex::new(Vec::new()));
    let mut queue = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_READ, 1),
        vec![
            Vec::new(),
            vec![ramshared_uring::UblkCompletion { tag: 0, result: 0 }],
            vec![ramshared_uring::UblkCompletion {
                tag: 0,
                result: ublk::UBLK_IO_RES_ABORT,
            }],
        ],
        Arc::clone(&commits),
    );
    queue.buffers[0][..512].fill(0x5a);

    let backend = run_server_loop(queue, RamBackend::new(4096))
        .expect("an abort completion returns the owned backend");

    assert_eq!(backend.size_bytes(), 4096);
    assert_eq!(*commits.lock().expect("commit mutex"), vec![(0, 512)]);
}

#[test]
fn single_thread_server_loop_commits_unsupported_ops_and_rejects_fetch_errors() {
    let commits = Arc::new(Mutex::new(Vec::new()));
    let unsupported = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_WRITE_SAME, 1),
        vec![
            vec![ramshared_uring::UblkCompletion { tag: 0, result: 0 }],
            vec![ramshared_uring::UblkCompletion {
                tag: 0,
                result: ublk::UBLK_IO_RES_ABORT,
            }],
        ],
        Arc::clone(&commits),
    );
    run_server_loop(unsupported, RamBackend::new(4096))
        .expect("unsupported commands are refused without ending the loop");
    assert_eq!(*commits.lock().expect("commit mutex"), vec![(0, EINVAL)]);

    let failed_fetch = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_READ, 1),
        vec![vec![ramshared_uring::UblkCompletion { tag: 0, result: -5 }]],
        Arc::new(Mutex::new(Vec::new())),
    );
    let error = match run_server_loop(failed_fetch, RamBackend::new(4096)) {
        Err(error) => error,
        Ok(_) => panic!("a failed FETCH must terminate the single-thread loop"),
    };
    assert!(error.to_string().contains("FETCH failed: -5"));
}

#[test]
fn single_thread_server_loop_returns_fetch_setup_and_commit_errors() {
    struct FailingQueue {
        fail_submit: bool,
        fail_commit: bool,
        completions: VecDeque<ramshared_uring::UblkCompletion>,
        buffer: Vec<u8>,
    }
    impl QueueServer for FailingQueue {
        fn submit_initial_fetch(&mut self) -> io::Result<()> {
            if self.fail_submit {
                Err(io::Error::other("submit refused"))
            } else {
                Ok(())
            }
        }

        fn drain(&mut self) -> Vec<ramshared_uring::UblkCompletion> {
            self.completions.pop_front().into_iter().collect()
        }

        fn wait_and_drain(&mut self) -> io::Result<Vec<ramshared_uring::UblkCompletion>> {
            unreachable!("the single-thread loop does not block in this adapter")
        }

        fn io_desc_snapshot(&self, _tag: u16) -> io::Result<[u8; ublk::UBLK_IO_DESC_SIZE]> {
            Ok(descriptor_bytes(ublk::UBLK_IO_OP_READ, 1))
        }

        fn buffer_mut(&mut self, _tag: u16) -> io::Result<&mut [u8]> {
            Ok(&mut self.buffer)
        }

        fn commit_and_fetch(&mut self, _tag: u16, _result: i32) -> io::Result<()> {
            if self.fail_commit {
                Err(io::Error::other("commit refused"))
            } else {
                Ok(())
            }
        }
    }

    let submit_error = match run_server_loop(
        FailingQueue {
            fail_submit: true,
            fail_commit: false,
            completions: VecDeque::new(),
            buffer: vec![0u8; 4096],
        },
        RamBackend::new(4096),
    ) {
        Err(error) => error,
        Ok(_) => panic!("FETCH setup failure must be returned"),
    };
    assert!(submit_error.to_string().contains("submit refused"));

    let commit_error = match run_server_loop(
        FailingQueue {
            fail_submit: false,
            fail_commit: true,
            completions: VecDeque::from([ramshared_uring::UblkCompletion { tag: 0, result: 0 }]),
            buffer: vec![0u8; 4096],
        },
        RamBackend::new(4096),
    ) {
        Err(error) => error,
        Ok(_) => panic!("COMMIT_AND_FETCH failure must be returned"),
    };
    assert!(commit_error.to_string().contains("commit refused"));
}

#[test]
fn fake_queue_runs_dispatch_commit_and_abort_without_a_device() {
    let commits = Arc::new(Mutex::new(Vec::new()));
    let queue = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_READ, 1),
        vec![
            vec![ramshared_uring::UblkCompletion { tag: 0, result: 0 }],
            vec![ramshared_uring::UblkCompletion {
                tag: 0,
                result: ublk::UBLK_IO_RES_ABORT,
            }],
        ],
        Arc::clone(&commits),
    );
    let (work_tx, work_rx) = mpsc::sync_channel(1);
    let (reply_tx, reply_rx) = mpsc::channel();
    let worker = spawn_ublk_worker(RamBackend::new(4096), work_rx, reply_tx);
    run_ring_owner(queue, 1, 4096, work_tx, reply_rx).expect("fake queue loop");
    assert_eq!(worker.join().expect("fake worker").size_bytes(), 4096);
    assert_eq!(*commits.lock().expect("commit mutex"), vec![(0, 512)]);
}

#[test]
fn fake_queue_rejects_unsupported_and_preserves_write_payload() {
    let commits = Arc::new(Mutex::new(Vec::new()));
    let unsupported = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_WRITE_SAME, 1),
        vec![
            vec![ramshared_uring::UblkCompletion { tag: 0, result: 0 }],
            vec![ramshared_uring::UblkCompletion {
                tag: 0,
                result: ublk::UBLK_IO_RES_ABORT,
            }],
        ],
        Arc::clone(&commits),
    );
    let (work_tx, work_rx) = mpsc::sync_channel(1);
    let (reply_tx, reply_rx) = mpsc::channel();
    let worker = spawn_ublk_worker(RamBackend::new(4096), work_rx, reply_tx);
    run_ring_owner(unsupported, 1, 4096, work_tx, reply_rx).expect("unsupported request loop");
    worker.join().expect("unsupported worker");
    assert_eq!(*commits.lock().expect("commit mutex"), vec![(0, EINVAL)]);

    let write_commits = Arc::new(Mutex::new(Vec::new()));
    let mut write_queue = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_WRITE, 1),
        Vec::new(),
        Arc::clone(&write_commits),
    );
    write_queue.buffers[0][..512].fill(0x5a);
    let (work_tx, work_rx) = mpsc::sync_channel(1);
    let mut pool = vec![vec![0u8; 4096]];
    assert!(dispatch_request(&mut write_queue, 0, &work_tx, &mut pool).expect("dispatch write"));
    let work = work_rx.recv().expect("write work");
    assert!(work.payload.iter().all(|byte| *byte == 0x5a));
    commit_reply(
        &mut write_queue,
        WorkerReply {
            qid: 0,
            tag: 0,
            result: 512,
            buf: work.payload,
            is_read: false,
        },
        &mut pool,
    )
    .expect("commit write");
    assert_eq!(
        *write_commits.lock().expect("write commit mutex"),
        vec![(0, 512)]
    );
    assert_eq!(pool.len(), 1);
}

#[test]
fn dispatch_refuses_oversized_write_and_disconnected_worker() {
    let oversized_commits = Arc::new(Mutex::new(Vec::new()));
    let mut oversized = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_WRITE, 9),
        Vec::new(),
        Arc::clone(&oversized_commits),
    );
    let (work_tx, work_rx) = mpsc::sync_channel(1);
    let mut pool = vec![vec![0u8; 4096]];
    assert!(
        !dispatch_request(&mut oversized, 0, &work_tx, &mut pool)
            .expect("oversized write is completed as a refusal")
    );
    assert_eq!(
        *oversized_commits.lock().expect("commit mutex"),
        vec![(0, EINVAL)]
    );
    assert_eq!(pool.len(), 1, "refused buffer returns to its pool");
    assert!(work_rx.try_recv().is_err());

    let mut read = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_READ, 1),
        Vec::new(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let (work_tx, work_rx) = mpsc::sync_channel(1);
    drop(work_rx);
    let error = dispatch_request(&mut read, 0, &work_tx, &mut pool)
        .expect_err("worker disconnect fails the request closed");
    assert!(error.to_string().contains("worker terminated unexpectedly"));
}

#[test]
fn worker_loop_stops_when_the_ring_owner_drops_its_reply_receiver() {
    let (work_tx, work_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = mpsc::channel();
    drop(reply_rx);
    work_tx
        .send(ublk::IoWork {
            qid: 0,
            tag: 0,
            buffer_addr: 0,
            req: Request {
                flags: 0,
                cmd: Command::Read,
                handle: 1,
                offset: 0,
                len: 512,
            },
            payload: vec![0; 512],
        })
        .expect("queue test request");
    drop(work_tx);

    worker_loop(&mut RamBackend::new(4096), work_rx, reply_tx);
}

#[test]
fn ring_owner_stops_when_worker_reply_channel_is_disconnected() {
    let queue = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_READ, 1),
        vec![vec![ramshared_uring::UblkCompletion { tag: 0, result: 0 }]],
        Arc::new(Mutex::new(Vec::new())),
    );
    let (work_tx, _work_rx) = mpsc::sync_channel(1);
    let (reply_tx, reply_rx) = mpsc::channel();
    drop(reply_tx);

    let error = run_ring_owner(queue, 1, 4096, work_tx, reply_rx)
        .expect_err("a stopped worker cannot leave the ring owner waiting forever");
    assert!(error.to_string().contains("worker terminated unexpectedly"));
}

#[test]
fn server_and_worker_join_outcomes_are_fail_closed() {
    let success = ServerHandle {
        thread: thread::spawn(|| Ok(RamBackend::new(512))),
    };
    assert_eq!(success.join().expect("server success").size_bytes(), 512);

    let panic = ServerHandle {
        thread: thread::spawn(|| -> io::Result<RamBackend> { panic!("manufactured") }),
    };
    let panic_error = match panic.join() {
        Ok(_) => panic!("manufactured server panic must be refused"),
        Err(error) => error,
    };
    assert_eq!(panic_error.to_string(), "server thread panicked");

    let worker = WorkerHandle {
        thread: thread::spawn(|| -> u8 { panic!("manufactured") }),
    };
    assert_eq!(
        worker.join().expect_err("worker panic refusal").to_string(),
        "ublk worker panicked"
    );

    let dt3_success = ServerHandleDt3 {
        ring: thread::spawn(|| Ok(())),
        worker: WorkerHandle {
            thread: thread::spawn(|| 11u8),
        },
    };
    assert_eq!(dt3_success.join().expect("DT-3 success"), 11);

    let residency = ServerHandleDt3VramResidency {
        ring: thread::spawn(|| Ok(())),
        worker: thread::spawn(|| Ok(())),
        demotes: Arc::new(AtomicU32::new(3)),
        shutdown_tx: mpsc::sync_channel(1).0,
    };
    assert_eq!(residency.demote_count(), 3);
    residency.join().expect("residency success");
}

#[test]
fn serve_request_refuses_unaligned_and_out_of_bounds() {
    let mut backend = RamBackend::new(4096);
    let mut buffer = vec![0u8; 8192];
    let mut request = Request {
        flags: 0,
        cmd: Command::Read,
        handle: 1,
        offset: 0,
        len: 4096,
    };
    assert_eq!(serve_request(&request, &mut backend, &mut buffer), 4096);

    request.offset = 100; // Unaligned to 512
    assert_eq!(serve_request(&request, &mut backend, &mut buffer), EINVAL);

    request.offset = 0;
    request.len = 100; // Length unaligned
    assert_eq!(serve_request(&request, &mut backend, &mut buffer), EINVAL);

    request.len = 4096;
    request.offset = 4096; // Out of bounds (backend is 4096, offset 4096 + len 4096 = 8192 > 4096)
    assert_eq!(serve_request(&request, &mut backend, &mut buffer), ERANGE);
}

#[test]
fn serve_request_refuses_unsupported_commands_and_covers_trim() {
    let mut backend = RamBackend::new(4096);
    let mut buffer = vec![0u8; 4096];
    for (cmd, expected) in [
        (Command::Trim, 0),
        (Command::Disc, EINVAL),
        (Command::Unknown(255), EINVAL),
    ] {
        let request = Request {
            flags: 0,
            cmd,
            handle: 1,
            offset: 0,
            len: 4096,
        };
        assert_eq!(serve_request(&request, &mut backend, &mut buffer), expected);
    }

    let oversized = Request {
        flags: 0,
        cmd: Command::Read,
        handle: 2,
        offset: 0,
        len: 8192,
    };
    assert_eq!(serve_request(&oversized, &mut backend, &mut buffer), EINVAL);
}

#[test]
fn dt3_join_attempts_worker_after_ring_error() {
    let completed = Arc::new(AtomicBool::new(false));
    let worker_completed = Arc::clone(&completed);
    let handle = ServerHandleDt3 {
        ring: thread::spawn(|| Err(io::Error::other("ring failure"))),
        worker: WorkerHandle {
            thread: thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                worker_completed.store(true, Ordering::Release);
                7u8
            }),
        },
    };

    let err = handle.join().expect_err("ring failure must win");
    assert_eq!(err.to_string(), "ring failure");
    assert!(completed.load(Ordering::Acquire));
}

#[test]
fn ring_owner_commits_each_ready_reply_and_refuses_idle_fetch_errors() {
    let commits = Arc::new(Mutex::new(Vec::new()));
    let mut queue = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_READ, 1),
        vec![
            vec![
                ramshared_uring::UblkCompletion { tag: 0, result: 0 },
                ramshared_uring::UblkCompletion { tag: 1, result: 0 },
            ],
            vec![ramshared_uring::UblkCompletion {
                tag: 0,
                result: ublk::UBLK_IO_RES_ABORT,
            }],
        ],
        Arc::clone(&commits),
    );
    queue.buffers.push(vec![0; 4096]);
    let (work_tx, work_rx) = mpsc::sync_channel(2);
    let (reply_tx, reply_rx) = mpsc::channel();
    for tag in [0, 1] {
        reply_tx
            .send(WorkerReply {
                qid: 0,
                tag,
                result: 512,
                buf: vec![tag as u8; 512],
                is_read: true,
            })
            .expect("ready fixture reply");
    }
    run_ring_owner(queue, 2, 4096, work_tx, reply_rx).expect("abort after both replies");
    assert_eq!(work_rx.try_iter().count(), 2);
    assert_eq!(*commits.lock().expect("commit mutex"), [(0, 512), (1, 512)]);

    let queue = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_READ, 1),
        vec![vec![ramshared_uring::UblkCompletion { tag: 0, result: -5 }]],
        Arc::new(Mutex::new(Vec::new())),
    );
    let (work_tx, work_rx) = mpsc::sync_channel(1);
    let (_reply_tx, reply_rx) = mpsc::channel();
    let error = run_ring_owner(queue, 1, 4096, work_tx, reply_rx).expect_err("idle FETCH error");
    assert_eq!(error.to_string(), "FETCH failed: -5");
    assert_eq!(work_rx.try_iter().count(), 0);
}

#[test]
fn residency_shutdown_handoff_preserves_one_lease_and_reports_full_channel() {
    let permit = Arc::new(
        crate::tier_release::try_claim_backend_shutdown_for(
            "/dev/ramshared-absent-ublk-handoff-fixture",
        )
        .expect("read-only absence claims isolated custody"),
    );
    let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
    let handle = ServerHandleDt3VramResidency {
        ring: thread::spawn(|| Ok(())),
        worker: thread::spawn(|| Ok(())),
        demotes: Arc::new(AtomicU32::new(0)),
        shutdown_tx,
    };
    handle
        .authorize_shutdown(Arc::clone(&permit))
        .expect("initial handoff");
    let error = handle
        .authorize_shutdown(Arc::clone(&permit))
        .expect_err("duplicate handoff");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    let worker_permit = shutdown_rx.recv().expect("same lease reaches worker");
    assert!(Arc::ptr_eq(&permit, &worker_permit));
    drop(shutdown_rx);
    handle
        .authorize_shutdown(Arc::clone(&permit))
        .expect("already exited receiver");
    drop(permit);
    assert!(!crate::tier_release::release_custody_is_clear());
    handle.join().expect("both empty fixture owners finish");
    assert!(!crate::tier_release::release_custody_is_clear());
    drop(worker_permit);
}

#[test]
fn vram_joins_report_panics_after_observing_both_owners() {
    for ring_panics in [false, true] {
        let observed = Arc::new(AtomicBool::new(false));
        let worker_observed = Arc::clone(&observed);
        let handle = ServerHandleDt3Vram {
            ring: thread::spawn(move || {
                assert!(!ring_panics, "injected ring panic");
                Ok(())
            }),
            worker: thread::spawn(move || {
                worker_observed.store(true, Ordering::Release);
                panic!("injected worker panic");
            }),
        };
        let error = handle.join().expect_err("panics fail completion");
        assert_eq!(
            error.to_string(),
            if ring_panics {
                "ring owner panicked"
            } else {
                "vram worker panicked"
            }
        );
        assert!(observed.load(Ordering::Acquire));

        let observed = Arc::new(AtomicBool::new(false));
        let worker_observed = Arc::clone(&observed);
        let handle = ServerHandleDt3VramResidency {
            ring: thread::spawn(move || {
                assert!(!ring_panics, "injected ring panic");
                Ok(())
            }),
            worker: thread::spawn(move || {
                worker_observed.store(true, Ordering::Release);
                panic!("injected residency-worker panic");
            }),
            demotes: Arc::new(AtomicU32::new(0)),
            shutdown_tx: mpsc::sync_channel(1).0,
        };
        let error = handle.join().expect_err("panics fail completion");
        assert_eq!(
            error.to_string(),
            if ring_panics {
                "ring owner panicked"
            } else {
                "vram residency worker panicked"
            }
        );
        assert!(observed.load(Ordering::Acquire));
    }
}

#[test]
fn dt3_vram_join_attempts_worker_after_ring_error() {
    let completed = Arc::new(AtomicBool::new(false));
    let worker_completed = Arc::clone(&completed);
    let handle = ServerHandleDt3Vram {
        ring: thread::spawn(|| Err(io::Error::other("ring failure"))),
        worker: thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            worker_completed.store(true, Ordering::Release);
            Ok(())
        }),
    };

    let err = handle.join().expect_err("ring failure must win");
    assert_eq!(err.to_string(), "ring failure");
    assert!(completed.load(Ordering::Acquire));
}
