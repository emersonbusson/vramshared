// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark/worker.rs
//! Volume-confined 32 MiB benchmark worker & bounded child-process supervisor
//! (`SPEC.md` DT-14, DT-16, DT-21).

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use ramshared_exec::bounded_process::configure_process_group;
use ramshared_exec::retained_process::{NativeProcessControl, ProcessControl};
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, StatVfsMountFlags, fstatvfs, fsync, mkdirat, openat, statat,
    unlinkat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::custody::{
    AuditEvent, AuditSink, WorkerLease, WorkerLeaseStatus, open_verified_mount_root,
    prove_process_exited, read_linux_proc_start_time, replace_lease, statx_descriptor,
    validate_regular_file,
};
use super::model::{
    BenchmarkVolume, ROUNDS_PER_VOLUME, SAMPLE_PAYLOAD_BYTES, SEQ_BUFFER_BYTES,
    SEQ_CHUNKS_PER_SAMPLE, SEQ_WRITE_BYTES, SMALL_WRITE_BYTES, SMALL_WRITES_PER_SAMPLE,
    VOLUME_WORKER_DEADLINE, WorkerSampleOutcome, fill_deterministic_page, small_write_slot,
};
use crate::resource_config::{BenchmarkUseCase, InventoryError, unix_millis};

const MANAGED_BENCHMARK_SUBDIRS: &[&str] =
    &["var", "lib", "ramshared", "resource-config", "benchmark"];
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const KILL_REAP_GRACE: Duration = Duration::from_millis(500);

/// Serialized worker instruction sent on stdin (`DT-17`: zero secrets or paths in process argv).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkerRequest {
    pub(crate) tx_id: String,
    pub(crate) round: usize,
    pub(crate) volume: BenchmarkVolume,
    pub(crate) temp_leaf_name: String,
    pub(crate) sample_seed: u64,
    pub(crate) byte_ceiling: u64,
    pub(crate) owner_uid: u32,
}

/// Wire reply emitted by the child worker over stdout.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkerReply {
    Completed(WorkerSampleOutcome),
    Failed {
        detail: String,
        cleanup_verified: bool,
    },
}

/// Result of supervising one bounded sample worker against its exclusive lease.
#[derive(Debug)]
pub(crate) enum WorkerRunOutcome {
    Completed(WorkerSampleOutcome),
    Failed { detail: String },
    WorkerStuck { detail: String },
}

/// Execution context shared with a benchmark sample worker runner.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkerRunContext<'a> {
    pub(crate) state_dir: &'a Path,
    pub(crate) owner_uid: u32,
    pub(crate) current_boot_id: &'a str,
    pub(crate) use_case: BenchmarkUseCase,
}

/// Abstract worker launcher/supervisor so unit tests can inject timeouts or stuck workers.
pub(crate) trait WorkerRunner {
    fn run_sample(
        &mut self,
        request: &WorkerRequest,
        run_ctx: WorkerRunContext<'_>,
        lease: &mut WorkerLease,
        audit: &mut dyn AuditSink,
    ) -> Result<WorkerRunOutcome, InventoryError>;
}

/// Validate a leaf filename for `var/lib/ramshared/resource-config/benchmark/<leaf>`.
pub(crate) fn validate_temp_leaf_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 96
        && name.starts_with("bench-")
        && name.ends_with(".bin")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !valid || name.contains("..") {
        return Err("benchmark temporary filename failed safe lexical validation".into());
    }
    Ok(())
}

/// Compute relative managed path stored inside the worker lease (`DT-21`).
pub(crate) fn relative_temp_path(leaf: &str) -> String {
    format!("var/lib/ramshared/resource-config/benchmark/{leaf}")
}

/// Open or create the volume's confined benchmark temp directory without following symlinks.
fn open_volume_benchmark_dir(
    mount_root: &File,
    create_missing: bool,
    owner_uid: u32,
) -> Result<Option<File>, String> {
    let mut current = mount_root
        .try_clone()
        .map_err(|err| format!("cannot clone verified mount root handle: {err}"))?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    for component in MANAGED_BENCHMARK_SUBDIRS {
        let next = match openat(&current, *component, flags, Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(err) if err == rustix::io::Errno::NOENT && create_missing => {
                mkdirat(&current, *component, Mode::RWXU)
                    .map_err(|e| format!("cannot create benchmark subdir {component}: {e}"))?;
                fsync(&current).map_err(|e| format!("cannot sync benchmark parent dir: {e}"))?;
                File::from(
                    openat(&current, *component, flags, Mode::empty())
                        .map_err(|e| format!("cannot reopen benchmark subdir {component}: {e}"))?,
                )
            }
            Err(err) if err == rustix::io::Errno::NOENT => return Ok(None),
            Err(err) => {
                return Err(format!(
                    "cannot open benchmark subdir {component} without links: {err}"
                ));
            }
        };
        let stat = rustix::fs::fstat(&next)
            .map_err(|e| format!("cannot inspect benchmark subdir {component}: {e}"))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
            || stat.st_uid != owner_uid
            || stat.st_mode & 0o022 != 0
        {
            return Err(format!(
                "managed benchmark directory {component} must be owner-only without group/world write"
            ));
        }
        current = next;
    }
    Ok(Some(current))
}

/// Prove the exact sample temp file is absent, or remove ONLY the matching owned regular file.
pub(crate) fn verify_or_cleanup_volume_temp(
    volume: &BenchmarkVolume,
    leaf: &str,
    expected_identity: Option<(u64, u64)>,
    owner_uid: u32,
) -> Result<(), String> {
    validate_temp_leaf_name(leaf)?;
    let mount_root = open_verified_mount_root(
        &volume.mountpoint,
        volume.mount_id,
        &volume.major_minor,
        owner_uid,
    )?;
    let Some(bench_dir) = open_volume_benchmark_dir(&mount_root, false, owner_uid)? else {
        return Ok(());
    };
    cleanup_exact_leaf(&bench_dir, leaf, expected_identity, owner_uid)
}

fn cleanup_exact_leaf(
    bench_dir: &File,
    leaf: &str,
    expected_identity: Option<(u64, u64)>,
    owner_uid: u32,
) -> Result<(), String> {
    match statat(bench_dir, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Err(err) if err == rustix::io::Errno::NOENT => return Ok(()),
        Err(err) => {
            return Err(format!(
                "cannot inspect benchmark file before cleanup: {err}"
            ));
        }
        Ok(_) => {}
    }
    let handle = File::from(
        openat(
            bench_dir,
            leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|err| format!("cannot open benchmark temp file for cleanup proof: {err}"))?,
    );
    let (dev, ino) = validate_regular_file(&handle, owner_uid, "benchmark sample file")?;
    if let Some((expected_dev, expected_ino)) = expected_identity
        && (dev != expected_dev || ino != expected_ino)
    {
        return Err("benchmark temp file identity changed before cleanup".into());
    }
    drop(handle);

    unlinkat(bench_dir, leaf, AtFlags::empty())
        .map_err(|err| format!("cannot remove benchmark temp file: {err}"))?;
    match statat(bench_dir, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Err(err) if err == rustix::io::Errno::NOENT => {}
        _ => return Err("benchmark temp file stayed visible after unlink".into()),
    }
    fsync(bench_dir).map_err(|err| format!("cannot sync benchmark dir after unlink: {err}"))?;
    Ok(())
}

/// Execute one 32 MiB write + fdatasync + full read-back verify + exact cleanup cycle (DT-14, DT-21).
pub(crate) fn execute_worker_sample(request: &WorkerRequest) -> WorkerReply {
    match execute_worker_sample_inner(request) {
        Ok(outcome) => WorkerReply::Completed(outcome),
        Err((detail, cleanup_verified)) => WorkerReply::Failed {
            detail,
            cleanup_verified,
        },
    }
}

fn execute_worker_sample_inner(
    request: &WorkerRequest,
) -> Result<WorkerSampleOutcome, (String, bool)> {
    validate_temp_leaf_name(&request.temp_leaf_name).map_err(|e| (e, true))?;
    if request.byte_ceiling != SAMPLE_PAYLOAD_BYTES || request.round >= ROUNDS_PER_VOLUME {
        return Err((
            "worker request exceeded 32 MiB ceiling or round bound".into(),
            true,
        ));
    }

    let mount_root = open_verified_mount_root(
        &request.volume.mountpoint,
        request.volume.mount_id,
        &request.volume.major_minor,
        request.owner_uid,
    )
    .map_err(|e| (e, true))?;

    let vfs = fstatvfs(&mount_root).map_err(|e| (format!("fstatvfs failed: {e}"), true))?;
    if vfs.f_flag.contains(StatVfsMountFlags::RDONLY) || vfs.f_frsize == 0 {
        return Err(("target mount became read-only or invalid".into(), true));
    }
    let free = vfs
        .f_bavail
        .checked_mul(vfs.f_frsize)
        .ok_or_else(|| ("mount free space overflow".into(), true))?;
    if free < request.byte_ceiling {
        return Err((
            format!(
                "mount has {free} bytes left, below {} sample ceiling",
                request.byte_ceiling
            ),
            true,
        ));
    }

    let bench_dir = open_volume_benchmark_dir(&mount_root, true, request.owner_uid)
        .map_err(|e| (e, true))?
        .ok_or_else(|| {
            (
                "benchmark directory did not open after creation".into(),
                true,
            )
        })?;

    let mut file = File::from(
        openat(
            &bench_dir,
            request.temp_leaf_name.as_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|e| {
            (
                format!("cannot exclusively create benchmark sample file: {e}"),
                true,
            )
        })?,
    );
    let (file_dev, file_ino) =
        validate_regular_file(&file, request.owner_uid, "created benchmark sample file")
            .map_err(|e| (e, false))?;
    let stat = statx_descriptor(&file).map_err(|e| (e, false))?;
    if stat.stx_mnt_id != request.volume.mount_id {
        let cleaned = cleanup_exact_leaf(
            &bench_dir,
            &request.temp_leaf_name,
            Some((file_dev, file_ino)),
            request.owner_uid,
        )
        .is_ok();
        return Err(("sample file crossed mount boundary".into(), cleaned));
    }

    let payload_res = run_32mib_io_and_verify(
        &mut file,
        request.sample_seed,
        file_dev,
        file_ino,
        request.volume.mount_id,
    );
    drop(file);

    let cleanup_ok = cleanup_exact_leaf(
        &bench_dir,
        &request.temp_leaf_name,
        Some((file_dev, file_ino)),
        request.owner_uid,
    );

    match (payload_res, cleanup_ok) {
        (Ok(mut outcome), Ok(())) => {
            outcome.cleanup_verified = true;
            Ok(outcome)
        }
        (Ok(_), Err(cleanup_err)) => Err((format!("cleanup proof failed: {cleanup_err}"), false)),
        (Err(io_err), Ok(())) => Err((io_err, true)),
        (Err(io_err), Err(cleanup_err)) => Err((
            format!("{io_err}; cleanup also failed: {cleanup_err}"),
            false,
        )),
    }
}

fn run_32mib_io_and_verify(
    file: &mut File,
    seed: u64,
    expected_dev: u64,
    expected_ino: u64,
    expected_mount_id: u64,
) -> Result<WorkerSampleOutcome, String> {
    let mut fixed_buf = vec![0u8; SEQ_BUFFER_BYTES];
    let mut expected_hasher = Sha256::new();
    let mut bytes_written = 0u64;

    let seq_start = Instant::now();
    for chunk_idx in 0..SEQ_CHUNKS_PER_SAMPLE {
        for slot in 0..SMALL_WRITES_PER_SAMPLE {
            let page_idx = (chunk_idx * SMALL_WRITES_PER_SAMPLE + slot) as u32;
            let start = slot * SMALL_WRITE_BYTES;
            fill_deterministic_page(
                seed,
                page_idx,
                &mut fixed_buf[start..start + SMALL_WRITE_BYTES],
            );
        }
        expected_hasher.update(&fixed_buf);
        file.write_all(&fixed_buf)
            .map_err(|e| format!("sequential write failed: {e}"))?;
        bytes_written = bytes_written
            .checked_add(SEQ_BUFFER_BYTES as u64)
            .ok_or("byte counter overflow")?;
    }
    file.sync_data()
        .map_err(|e| format!("sequential fdatasync failed: {e}"))?;
    let seq_elapsed_ns = seq_start
        .elapsed()
        .as_nanos()
        .max(1)
        .min(u128::from(u64::MAX)) as u64;

    let mut small_latencies_ns = Vec::with_capacity(SMALL_WRITES_PER_SAMPLE);
    let tail_base_page = (SEQ_CHUNKS_PER_SAMPLE * SMALL_WRITES_PER_SAMPLE) as u32;
    for step in 0..SMALL_WRITES_PER_SAMPLE {
        let slot = small_write_slot(step);
        let page_idx = tail_base_page + slot as u32;
        fill_deterministic_page(seed, page_idx, &mut fixed_buf[..SMALL_WRITE_BYTES]);
        let offset = SEQ_WRITE_BYTES + (slot as u64) * (SMALL_WRITE_BYTES as u64);
        let t0 = Instant::now();
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("small-write seek failed: {e}"))?;
        file.write_all(&fixed_buf[..SMALL_WRITE_BYTES])
            .map_err(|e| format!("small write failed: {e}"))?;
        file.sync_data()
            .map_err(|e| format!("small-write fdatasync failed: {e}"))?;
        small_latencies_ns.push(t0.elapsed().as_nanos().max(1).min(u128::from(u64::MAX)) as u64);
        bytes_written = bytes_written
            .checked_add(SMALL_WRITE_BYTES as u64)
            .ok_or("byte counter overflow")?;
    }

    for slot in 0..SMALL_WRITES_PER_SAMPLE {
        fill_deterministic_page(
            seed,
            tail_base_page + slot as u32,
            &mut fixed_buf[..SMALL_WRITE_BYTES],
        );
        expected_hasher.update(&fixed_buf[..SMALL_WRITE_BYTES]);
    }
    let expected_sha256 = hex_digest(&expected_hasher.finalize());

    let meta = file
        .metadata()
        .map_err(|e| format!("metadata check failed: {e}"))?;
    let stat = statx_descriptor(file)?;
    if meta.len() != SAMPLE_PAYLOAD_BYTES
        || meta.dev() != expected_dev
        || meta.ino() != expected_ino
        || stat.stx_mnt_id != expected_mount_id
    {
        return Err("sample file geometry or identity drifted before read-back".into());
    }

    let observed_sha256 = verify_readback_32mib(file, seed, &mut fixed_buf)?;
    if observed_sha256 != expected_sha256 {
        return Err("benchmark read-back SHA-256 verification failed".into());
    }

    Ok(WorkerSampleOutcome {
        bytes_written,
        seq_bytes_written: SEQ_WRITE_BYTES,
        small_write_count: SMALL_WRITES_PER_SAMPLE,
        seq_elapsed_ns,
        small_write_latencies_ns: small_latencies_ns,
        expected_sha256,
        observed_sha256,
        file_dev: expected_dev,
        file_ino: expected_ino,
        cleanup_verified: false,
    })
}

fn verify_readback_32mib(
    file: &mut File,
    seed: u64,
    read_buf: &mut [u8],
) -> Result<String, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("read-back seek failed: {e}"))?;
    let mut hasher = Sha256::new();
    let mut expected_page = [0u8; SMALL_WRITE_BYTES];
    let total_chunks = SEQ_CHUNKS_PER_SAMPLE + 1;
    let mut total_read = 0u64;

    for chunk_idx in 0..total_chunks {
        file.read_exact(read_buf)
            .map_err(|e| format!("read-back chunk {chunk_idx} failed: {e}"))?;
        hasher.update(&*read_buf);
        total_read += read_buf.len() as u64;
        for slot in 0..SMALL_WRITES_PER_SAMPLE {
            let page_idx = (chunk_idx * SMALL_WRITES_PER_SAMPLE + slot) as u32;
            fill_deterministic_page(seed, page_idx, &mut expected_page);
            let start = slot * SMALL_WRITE_BYTES;
            if read_buf[start..start + SMALL_WRITE_BYTES] != expected_page {
                return Err(format!("read-back byte mismatch at page {page_idx}"));
            }
        }
    }
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing).map_err(|e| e.to_string())? != 0
        || total_read != SAMPLE_PAYLOAD_BYTES
    {
        return Err("sample file length exceeded 32 MiB ceiling on read-back".into());
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Entrypoint invoked by `ramshared __config-benchmark-worker` child process.
pub(crate) fn run_worker_from_io(
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
) -> Result<(), String> {
    let mut reader = BufReader::new(stdin);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|err| format!("cannot read worker request: {err}"))?;
    let request: WorkerRequest = serde_json::from_str(request_line.trim())
        .map_err(|err| format!("worker request JSON is invalid: {err}"))?;

    writeln!(stdout, "READY").map_err(|err| format!("cannot write READY handshake: {err}"))?;
    stdout
        .flush()
        .map_err(|err| format!("cannot flush READY handshake: {err}"))?;

    let mut gate_line = String::new();
    reader
        .read_line(&mut gate_line)
        .map_err(|err| format!("cannot read parent GO signal: {err}"))?;
    if gate_line.trim() != "GO" {
        return Err("worker aborted before parent GO lease commit".into());
    }

    let reply = execute_worker_sample(&request);
    let encoded = serde_json::to_string(&reply)
        .map_err(|err| format!("cannot serialize worker reply: {err}"))?;
    writeln!(stdout, "{encoded}").map_err(|err| format!("cannot write worker reply: {err}"))?;
    stdout
        .flush()
        .map_err(|err| format!("cannot flush worker reply: {err}"))?;
    Ok(())
}

/// Real OS subprocess supervisor enforcing pre-write child lease pinning and 120s timeout (`DT-21`).
pub(crate) struct SubprocessWorkerRunner {
    executable: std::path::PathBuf,
    args: Vec<String>,
    env_vars: Vec<(String, String)>,
    deadline: Duration,
    process_control: Box<dyn ProcessControl>,
}

impl SubprocessWorkerRunner {
    pub(crate) fn native() -> Result<Self, InventoryError> {
        let exe = std::env::current_exe()
            .map_err(|err| InventoryError(format!("cannot resolve current executable: {err}")))?;
        Ok(Self {
            executable: exe,
            args: vec!["__config-benchmark-worker".into()],
            env_vars: Vec::new(),
            deadline: VOLUME_WORKER_DEADLINE,
            process_control: Box::new(NativeProcessControl),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_command(
        executable: std::path::PathBuf,
        args: Vec<String>,
        env_vars: Vec<(String, String)>,
        deadline: Duration,
        process_control: Box<dyn ProcessControl>,
    ) -> Self {
        Self {
            executable,
            args,
            env_vars,
            deadline,
            process_control,
        }
    }
}

impl WorkerRunner for SubprocessWorkerRunner {
    fn run_sample(
        &mut self,
        request: &WorkerRequest,
        run_ctx: WorkerRunContext<'_>,
        lease: &mut WorkerLease,
        audit: &mut dyn AuditSink,
    ) -> Result<WorkerRunOutcome, InventoryError> {
        let mut cmd = Command::new(&self.executable);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in &self.env_vars {
            cmd.env(k, v);
        }
        configure_process_group(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|err| InventoryError(format!("cannot spawn benchmark worker: {err}")))?;
        let child_pid = child.id();
        lease.pid = child_pid;
        lease.boot_id = run_ctx.current_boot_id.to_owned();
        let mut fail_ctx = WorkerFailCtx {
            process_control: &*self.process_control,
            lease,
            run_ctx,
            audit,
            request,
        };
        match read_linux_proc_start_time(child_pid) {
            Ok(start_time) => fail_ctx.lease.process_start_time = start_time,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fail_ctx.lease.process_start_time = 0;
            }
            Err(err) => {
                drop(child.stdin.take());
                return terminate_and_fail(
                    &mut fail_ctx,
                    &mut child,
                    &format!("cannot bind worker start identity for pid {child_pid}: {err}"),
                );
            }
        }

        let Some(mut child_stdin) = child.stdin.take() else {
            return terminate_and_fail(&mut fail_ctx, &mut child, "worker stdin missing");
        };
        let Some(child_stdout) = child.stdout.take() else {
            drop(child_stdin);
            return terminate_and_fail(&mut fail_ctx, &mut child, "worker stdout missing");
        };

        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<String, String>>(1);
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<String, String>>(1);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(child_stdout);
            let mut line = String::new();
            let ready_res = reader
                .read_line(&mut line)
                .map(|_| line.clone())
                .map_err(|e| e.to_string());
            let _ = ready_tx.send(ready_res);
            line.clear();
            let reply_res = reader
                .read_line(&mut line)
                .map(|_| line)
                .map_err(|e| e.to_string());
            let _ = reply_tx.send(reply_res);
        });

        let encoded_req = serde_json::to_string(request)
            .map_err(|err| InventoryError(format!("cannot serialize worker request: {err}")))?;
        if let Err(err) = writeln!(child_stdin, "{encoded_req}").and_then(|()| child_stdin.flush())
        {
            drop(child_stdin);
            return terminate_and_fail(
                &mut fail_ctx,
                &mut child,
                &format!("cannot deliver worker request: {err}"),
            );
        }

        let deadline = Instant::now() + self.deadline;
        let ready_wait = deadline.saturating_duration_since(Instant::now());
        match ready_rx.recv_timeout(ready_wait) {
            Ok(Ok(line)) if line.trim() == "READY" => {}
            Ok(Ok(_)) | Ok(Err(_)) => {
                drop(child_stdin);
                return terminate_and_fail(
                    &mut fail_ctx,
                    &mut child,
                    "benchmark worker failed READY handshake before disk write",
                );
            }
            Err(_) => {
                drop(child_stdin);
                return terminate_and_fail(
                    &mut fail_ctx,
                    &mut child,
                    "benchmark worker exceeded 120s volume deadline before READY",
                );
            }
        }

        fail_ctx.lease.status = WorkerLeaseStatus::Active;
        replace_lease(
            fail_ctx.run_ctx.state_dir,
            fail_ctx.run_ctx.owner_uid,
            fail_ctx.lease,
        )?;

        if let Err(err) = writeln!(child_stdin, "GO").and_then(|()| child_stdin.flush()) {
            drop(child_stdin);
            return terminate_and_fail(
                &mut fail_ctx,
                &mut child,
                &format!("cannot deliver GO confirmation to worker: {err}"),
            );
        }
        drop(child_stdin);

        loop {
            match self.process_control.observe_exit(&mut child) {
                Ok(true) => {
                    let status = self.process_control.reap(&mut child).map_err(|err| {
                        InventoryError(format!("cannot reap finished benchmark worker: {err}"))
                    })?;
                    let reply_line = reply_rx
                        .recv_timeout(Duration::from_millis(250))
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_default();
                    if !status.is_some_and(|s| s.success()) && reply_line.trim().is_empty() {
                        return handle_worker_failure_cleanup(
                            &mut fail_ctx,
                            "benchmark worker exited abnormally without reply",
                        );
                    }
                    let reply: WorkerReply =
                        serde_json::from_str(reply_line.trim()).map_err(|e| {
                            InventoryError(format!("malformed reply from benchmark worker: {e}"))
                        })?;
                    return Ok(match reply {
                        WorkerReply::Completed(outcome) => WorkerRunOutcome::Completed(outcome),
                        WorkerReply::Failed { detail, .. } => WorkerRunOutcome::Failed { detail },
                    });
                }
                Ok(false) if Instant::now() < deadline => sleep(POLL_INTERVAL),
                Ok(false) => {
                    return terminate_and_fail(
                        &mut fail_ctx,
                        &mut child,
                        "benchmark worker exceeded 120s volume deadline",
                    );
                }
                Err(err) => {
                    return terminate_and_fail(
                        &mut fail_ctx,
                        &mut child,
                        &format!("cannot observe benchmark worker exit: {err}"),
                    );
                }
            }
        }
    }
}

struct WorkerFailCtx<'a> {
    process_control: &'a dyn ProcessControl,
    lease: &'a mut WorkerLease,
    run_ctx: WorkerRunContext<'a>,
    audit: &'a mut dyn AuditSink,
    request: &'a WorkerRequest,
}

fn terminate_and_fail(
    ctx: &mut WorkerFailCtx<'_>,
    child: &mut Child,
    reason: &str,
) -> Result<WorkerRunOutcome, InventoryError> {
    let stop_err = ctx.process_control.stop_group(child).err();
    let grace_deadline = Instant::now() + KILL_REAP_GRACE;
    let mut reaped = false;
    while Instant::now() < grace_deadline {
        if let Ok(Some(_)) = ctx.process_control.reap(child) {
            reaped = true;
            break;
        }
        sleep(POLL_INTERVAL);
    }
    if !reaped || stop_err.is_some() {
        return persist_stuck(ctx, reason);
    }
    handle_worker_failure_cleanup(ctx, reason)
}

fn handle_worker_failure_cleanup(
    ctx: &mut WorkerFailCtx<'_>,
    reason: &str,
) -> Result<WorkerRunOutcome, InventoryError> {
    if prove_process_exited(
        ctx.lease,
        ctx.run_ctx.current_boot_id,
        &read_linux_proc_start_time,
    )
    .is_err()
    {
        return persist_stuck(ctx, reason);
    }
    if let Err(clean_err) = verify_or_cleanup_volume_temp(
        &ctx.request.volume,
        &ctx.request.temp_leaf_name,
        None,
        ctx.run_ctx.owner_uid,
    ) {
        return persist_stuck(
            ctx,
            &format!("{reason}; cleanup verification failed: {clean_err}"),
        );
    }
    Ok(WorkerRunOutcome::Failed {
        detail: reason.to_owned(),
    })
}

fn persist_stuck(
    ctx: &mut WorkerFailCtx<'_>,
    reason: &str,
) -> Result<WorkerRunOutcome, InventoryError> {
    ctx.lease.status = WorkerLeaseStatus::WorkerStuck;
    ctx.lease.detail = Some(reason.to_owned());
    replace_lease(ctx.run_ctx.state_dir, ctx.run_ctx.owner_uid, ctx.lease)?;
    let _ = ctx.audit.record_event(&AuditEvent {
        schema_version: 1,
        tx_id: ctx.lease.tx_id.clone(),
        timestamp_unix_ms: unix_millis(),
        action: "benchmark_worker_stuck".into(),
        use_case: ctx.run_ctx.use_case,
        plan_hash: ctx.lease.plan_hash.clone(),
        target_identities: vec![ctx.request.volume.stable_id()],
        source_sample_timestamps: vec![ctx.request.volume.capacity_observed_unix_ms],
        per_volume_cap_bytes: ctx.request.byte_ceiling,
        run_cap_bytes: ctx.request.byte_ceiling,
        consent: "worker_timeout".into(),
        result: "worker_stuck".into(),
        round: Some(ctx.request.round),
        bytes_written: None,
        readback_sha256: None,
        cleanup_verified: Some(false),
        pending_recovery: Some(format!("{}::{}", ctx.lease.temp_relative_path, reason)),
    });
    Ok(WorkerRunOutcome::WorkerStuck {
        detail: format!("worker_stuck: {reason}"),
    })
}
