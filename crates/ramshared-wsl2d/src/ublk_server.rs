// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/ublk_server.rs
//! RAM backend and I/O service logic for the ublk loop.
//!
//! `serve_request` is pure: given a `Request` and the data buffer, serves against a
//! `BlockBackend` and returns the `result` (bytes `>= 0`, or `-errno`) that the COMMIT
//! must load. `RamBackend` validates the loop without CUDA; `spawn_ublk_worker` is the
//! worker DT-3 (thread owning the backend) that will be used with `VramBackend`.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ramshared_block::{BlockBackend, Command, Request};
use ramshared_cuda::Cuda;

use crate::VramBackend;
use crate::backend::RamBackend;
use crate::ublk;

mod residency_worker;
pub use residency_worker::{ServerHandleDt3VramResidency, spawn_server_dt3_vram_with_residency};

const EIO: i32 = -5;
const EINVAL: i32 = -22;
const ERANGE: i32 = -34;

fn open_ublk_char_device(path: impl AsRef<Path>) -> io::Result<std::fs::File> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    if !file.metadata()?.file_type().is_char_device() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ublk server path is not a character device",
        ));
    }
    Ok(file)
}

trait QueueServer {
    fn submit_initial_fetch(&mut self) -> io::Result<()>;
    fn drain(&mut self) -> Vec<ramshared_uring::UblkCompletion>;
    fn wait_and_drain(&mut self) -> io::Result<Vec<ramshared_uring::UblkCompletion>>;
    fn io_desc_snapshot(&self, tag: u16) -> io::Result<[u8; ublk::UBLK_IO_DESC_SIZE]>;
    fn buffer_mut(&mut self, tag: u16) -> io::Result<&mut [u8]>;
    fn commit_and_fetch(&mut self, tag: u16, result: i32) -> io::Result<()>;
}

impl QueueServer for ramshared_uring::UblkServer {
    fn submit_initial_fetch(&mut self) -> io::Result<()> {
        ramshared_uring::UblkServer::submit_initial_fetch(self)
    }

    fn drain(&mut self) -> Vec<ramshared_uring::UblkCompletion> {
        ramshared_uring::UblkServer::drain(self)
    }

    fn wait_and_drain(&mut self) -> io::Result<Vec<ramshared_uring::UblkCompletion>> {
        ramshared_uring::UblkServer::wait_and_drain(self)
    }

    fn io_desc_snapshot(&self, tag: u16) -> io::Result<[u8; ublk::UBLK_IO_DESC_SIZE]> {
        ramshared_uring::UblkServer::io_desc_snapshot(self, tag)
    }

    fn buffer_mut(&mut self, tag: u16) -> io::Result<&mut [u8]> {
        ramshared_uring::UblkServer::buffer_mut(self, tag)
    }

    fn commit_and_fetch(&mut self, tag: u16, result: i32) -> io::Result<()> {
        ramshared_uring::UblkServer::commit_and_fetch(self, tag, result)
    }
}

/// Serves a ublk `Request` against any [`BlockBackend`] using `buf` (where data
/// lives) and returns the COMMIT `result`: transferred bytes (`>= 0`) or
/// `-errno`. Serves **in-place** in the buffer (no alloc in the hot path — DT-8). `buf` is the
/// tag buffer in the single-threaded loop, or a worker buffer in DT-3.
///
/// In WRITE, `buf` already contains the data (kernel copied from the bio); in READ, the backend
/// populates `buf` and the kernel copies `result` bytes back on COMMIT — which is why
/// `result` must be exactly the bytes served.
pub fn serve_request<B: BlockBackend + ?Sized>(
    req: &Request,
    backend: &mut B,
    buf: &mut [u8],
) -> i32 {
    let len = req.len as usize;
    if len > buf.len() {
        return EINVAL; // request larger than the available buffer
    }

    let bs = backend.block_size() as u64;
    if bs > 0 && (!req.offset.is_multiple_of(bs) || !(req.len as u64).is_multiple_of(bs)) {
        return EINVAL;
    }

    // Physical bounds guard
    if req
        .offset
        .checked_add(req.len as u64)
        .is_none_or(|end| end > backend.size_bytes())
    {
        return ERANGE;
    }

    // Command dispatch is exhaustive so a new `Command` variant is a compile
    // error here, never a panic on the device I/O path.
    let served = match req.cmd {
        Command::Read => backend.read_at(req.offset, &mut buf[..len]).map(|()| len),
        Command::Write => backend.write_at(req.offset, &buf[..len]).map(|()| len),
        Command::Flush => backend.flush().map(|()| 0),
        // Discard is a safe no-op in the MVP. Disconnect is not an ublk request
        // command — the device lifecycle is STOP/DEL_DEV driven — so `Disc` and
        // unknown opcodes are rejected like any other unsupported command.
        Command::Trim => return 0,
        Command::Disc | Command::Unknown(_) => return EINVAL,
    };

    match served {
        Ok(bytes) => i32::try_from(bytes).unwrap_or(EIO),
        Err(_) => EIO,
    }
}

/// Handle of the ublk server thread; `join` waits for the loop to terminate (upon receiving the
/// abort from STOP/DEL_DEV) and returns the `RamBackend` for inspection.
pub struct ServerHandle {
    thread: JoinHandle<io::Result<RamBackend>>,
}

impl ServerHandle {
    pub fn join(self) -> io::Result<RamBackend> {
        match self.thread.join() {
            Ok(result) => result,
            Err(_) => Err(io::Error::other("server thread panicked")),
        }
    }
}

/// Opens `char_path`, creates the `UblkServer` and runs the service loop in its own
/// thread (sole owner of the ring, DT-3). The thread submits FETCH, serves each request
/// against `backend` and re-arms via COMMIT_AND_FETCH; terminates upon receiving the abort
/// (`UBLK_IO_RES_ABORT`) triggered by STOP/DEL_DEV.
pub fn spawn_server(
    char_path: impl AsRef<Path>,
    queue_depth: u16,
    buf_size: usize,
    backend: RamBackend,
) -> io::Result<ServerHandle> {
    let char_dev = open_ublk_char_device(char_path)?;
    let server = ramshared_uring::UblkServer::new(char_dev.as_raw_fd(), queue_depth, buf_size)?;

    let thread = thread::spawn(move || {
        // Keeps the char device open while the loop uses the ring (dropped after).
        let _char_dev = char_dev;
        run_server_loop(server, backend)
    });

    Ok(ServerHandle { thread })
}

fn run_server_loop<S: QueueServer>(
    mut server: S,
    mut backend: RamBackend,
) -> io::Result<RamBackend> {
    server.submit_initial_fetch()?;

    loop {
        let completions = server.drain();
        if completions.is_empty() {
            thread::sleep(Duration::from_micros(200));
            continue;
        }

        for completion in completions {
            if completion.result == ublk::UBLK_IO_RES_ABORT {
                return Ok(backend); // teardown: STOP/DEL_DEV aborted the FETCH
            }
            if completion.result < 0 {
                return Err(io::Error::other(format!(
                    "FETCH failed: {}",
                    completion.result
                )));
            }

            // result == UBLK_IO_RES_OK (0): there is a request ready at the tag.
            let snapshot = server.io_desc_snapshot(completion.tag)?;
            let iod = ublk::IoDesc::from_ne_bytes(&snapshot)
                .ok_or_else(|| io::Error::other("invalid io-desc snapshot"))?;
            let result = match iod.to_block_request(completion.tag) {
                Ok(req) => serve_request(&req, &mut backend, server.buffer_mut(completion.tag)?),
                Err(_) => EINVAL, // ublk op without safe equivalence
            };
            server.commit_and_fetch(completion.tag, result)?;
        }
    }
}

/// Reply from worker DT-3 to the ring owner. `buf` is the buffer yielded by the ring
/// owner, returned for recycling (pool without alloc in the hot path — DT-8). When
/// `is_read` and `result >= 0`, `buf` carries the `result` read bytes that the ring owner
/// copies to the tag buffer before `commit_and_fetch`.
#[derive(Clone, Debug)]
pub struct WorkerReply {
    pub qid: u16,
    pub tag: u16,
    pub result: i32,
    pub buf: Vec<u8>,
    pub is_read: bool,
}

/// Handle of the worker thread DT-3; `join` waits for the worker to terminate (when the `IoWork`
/// channel closes) and returns the backend.
pub struct WorkerHandle<B> {
    thread: JoinHandle<B>,
}

impl<B> WorkerHandle<B> {
    pub fn join(self) -> io::Result<B> {
        self.thread
            .join()
            .map_err(|_| io::Error::other("ublk worker panicked"))
    }
}

/// Starts worker DT-3: the thread owning `backend` (the only one touching VRAM/CUDA).
/// Receives `IoWork` through the channel, serves against the `backend` and returns `WorkerReply`.
/// Terminates when `work_rx` closes (the ring owner dropped) or `reply_tx` breaks.
pub fn spawn_ublk_worker<B: BlockBackend + Send + 'static>(
    mut backend: B,
    work_rx: Receiver<ublk::IoWork>,
    reply_tx: Sender<WorkerReply>,
) -> WorkerHandle<B> {
    let thread = thread::spawn(move || {
        worker_loop(&mut backend, work_rx, reply_tx);
        backend
    });
    WorkerHandle { thread }
}

fn worker_loop<B: BlockBackend>(
    backend: &mut B,
    work_rx: Receiver<ublk::IoWork>,
    reply_tx: Sender<WorkerReply>,
) {
    while let Ok(mut work) = work_rx.recv() {
        // `payload` is the buffer yielded by the ring owner, already sized to `req.len`:
        // in WRITE it carries data from the bio; in READ the backend populates it. The worker
        // serves in-place and returns the same buffer — no alloc here (DT-8).
        let result = serve_request(&work.req, backend, &mut work.payload);
        let is_read = work.req.cmd == Command::Read;

        let reply = WorkerReply {
            qid: work.qid,
            tag: work.tag,
            result,
            buf: work.payload,
            is_read,
        };
        if reply_tx.send(reply).is_err() {
            break; // ring owner dropped
        }
    }
}

const RING_CHAN_CAP: usize = 64;

/// Handle of the DT-3 server (ring owner + worker). `join` waits for the ring owner
/// to terminate (on abort of STOP/DEL_DEV), which closes the channel and terminates the worker,
/// and returns the backend.
pub struct ServerHandleDt3<B> {
    ring: JoinHandle<io::Result<()>>,
    worker: WorkerHandle<B>,
}

impl<B> ServerHandleDt3<B> {
    pub fn join(self) -> io::Result<B> {
        let ring_result = self
            .ring
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("ring owner panicked")));
        let worker_result = self.worker.join();
        match ring_result {
            Ok(()) => worker_result,
            Err(error) => Err(error),
        }
    }
}

/// Starts the ublk server in the DT-3 architecture: a **ring owner** thread (owner of the
/// `UblkServer`) that drains CQEs, sends `IoWork` to the **worker** (thread owning the
/// `backend`, the only one touching VRAM/CUDA), and completes via `COMMIT_AND_FETCH` with the
/// returned data. Works with any `BlockBackend` (RAM or VRAM).
pub fn spawn_server_dt3<B: BlockBackend + Send + 'static>(
    char_path: impl AsRef<Path>,
    queue_depth: u16,
    buf_size: usize,
    backend: B,
) -> io::Result<ServerHandleDt3<B>> {
    let char_dev = open_ublk_char_device(char_path)?;
    let server = ramshared_uring::UblkServer::new(char_dev.as_raw_fd(), queue_depth, buf_size)?;

    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(RING_CHAN_CAP);
    let (reply_tx, reply_rx) = mpsc::channel::<WorkerReply>();
    let worker = spawn_ublk_worker(backend, work_rx, reply_tx);

    let ring = thread::spawn(move || {
        // The char device remains open while the ring lives; `work_tx` drops upon returning
        // (terminates the worker).
        let _char_dev = char_dev;
        run_ring_owner(server, queue_depth, buf_size, work_tx, reply_rx)
    });

    Ok(ServerHandleDt3 { ring, worker })
}

fn run_ring_owner<S: QueueServer>(
    mut server: S,
    queue_depth: u16,
    buf_size: usize,
    work_tx: SyncSender<ublk::IoWork>,
    reply_rx: Receiver<WorkerReply>,
) -> io::Result<()> {
    server.submit_initial_fetch()?;

    // Pool of recycled buffers (DT-8): pre-warms `queue_depth` buffers of
    // `buf_size`. Each request takes one from the pool on dispatch and returns it on COMMIT —
    // zero malloc/free in the hot path under steady state. The pool never empties because the number
    // of requests inflight is limited to `queue_depth` (pool.len() + in_flight == qd).
    let mut buf_pool: Vec<Vec<u8>> = (0..queue_depth).map(|_| vec![0u8; buf_size]).collect();

    let mut in_flight = 0u32;
    loop {
        if in_flight > 0 {
            // There is a request inflight: blocks on the worker's reply (no poll/spin).
            match reply_rx.recv() {
                Ok(reply) => {
                    in_flight -= 1;
                    commit_reply(&mut server, reply, &mut buf_pool)?;
                }
                Err(_) => return Err(io::Error::other("worker terminated unexpectedly")),
            }
            // Drains additional replies already ready, without blocking.
            while let Ok(reply) = reply_rx.try_recv() {
                in_flight -= 1;
                commit_reply(&mut server, reply, &mut buf_pool)?;
            }
        } else {
            // Idle: blocks until the next CQE (request served or abort).
            for completion in server.wait_and_drain()? {
                if completion.result == ublk::UBLK_IO_RES_ABORT {
                    return Ok(()); // teardown: STOP/DEL_DEV aborted the FETCH
                }
                if completion.result < 0 {
                    return Err(io::Error::other(format!(
                        "FETCH failed: {}",
                        completion.result
                    )));
                }
                if dispatch_request(&mut server, completion.tag, &work_tx, &mut buf_pool)? {
                    in_flight += 1;
                }
            }
        }
    }
}

/// Copies READ data (if any) to the tag buffer, completes via COMMIT, and
/// returns the buffer to the pool (without dealloc — preserves capacity).
fn commit_reply<S: QueueServer>(
    server: &mut S,
    reply: WorkerReply,
    buf_pool: &mut Vec<Vec<u8>>,
) -> io::Result<()> {
    if reply.is_read && reply.result >= 0 {
        let n = usize::try_from(reply.result).unwrap_or(0);
        let tag_buf = server.buffer_mut(reply.tag)?;
        let n = n.min(reply.buf.len()).min(tag_buf.len());
        tag_buf[..n].copy_from_slice(&reply.buf[..n]);
    }
    server.commit_and_fetch(reply.tag, reply.result)?;
    // Recycles the buffer: clears the len but preserves capacity (no free).
    let mut buf = reply.buf;
    buf.clear();
    buf_pool.push(buf);
    Ok(())
}

/// Reads the io-desc of `tag`, takes a recycled buffer from the pool sized to `len`
/// (copying the WRITE payload from the tag buffer), and sends it to the worker. Returns `true`
/// if work was sent, `false` if it rejected the request (already completed with error; the
/// buffer, if it was taken from the pool, goes back to it).
fn dispatch_request<S: QueueServer>(
    server: &mut S,
    tag: u16,
    work_tx: &SyncSender<ublk::IoWork>,
    buf_pool: &mut Vec<Vec<u8>>,
) -> io::Result<bool> {
    let snapshot = server.io_desc_snapshot(tag)?;
    let iod = ublk::IoDesc::from_ne_bytes(&snapshot)
        .ok_or_else(|| io::Error::other("invalid io-desc snapshot"))?;
    let req = match iod.to_block_request(tag) {
        Ok(req) => req,
        Err(_) => {
            server.commit_and_fetch(tag, -22)?; // EINVAL (no buffer taken from the pool)
            return Ok(false);
        }
    };

    // Takes a recycled buffer and sizes it to `len`. `unwrap_or_default` only allocates during
    // warming (empty pool); in steady state, the pre-warming guarantees one is available.
    let len = req.len as usize;
    let mut buf = buf_pool.pop().unwrap_or_default();
    buf.clear();
    buf.resize(len, 0);

    // WRITE: kernel already copied bio->tag buffer; passes it in the yielded buffer.
    if req.cmd == Command::Write {
        let tag_buf = server.buffer_mut(tag)?;
        if len > tag_buf.len() {
            buf_pool.push(buf); // returns to pool before rejecting
            server.commit_and_fetch(tag, -22)?; // EINVAL
            return Ok(false);
        }
        buf.copy_from_slice(&tag_buf[..len]);
    }

    let work = ublk::IoWork {
        qid: 0,
        tag,
        buffer_addr: 0,
        req,
        payload: buf,
    };
    work_tx
        .send(work)
        .map_err(|_| io::Error::other("worker terminated unexpectedly"))?;
    Ok(true)
}

/// Converts CUDA error to `io::Error` for the worker thread `Result`.
fn cuda_to_io(e: ramshared_cuda::CudaError) -> io::Error {
    io::Error::other(format!("CUDA: {e}"))
}

/// Handle of the DT-3 server served by VRAM (ring owner + worker owning the CUDA stack).
pub struct ServerHandleDt3Vram {
    ring: JoinHandle<io::Result<()>>,
    worker: JoinHandle<io::Result<()>>,
}

impl ServerHandleDt3Vram {
    pub fn join(self) -> io::Result<()> {
        let ring_result = self
            .ring
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("ring owner panicked")));
        let worker_result = self
            .worker
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("vram worker panicked")));
        match ring_result {
            Ok(()) => worker_result,
            Err(error) => Err(error),
        }
    }
}

/// Like [`spawn_server_dt3`], but the worker serves from **VRAM**: it creates the
/// stack `Cuda`/`Context`/`DeviceMem`/`VramBackend` **on its own thread** (the
/// CUDA context has thread affinity and `VramBackend` is not `Send`/`'static`)
/// and runs the loop there. `vram_bytes` is the GPU allocation size; `block_size` the
/// logical block size.
pub fn spawn_server_dt3_vram(
    char_path: impl AsRef<Path>,
    queue_depth: u16,
    buf_size: usize,
    vram_bytes: usize,
    block_size: u32,
) -> io::Result<ServerHandleDt3Vram> {
    let char_dev = open_ublk_char_device(char_path)?;
    let server = ramshared_uring::UblkServer::new(char_dev.as_raw_fd(), queue_depth, buf_size)?;

    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(RING_CHAN_CAP);
    let (reply_tx, reply_rx) = mpsc::channel::<WorkerReply>();

    let worker = thread::spawn(move || -> io::Result<()> {
        // The entire CUDA stack lives in this thread (context affinity).
        let cuda = Cuda::load().map_err(cuda_to_io)?;
        let device = cuda.device(0).map_err(cuda_to_io)?;
        let ctx = cuda.create_context(&device).map_err(cuda_to_io)?;
        let mut mem = ctx.alloc(vram_bytes).map_err(cuda_to_io)?;
        mem.zero().map_err(cuda_to_io)?;
        let mut backend = VramBackend::new(mem, block_size);
        worker_loop(&mut backend, work_rx, reply_tx);
        Ok(())
    });

    let ring = thread::spawn(move || {
        let _char_dev = char_dev;
        run_ring_owner(server, queue_depth, buf_size, work_tx, reply_rx)
    });

    Ok(ServerHandleDt3Vram { ring, worker })
}

#[cfg(test)]
mod join_tests;

#[cfg(test)]
mod residency_tests;
