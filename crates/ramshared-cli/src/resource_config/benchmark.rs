// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark.rs
//! Orchestration, CLI facade, health probes, and reporting for
//! `ramshared config benchmark` (`SPEC.md` DT-14..DT-19, DT-21).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

pub(crate) mod custody;
pub(crate) mod model;
pub(crate) mod windows_host;
pub(crate) mod worker;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests;

use custody::{
    AuditEvent, AuditSink, FileAuditSink, NATIVE_STATE_DIR, ProcessIdentity,
    acquire_launching_lease, clear_verified_lease, initial_launching_lease, read_existing_lease,
    read_linux_proc_start_time, recover_lease,
};
use model::{
    BenchmarkPlan, BenchmarkRecommendation, BenchmarkVolume, PER_VOLUME_WRITE_CAP_BYTES,
    ROUNDS_PER_VOLUME, RuntimeHealthSnapshot, SAMPLE_PAYLOAD_BYTES, VolumeBenchmarkResult,
    WorkerSampleOutcome, build_benchmark_plan, recommend_leader, revalidate_volume, sample_seed,
    summarize_volume,
};
use worker::{
    SubprocessWorkerRunner, WorkerRequest, WorkerRunContext, WorkerRunOutcome, WorkerRunner,
    relative_temp_path, run_worker_from_io, verify_or_cleanup_volume_temp,
};

use super::{InventoryError, ResourceSnapshot, collect_snapshot, load_profile_text, unix_millis};

/// Operator-selected workload optimization target for benchmark ranking (`DT-15`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BenchmarkUseCase {
    Swap,
    Origin,
}

/// Requested action for `ramshared config benchmark`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BenchmarkAction {
    Plan,
    InteractiveOrConfirm,
    Confirmed,
    Recover,
}

/// Parsed CLI options for `ramshared config benchmark`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BenchmarkRequest {
    pub(crate) action: BenchmarkAction,
    pub(crate) use_case: BenchmarkUseCase,
    pub(crate) json: bool,
    pub(crate) profile_path: Option<String>,
    pub(crate) batch: Vec<String>,
}

/// Completed benchmark comparison report (`DT-15`, `DT-21`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct BenchmarkReport {
    pub(crate) schema_version: u32,
    pub(crate) tx_id: String,
    pub(crate) plan_hash: String,
    pub(crate) platform: &'static str,
    pub(crate) use_case: BenchmarkUseCase,
    pub(crate) rounds: usize,
    pub(crate) volumes: Vec<VolumeBenchmarkResult>,
    pub(crate) recommendation: BenchmarkRecommendation,
    pub(crate) writes_performed: bool,
    pub(crate) cleanup_verified: bool,
    pub(crate) status: &'static str,
}

/// Recovery report returned by `ramshared config benchmark --recover`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct BenchmarkRecoveryReport {
    pub(crate) schema_version: u32,
    pub(crate) recovered: bool,
    pub(crate) previous_tx_id: Option<String>,
    pub(crate) previous_status: Option<String>,
    pub(crate) detail: String,
}

/// Profile-reading callback type used by [`BenchmarkEnvironment`].
pub(crate) type ProfileLoader<'a> = Box<dyn FnMut(&str) -> Result<String, InventoryError> + 'a>;

/// Dependency-injected runtime boundaries for hermetic tests and live Linux execution.
pub(crate) struct BenchmarkEnvironment<'a> {
    pub(crate) owner_uid: u32,
    pub(crate) state_dir: PathBuf,
    pub(crate) collect_snapshot: Box<dyn FnMut() -> Result<ResourceSnapshot, InventoryError> + 'a>,
    pub(crate) probe_health: Box<dyn FnMut() -> RuntimeHealthSnapshot + 'a>,
    pub(crate) load_profile: ProfileLoader<'a>,
    pub(crate) now_unix_ms: Box<dyn FnMut() -> u64 + 'a>,
    pub(crate) read_boot_id: Box<dyn FnMut() -> Result<String, String> + 'a>,
    pub(crate) read_proc_start_time: Box<dyn Fn(u32) -> io::Result<u64> + 'a>,
    pub(crate) audit: Box<dyn AuditSink + 'a>,
    pub(crate) worker_runner: Box<dyn WorkerRunner + 'a>,
    pub(crate) is_tty: bool,
    pub(crate) consent_reader: Box<dyn BufRead + 'a>,
}

impl BenchmarkEnvironment<'static> {
    /// Construct production Linux boundaries over sysfs/procfs and `/var/lib/ramshared/resource-config`.
    pub(crate) fn native() -> Result<Self, InventoryError> {
        let owner_uid = rustix::process::geteuid().as_raw();
        let state_dir = PathBuf::from(NATIVE_STATE_DIR);
        let worker_runner = SubprocessWorkerRunner::native()?;
        Ok(Self {
            owner_uid,
            state_dir: state_dir.clone(),
            collect_snapshot: Box::new(collect_snapshot),
            probe_health: Box::new(probe_native_runtime_health),
            load_profile: Box::new(|path| {
                load_profile_text(std::path::Path::new(path), false)?
                    .ok_or_else(|| InventoryError("requested profile file does not exist".into()))
            }),
            now_unix_ms: Box::new(unix_millis),
            read_boot_id: Box::new(crate::workload::current_boot_id),
            read_proc_start_time: Box::new(read_linux_proc_start_time),
            audit: Box::new(FileAuditSink::new(state_dir, owner_uid)),
            worker_runner: Box::new(worker_runner),
            is_tty: io::stdin().is_terminal() && io::stdout().is_terminal(),
            consent_reader: Box::new(BufReader::new(io::stdin())),
        })
    }
}

/// Probe live host health cleanly using existing cascade and supervisor authorities (`DT-16`).
pub(crate) fn probe_native_runtime_health() -> RuntimeHealthSnapshot {
    let ramshared_off = matches!(crate::cascade::preparation_is_off(), Ok(true));
    let pressure_text = std::fs::read_to_string("/proc/pressure/memory").ok();
    let psi_full_avg10 = pressure_text
        .as_deref()
        .and_then(crate::supervisor::parse_psi_full_avg10);
    let telemetry_detail = if pressure_text.is_none() {
        Some("cannot read /proc/pressure/memory".into())
    } else if psi_full_avg10.is_none() {
        Some("malformed /proc/pressure/memory reading".into())
    } else if !ramshared_off && crate::cascade::preparation_is_off().is_err() {
        Some("cascade swap state cannot be read".into())
    } else {
        None
    };
    RuntimeHealthSnapshot {
        ramshared_off,
        psi_full_avg10,
        telemetry_healthy: telemetry_detail.is_none(),
        telemetry_detail,
    }
}

/// Execute `ramshared config benchmark` against live native boundaries.
pub(crate) fn run(request: BenchmarkRequest, stdout: &mut dyn Write) -> Result<(), InventoryError> {
    let mut env = BenchmarkEnvironment::native()?;
    let snapshot = (env.collect_snapshot)()?;
    if snapshot.platform == crate::resource_config::RuntimePlatform::Wsl2 {
        return windows_host::run_wsl2_benchmark(&snapshot, &request, stdout);
    }
    run_with_env(&request, &mut env, stdout)
}

/// Hidden internal worker entrypoint (`ramshared __config-benchmark-worker`).
pub(crate) fn run_worker_from_stdin() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdin_lock = stdin.lock();
    let mut stdout_lock = stdout.lock();
    match run_worker_from_io(&mut stdin_lock, &mut stdout_lock) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let _ = writeln!(io::stderr(), "benchmark worker failed: {err}");
            ExitCode::from(1)
        }
    }
}

/// Core orchestrator executed with either live or injected test boundaries.
pub(crate) fn run_with_env(
    request: &BenchmarkRequest,
    env: &mut BenchmarkEnvironment<'_>,
    stdout: &mut dyn Write,
) -> Result<(), InventoryError> {
    if request.action == BenchmarkAction::Recover {
        return run_recovery(request, env, stdout);
    }

    let existing_lease =
        read_existing_lease(&env.state_dir, env.owner_uid).map_err(InventoryError)?;
    if request.action != BenchmarkAction::Plan
        && let Some(lease) = existing_lease
    {
        return Err(InventoryError(format!(
            "cannot start benchmark while existing lease is present (status={:?}, tx={}); run `ramshared config benchmark --recover` after verifying worker exit",
            lease.status, lease.tx_id
        )));
    }

    let snapshot = (env.collect_snapshot)()?;
    let health = (env.probe_health)();
    let profile_text = request
        .profile_path
        .as_deref()
        .map(|path| (env.load_profile)(path))
        .transpose()?;
    let now = (env.now_unix_ms)();
    let plan = build_benchmark_plan(&snapshot, request, profile_text.as_deref(), &health, now)?;

    if request.action == BenchmarkAction::Plan {
        if existing_lease.is_some() {
            return Err(InventoryError(
                "existing benchmark worker lease present; run `ramshared config benchmark --recover` before launching a new benchmark"
                    .into(),
            ));
        }
        return write_plan_output(&plan, request.json, stdout);
    }

    let consent = obtain_consent(request, &plan, env, stdout)?;
    let tx_id = generate_tx_id()?;
    let target_identities: Vec<String> = plan
        .volumes
        .iter()
        .map(BenchmarkVolume::stable_id)
        .collect();
    let sample_timestamps: Vec<u64> = plan
        .volumes
        .iter()
        .map(|v| v.capacity_observed_unix_ms)
        .collect();

    env.audit
        .record_event(&AuditEvent {
            schema_version: 1,
            tx_id: tx_id.clone(),
            timestamp_unix_ms: (env.now_unix_ms)(),
            action: "benchmark_intent".into(),
            use_case: request.use_case,
            plan_hash: plan.plan_hash.clone(),
            target_identities: target_identities.clone(),
            source_sample_timestamps: sample_timestamps.clone(),
            per_volume_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
            run_cap_bytes: plan.max_total_write_bytes,
            consent: consent.into(),
            result: "intent_recorded".into(),
            round: None,
            bytes_written: None,
            readback_sha256: None,
            cleanup_verified: None,
            pending_recovery: None,
        })
        .map_err(|err| {
            InventoryError(format!(
                "refusing benchmark write: cannot verify durable audit intent: {err}"
            ))
        })?;

    let report = execute_rounds(request, &plan, &tx_id, consent, env)?;
    windows_host::try_persist_latest_report(&report);
    write_report_output(&report, request.json, stdout)
}

fn run_recovery(
    request: &BenchmarkRequest,
    env: &mut BenchmarkEnvironment<'_>,
    stdout: &mut dyn Write,
) -> Result<(), InventoryError> {
    let boot_id = (env.read_boot_id)().map_err(InventoryError)?;
    let owner_uid = env.owner_uid;
    let recovered = recover_lease(
        &env.state_dir,
        owner_uid,
        &boot_id,
        &*env.read_proc_start_time,
        |lease| {
            let leaf = PathBuf::from(&lease.temp_relative_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let vol = BenchmarkVolume {
                filesystem_uuid: lease.volume_filesystem_uuid.clone(),
                device_identity: lease.volume_device_identity.clone(),
                display_name: lease.mountpoint.clone(),
                mountpoint: lease.mountpoint.clone(),
                mount_id: lease.mount_id,
                major_minor: lease.major_minor.clone(),
                filesystem: "ext4".into(),
                free_bytes: 0,
                required_free_bytes: 0,
                capacity_observed_unix_ms: 0,
            };
            let identity = lease.file_dev.zip(lease.file_ino);
            verify_or_cleanup_volume_temp(&vol, &leaf, identity, owner_uid)
        },
    )?;

    let report = match recovered {
        Some(lease) => {
            let status_label = format!("{:?}", lease.status).to_ascii_lowercase();
            let _ = env.audit.record_event(&AuditEvent {
                schema_version: 1,
                tx_id: lease.tx_id.clone(),
                timestamp_unix_ms: (env.now_unix_ms)(),
                action: "benchmark_recover".into(),
                use_case: request.use_case,
                plan_hash: lease.plan_hash,
                target_identities: vec![format!(
                    "{}@{}",
                    lease.volume_filesystem_uuid, lease.volume_device_identity
                )],
                source_sample_timestamps: Vec::new(),
                per_volume_cap_bytes: lease.byte_ceiling,
                run_cap_bytes: lease.byte_ceiling,
                consent: "attended_recover".into(),
                result: "recovered".into(),
                round: None,
                bytes_written: None,
                readback_sha256: None,
                cleanup_verified: Some(true),
                pending_recovery: None,
            });
            BenchmarkRecoveryReport {
                schema_version: 1,
                recovered: true,
                previous_tx_id: Some(lease.tx_id),
                previous_status: Some(status_label),
                detail: "verified worker exit and exact temporary file absence; lease cleared"
                    .into(),
            }
        }
        None => BenchmarkRecoveryReport {
            schema_version: 1,
            recovered: false,
            previous_tx_id: None,
            previous_status: None,
            detail: "no active or stuck benchmark worker lease present".into(),
        },
    };
    if request.json {
        let json = serde_json::to_string(&report)
            .map_err(|err| InventoryError(format!("cannot serialize recovery report: {err}")))?;
        writeln!(stdout, "{json}")
            .map_err(|err| InventoryError(format!("cannot write recovery output: {err}")))
    } else {
        writeln!(
            stdout,
            "Benchmark recovery (recovered={}): {}",
            report.recovered, report.detail
        )
        .map_err(|err| InventoryError(format!("cannot write recovery output: {err}")))
    }
}

fn execute_rounds(
    request: &BenchmarkRequest,
    plan: &BenchmarkPlan,
    tx_id: &str,
    consent: &str,
    env: &mut BenchmarkEnvironment<'_>,
) -> Result<BenchmarkReport, InventoryError> {
    let boot_id = (env.read_boot_id)().map_err(InventoryError)?;
    let parent_pid = std::process::id();
    let parent_start_time = (env.read_proc_start_time)(parent_pid)
        .map_err(|err| InventoryError(format!("cannot bind supervisor start time: {err}")))?;

    let mut samples_by_vol: HashMap<String, Vec<WorkerSampleOutcome>> = HashMap::new();
    let mut vol_index_by_id: HashMap<String, (usize, BenchmarkVolume)> = HashMap::new();
    for (idx, vol) in plan.volumes.iter().enumerate() {
        vol_index_by_id.insert(vol.stable_id(), (idx, vol.clone()));
    }

    let total_steps = plan
        .round_schedule
        .iter()
        .map(Vec::len)
        .sum::<usize>()
        .max(1) as u32;
    let total_bytes = plan
        .round_schedule
        .iter()
        .map(Vec::len)
        .sum::<usize>()
        .saturating_mul(SAMPLE_PAYLOAD_BYTES as usize) as u64;
    let progress_guard = (env.state_dir == std::path::Path::new(NATIVE_STATE_DIR)).then(|| {
        crate::operation_progress::OperationProgressGuard::start(
            crate::operation_progress::OperationKind::StorageBenchmark,
            "benchmark_volume",
            total_steps,
        )
    });
    let mut completed_bytes: u64 = 0;
    let mut current_step: u32 = 0;

    for (round, order) in plan.round_schedule.iter().enumerate() {
        for stable_id in order {
            current_step = current_step.saturating_add(1).min(total_steps);
            let Some((vol_idx, expected_vol)) = vol_index_by_id.get(stable_id) else {
                return Err(InventoryError(
                    "round schedule referenced unknown volume".into(),
                ));
            };
            if let Some(guard) = progress_guard.as_ref() {
                guard.update(crate::operation_progress::ProgressUpdate {
                    phase: "benchmark_volume",
                    step_index: current_step,
                    target: Some(&expected_vol.display_name),
                    completed_bytes,
                    total_bytes,
                    throughput_mib_per_s: None,
                });
            }
            let current_snapshot = (env.collect_snapshot)()?;
            let current_health = (env.probe_health)();
            let now = (env.now_unix_ms)();
            let (_mount, fresh_vol) =
                revalidate_volume(&current_snapshot, expected_vol, &current_health, now)
                    .inspect_err(|err| {
                        record_abort(
                            env,
                            AbortAuditContext {
                                tx_id,
                                plan,
                                use_case: request.use_case,
                                consent,
                                round,
                                stable_id,
                                detail: &err.0,
                            },
                        );
                    })?;

            let leaf = format!(
                "bench-{}-r{round}-v{vol_idx}.bin",
                tx_id.get(..12).unwrap_or(tx_id)
            );
            let rel_path = relative_temp_path(&leaf);
            let mut lease = initial_launching_lease(
                tx_id,
                &plan.plan_hash,
                &fresh_vol,
                &rel_path,
                ProcessIdentity {
                    boot_id: &boot_id,
                    pid: parent_pid,
                    start_time: parent_start_time,
                },
                now,
            );
            acquire_launching_lease(&env.state_dir, env.owner_uid, &lease)?;

            let worker_req = WorkerRequest {
                tx_id: tx_id.to_owned(),
                round,
                volume: fresh_vol.clone(),
                temp_leaf_name: leaf.clone(),
                sample_seed: sample_seed(tx_id, round, stable_id),
                byte_ceiling: SAMPLE_PAYLOAD_BYTES,
                owner_uid: env.owner_uid,
            };

            let outcome = env.worker_runner.run_sample(
                &worker_req,
                WorkerRunContext {
                    state_dir: &env.state_dir,
                    owner_uid: env.owner_uid,
                    current_boot_id: &boot_id,
                    use_case: request.use_case,
                },
                &mut lease,
                &mut *env.audit,
            )?;

            match outcome {
                WorkerRunOutcome::WorkerStuck { detail } => {
                    return Err(InventoryError(detail));
                }
                WorkerRunOutcome::Failed { detail } => {
                    clear_verified_lease(&env.state_dir, env.owner_uid, tx_id)?;
                    record_abort(
                        env,
                        AbortAuditContext {
                            tx_id,
                            plan,
                            use_case: request.use_case,
                            consent,
                            round,
                            stable_id,
                            detail: &detail,
                        },
                    );
                    return Err(InventoryError(format!(
                        "benchmark failed at round {round} on {stable_id}; run aborted with no ranking: {detail}"
                    )));
                }
                WorkerRunOutcome::Completed(sample) => {
                    if !sample.cleanup_verified {
                        return Err(InventoryError(
                            "manual_recovery_required: sample worker did not verify cleanup".into(),
                        ));
                    }
                    if let Err(audit_err) = env.audit.record_event(&AuditEvent {
                        schema_version: 1,
                        tx_id: tx_id.to_owned(),
                        timestamp_unix_ms: (env.now_unix_ms)(),
                        action: "benchmark_sample_result".into(),
                        use_case: request.use_case,
                        plan_hash: plan.plan_hash.clone(),
                        target_identities: vec![stable_id.clone()],
                        source_sample_timestamps: vec![fresh_vol.capacity_observed_unix_ms],
                        per_volume_cap_bytes: SAMPLE_PAYLOAD_BYTES,
                        run_cap_bytes: plan.max_total_write_bytes,
                        consent: consent.into(),
                        result: "sample_verified".into(),
                        round: Some(round),
                        bytes_written: Some(sample.bytes_written),
                        readback_sha256: Some(sample.observed_sha256.clone()),
                        cleanup_verified: Some(sample.cleanup_verified),
                        pending_recovery: None,
                    }) {
                        return Err(InventoryError(format!(
                            "manual_recovery_required: audit frontier sync failed after benchmark write (lease {} preserved): {audit_err}",
                            lease.tx_id
                        )));
                    }
                    clear_verified_lease(&env.state_dir, env.owner_uid, tx_id)?;
                    completed_bytes = completed_bytes
                        .saturating_add(sample.bytes_written)
                        .min(total_bytes);
                    if let Some(guard) = progress_guard.as_ref() {
                        guard.update(crate::operation_progress::ProgressUpdate {
                            phase: "benchmark_volume",
                            step_index: current_step,
                            target: Some(&fresh_vol.display_name),
                            completed_bytes,
                            total_bytes,
                            throughput_mib_per_s: None,
                        });
                    }
                    samples_by_vol
                        .entry(stable_id.clone())
                        .or_default()
                        .push(sample);
                }
            }
        }
    }

    let final_snapshot = (env.collect_snapshot)()?;
    let final_health = (env.probe_health)();
    let final_now = (env.now_unix_ms)();
    let mut results = Vec::with_capacity(plan.volumes.len());
    for vol in &plan.volumes {
        revalidate_volume(&final_snapshot, vol, &final_health, final_now)?;
        let samples = samples_by_vol
            .remove(&vol.stable_id())
            .ok_or_else(|| InventoryError("missing volume samples at aggregation".into()))?;
        results.push(summarize_volume(vol, &samples)?);
    }

    let recommendation = recommend_leader(request.use_case, &results);
    env.audit
        .record_event(&AuditEvent {
            schema_version: 1,
            tx_id: tx_id.to_owned(),
            timestamp_unix_ms: (env.now_unix_ms)(),
            action: "benchmark_outcome".into(),
            use_case: request.use_case,
            plan_hash: plan.plan_hash.clone(),
            target_identities: plan
                .volumes
                .iter()
                .map(BenchmarkVolume::stable_id)
                .collect(),
            source_sample_timestamps: plan
                .volumes
                .iter()
                .map(|v| v.capacity_observed_unix_ms)
                .collect(),
            per_volume_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
            run_cap_bytes: plan.max_total_write_bytes,
            consent: consent.into(),
            result: recommendation.status.into(),
            round: None,
            bytes_written: Some(results.iter().map(|r| r.total_bytes_written).sum()),
            readback_sha256: None,
            cleanup_verified: Some(true),
            pending_recovery: None,
        })
        .map_err(|err| {
            InventoryError(format!(
                "manual_recovery_required: final benchmark audit outcome sync failed: {err}"
            ))
        })?;

    Ok(BenchmarkReport {
        schema_version: 1,
        tx_id: tx_id.to_owned(),
        plan_hash: plan.plan_hash.clone(),
        platform: "native_linux",
        use_case: request.use_case,
        rounds: ROUNDS_PER_VOLUME,
        volumes: results,
        recommendation,
        writes_performed: true,
        cleanup_verified: true,
        status: "completed",
    })
}

struct AbortAuditContext<'a> {
    tx_id: &'a str,
    plan: &'a BenchmarkPlan,
    use_case: BenchmarkUseCase,
    consent: &'a str,
    round: usize,
    stable_id: &'a str,
    detail: &'a str,
}

fn record_abort(env: &mut BenchmarkEnvironment<'_>, ctx: AbortAuditContext<'_>) {
    let _ = env.audit.record_event(&AuditEvent {
        schema_version: 1,
        tx_id: ctx.tx_id.to_owned(),
        timestamp_unix_ms: (env.now_unix_ms)(),
        action: "benchmark_outcome".into(),
        use_case: ctx.use_case,
        plan_hash: ctx.plan.plan_hash.clone(),
        target_identities: vec![ctx.stable_id.to_owned()],
        source_sample_timestamps: Vec::new(),
        per_volume_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
        run_cap_bytes: ctx.plan.max_total_write_bytes,
        consent: ctx.consent.into(),
        result: "aborted".into(),
        round: Some(ctx.round),
        bytes_written: None,
        readback_sha256: None,
        cleanup_verified: None,
        pending_recovery: Some(ctx.detail.to_owned()),
    });
}

fn obtain_consent<'a>(
    request: &BenchmarkRequest,
    plan: &BenchmarkPlan,
    env: &mut BenchmarkEnvironment<'_>,
    stdout: &mut dyn Write,
) -> Result<&'a str, InventoryError> {
    if request.action == BenchmarkAction::Confirmed {
        return Ok("cli_confirm");
    }
    if !env.is_tty {
        return Err(InventoryError(
            "benchmark requires explicit consent or a foreground terminal; review `ramshared config benchmark --plan` first, then run with `--confirm`"
                .into(),
        ));
    }
    write!(stdout, "{}", render_plan_text(plan))
        .map_err(|err| InventoryError(format!("cannot display benchmark preview: {err}")))?;
    write!(
        stdout,
        "Type BENCHMARK to confirm bounded writes (<= 96 MiB/volume, max {} MiB total): ",
        plan.max_total_write_bytes / (1024 * 1024)
    )
    .and_then(|()| stdout.flush())
    .map_err(|err| InventoryError(format!("cannot prompt for benchmark consent: {err}")))?;
    let mut line = String::new();
    env.consent_reader
        .read_line(&mut line)
        .map_err(|err| InventoryError(format!("cannot read benchmark confirmation: {err}")))?;
    if line.trim() != "BENCHMARK" {
        return Err(InventoryError(
            "benchmark confirmation declined; zero bytes written".into(),
        ));
    }
    Ok("interactive_tty")
}

fn generate_tx_id() -> Result<String, InventoryError> {
    let mut random = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|err| {
            InventoryError(format!(
                "cannot generate transaction ID from /dev/urandom: {err}"
            ))
        })?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

pub(super) fn write_plan_output(
    plan: &BenchmarkPlan,
    json: bool,
    stdout: &mut dyn Write,
) -> Result<(), InventoryError> {
    let text = if json {
        serde_json::to_string(plan)
            .map_err(|err| InventoryError(format!("cannot serialize benchmark plan: {err}")))?
    } else {
        render_plan_text(plan)
    };
    writeln!(stdout, "{text}")
        .map_err(|err| InventoryError(format!("cannot write benchmark plan: {err}")))
}

pub(super) fn write_report_output(
    report: &BenchmarkReport,
    json: bool,
    stdout: &mut dyn Write,
) -> Result<(), InventoryError> {
    let text = if json {
        serde_json::to_string(report)
            .map_err(|err| InventoryError(format!("cannot serialize benchmark report: {err}")))?
    } else {
        render_report_text(report)
    };
    writeln!(stdout, "{text}")
        .map_err(|err| InventoryError(format!("cannot write benchmark report: {err}")))
}

pub(crate) fn render_plan_text(plan: &BenchmarkPlan) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "RamShared bounded storage benchmark plan ({})",
        plan.plan_hash
    );
    let _ = writeln!(
        out,
        "Use case: {:?} | Rounds: {} | Sample size: {} MiB | Cap/volume: {} MiB | Max run writes: {} MiB",
        plan.use_case,
        plan.rounds,
        plan.sample_payload_bytes / (1024 * 1024),
        plan.per_volume_write_cap_bytes / (1024 * 1024),
        plan.max_total_write_bytes / (1024 * 1024)
    );
    for vol in &plan.volumes {
        let _ = writeln!(
            out,
            "  {} ({} mount {}, free {:.2} GiB, requires 96 MiB + 10 GiB)",
            vol.display_name,
            vol.filesystem,
            vol.mount_id,
            (vol.free_bytes as f64) / (1024.0 * 1024.0 * 1024.0)
        );
    }
    let _ = writeln!(
        out,
        "Read-only preview. No files or settings were modified."
    );
    out
}

pub(crate) fn render_report_text(report: &BenchmarkReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "RamShared storage benchmark ({:?}, tx={}, plan={}):",
        report.use_case, report.tx_id, report.plan_hash
    );
    for vol in &report.volumes {
        let _ = writeln!(
            out,
            "  {} [{}]: seq median {:.2} MiB/s (stddev {:.2}, runs {:?}); 4KiB fdatasync median {:.1} us, p99 {:.1} us (n={}); written {} MiB; cleanup verified={}",
            vol.display_name,
            vol.stable_id,
            vol.sequential_median_mibs_per_sec,
            vol.sequential_stddev_mibs_per_sec,
            vol.sequential_mibs_per_sec,
            vol.small_write_median_us,
            vol.small_write_p99_us,
            vol.small_write_sample_count,
            vol.total_bytes_written / (1024 * 1024),
            vol.cleanup_verified
        );
    }
    let _ = writeln!(
        out,
        "Recommendation ({}): {} — {}",
        report.recommendation.status,
        report
            .recommendation
            .recommended_stable_id
            .as_deref()
            .unwrap_or("none"),
        report.recommendation.rationale
    );
    out
}
