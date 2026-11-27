// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/runtime.rs
//! Pure phase machine for storage-only product path (SPEC DT-7 / DT-9 / DT-12).
//!
//! Injectable [`RuntimeOps`] keeps Linux unit tests free of CUDA/Windows handles.

use crate::config::WinDriveConfig;
use crate::package::StartPolicy;

/// Runtime lifecycle phases (DT-7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimePhase {
    Stopped,
    WaitingForBroker,
    Leased,
    CudaReady,
    DiskCreated,
    QueueRegistered,
    Online,
    Degraded,
    Stopping,
    FailedSafe,
}

pub fn contain_broker_loss(state: &mut RuntimeState) {
    if matches!(
        state.phase,
        RuntimePhase::Online | RuntimePhase::Degraded | RuntimePhase::FailedSafe
    ) {
        state.phase = RuntimePhase::FailedSafe;
        state.healthy = false;
    } else {
        state.phase = RuntimePhase::Stopped;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostTeardownReleaseDisposition {
    CleanStop,
    StopWithBrokerFailure,
    PreserveFailedSafe,
}

pub fn post_teardown_release_disposition(
    broker_lost_online: bool,
    release_succeeded: bool,
) -> PostTeardownReleaseDisposition {
    if release_succeeded {
        PostTeardownReleaseDisposition::CleanStop
    } else if broker_lost_online {
        PostTeardownReleaseDisposition::StopWithBrokerFailure
    } else {
        PostTeardownReleaseDisposition::PreserveFailedSafe
    }
}

pub fn uninstall_plan_safe(storage_owned: bool, ambiguous: bool) -> bool {
    !storage_owned && !ambiguous
}

/// How the process was entered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    Console,
    Scm,
    ProbeCuda,
}

/// Stable error class for evidence / exit mapping (no payloads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeErrorClass {
    Config,
    Broker,
    Cuda,
    Abi,
    Identity,
    Checksum,
    PagefileSafety,
    Busy,
    Watchdog,
    Internal,
    AmbiguousCrash,
}

/// Structured runtime error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeError {
    pub class: RuntimeErrorClass,
    pub code: i32,
    pub message: String,
}

impl RuntimeError {
    pub fn new(class: RuntimeErrorClass, code: i32, message: impl Into<String>) -> Self {
        Self {
            class,
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "runtime {:?} code={}: {}",
            self.class, self.code, self.message
        )
    }
}

impl std::error::Error for RuntimeError {}

/// Summary returned after a successful or refused stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSummary {
    pub phase: RuntimePhase,
    pub lease_id: Option<u32>,
    pub allocated_bytes: u64,
    pub idempotent_stop: bool,
    pub exit_code: i32,
}

/// Authoritative in-process state (never reconstructed from JSONL — DT-9).
#[derive(Clone, Debug)]
pub struct RuntimeState {
    pub phase: RuntimePhase,
    pub mode: RunMode,
    pub lease_id: Option<u32>,
    pub lease_bytes: u64,
    pub allocated_bytes: u64,
    pub stop_completed: bool,
    pub healthy: bool,
    pub cuda_op_outstanding: bool,
    pub ambiguous_crash: bool,
    /// Effects applied once (for unwind / idempotent stop).
    pub effects: EffectLog,
}

impl RuntimeState {
    pub fn new(mode: RunMode) -> Self {
        Self {
            phase: RuntimePhase::Stopped,
            mode,
            lease_id: None,
            lease_bytes: 0,
            allocated_bytes: 0,
            stop_completed: false,
            healthy: true,
            cuda_op_outstanding: false,
            ambiguous_crash: false,
            effects: EffectLog::default(),
        }
    }
}

/// Countable side effects for unit tests (injection seam observability).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectLog {
    pub lease_acquire: u32,
    pub lease_release: u32,
    pub cuda_alloc: u32,
    pub cuda_free: u32,
    pub disk_create: u32,
    pub disk_destroy: u32,
    pub queue_register: u32,
    pub queue_unregister: u32,
    pub fallback_selected: u32,
    pub retries: u32,
}

/// Injectable operations for the phase machine.
pub trait RuntimeOps {
    fn acquire_lease(&mut self, bytes: u64) -> Result<u32, RuntimeError>;
    fn release_lease(&mut self, lease_id: u32) -> Result<(), RuntimeError>;
    fn cuda_alloc(&mut self, bytes: u64) -> Result<(), RuntimeError>;
    fn cuda_free(&mut self) -> Result<(), RuntimeError>;
    fn create_disk(&mut self) -> Result<(), RuntimeError>;
    fn destroy_disk(&mut self) -> Result<(), RuntimeError>;
    fn register_queue(&mut self) -> Result<(), RuntimeError>;
    fn unregister_queue(&mut self) -> Result<(), RuntimeError>;
    /// Gate A/B pagefile safety. Ok(true) = clear; Ok(false)/Err refuse destructive path.
    fn pagefile_gates_clear(&mut self) -> Result<bool, RuntimeError>;
    fn drain_io(&mut self) -> Result<(), RuntimeError>;
    /// Optional busy observation (enumeration). Returns true if still busy.
    fn observe_busy(&mut self) -> Result<bool, RuntimeError> {
        Ok(false)
    }
}

/// Run product startup to Online (or fail with reverse unwind).
pub fn run_runtime<O: RuntimeOps>(
    cfg: &WinDriveConfig,
    state: &mut RuntimeState,
    ops: &mut O,
) -> Result<RuntimeSummary, RuntimeError> {
    if state.ambiguous_crash {
        return Err(RuntimeError::new(
            RuntimeErrorClass::AmbiguousCrash,
            8,
            "ambiguous crash state is not replayed",
        ));
    }
    if state.phase != RuntimePhase::Stopped || state.stop_completed {
        return Err(RuntimeError::new(
            RuntimeErrorClass::Internal,
            1,
            "run_runtime requires Stopped phase",
        ));
    }

    // Leased
    let lease_id = match ops.acquire_lease(cfg.size_bytes) {
        Ok(id) => id,
        Err(e) => {
            state.phase = RuntimePhase::FailedSafe;
            return Err(e);
        }
    };
    state.effects.lease_acquire += 1;
    state.lease_id = Some(lease_id);
    state.lease_bytes = cfg.size_bytes;
    state.phase = RuntimePhase::Leased;

    // CudaReady
    if let Err(e) = ops.cuda_alloc(cfg.size_bytes) {
        unwind_from(state, ops, RuntimePhase::Leased)?;
        state.phase = RuntimePhase::FailedSafe;
        return Err(e);
    }
    state.effects.cuda_alloc += 1;
    state.allocated_bytes = cfg.size_bytes;
    state.phase = RuntimePhase::CudaReady;

    // DiskCreated
    if let Err(e) = create_with_bounded_busy(ops, &mut state.effects) {
        unwind_from(state, ops, RuntimePhase::CudaReady)?;
        state.phase = RuntimePhase::FailedSafe;
        return Err(e);
    }
    state.effects.disk_create += 1;
    state.phase = RuntimePhase::DiskCreated;

    // QueueRegistered
    if let Err(e) = ops.register_queue() {
        unwind_from(state, ops, RuntimePhase::DiskCreated)?;
        state.phase = RuntimePhase::FailedSafe;
        return Err(e);
    }
    state.effects.queue_register += 1;
    state.phase = RuntimePhase::QueueRegistered;

    state.phase = RuntimePhase::Online;
    Ok(RuntimeSummary {
        phase: RuntimePhase::Online,
        lease_id: state.lease_id,
        allocated_bytes: state.allocated_bytes,
        idempotent_stop: false,
        exit_code: 0,
    })
}

fn create_with_bounded_busy<O: RuntimeOps>(
    ops: &mut O,
    effects: &mut EffectLog,
) -> Result<(), RuntimeError> {
    match ops.create_disk() {
        Ok(()) => Ok(()),
        Err(e) if e.class == RuntimeErrorClass::Busy => {
            // One busy observation only (DT-7) — re-query, do not repeat CREATE.
            effects.retries += 1;
            if effects.retries > 1 {
                return Err(RuntimeError::new(
                    RuntimeErrorClass::Busy,
                    5,
                    "busy observation budget exhausted",
                ));
            }
            let still = ops.observe_busy()?;
            if still {
                Err(e)
            } else {
                // State re-queried clear; still must not re-issue CREATE in this design
                // when first CREATE returned busy without completing — fail closed.
                Err(RuntimeError::new(
                    RuntimeErrorClass::Busy,
                    5,
                    "CREATE busy; will not retry IOCTL",
                ))
            }
        }
        Err(e) => Err(e),
    }
}

/// Stop / teardown. Idempotent after successful completion (DT-9).
pub fn stop_runtime<O: RuntimeOps>(
    state: &mut RuntimeState,
    ops: &mut O,
) -> Result<RuntimeSummary, RuntimeError> {
    if state.ambiguous_crash {
        return Err(RuntimeError::new(
            RuntimeErrorClass::AmbiguousCrash,
            8,
            "ambiguous crash state is not replayed",
        ));
    }
    if state.stop_completed && state.phase == RuntimePhase::Stopped {
        return Ok(RuntimeSummary {
            phase: RuntimePhase::Stopped,
            lease_id: None,
            allocated_bytes: 0,
            idempotent_stop: true,
            exit_code: 0,
        });
    }
    if state.phase == RuntimePhase::Stopped && !state.stop_completed {
        // Never started.
        state.stop_completed = true;
        return Ok(RuntimeSummary {
            phase: RuntimePhase::Stopped,
            lease_id: None,
            allocated_bytes: 0,
            idempotent_stop: true,
            exit_code: 0,
        });
    }

    // Watchdog path: CUDA still outstanding — do not destroy context (DT-12).
    if state.cuda_op_outstanding {
        state.healthy = false;
        state.phase = RuntimePhase::FailedSafe;
        return Err(RuntimeError::new(
            RuntimeErrorClass::Watchdog,
            9,
            "cuda op outstanding; preserve allocation/lease/disk",
        ));
    }

    let prev = state.phase;
    state.phase = RuntimePhase::Stopping;

    // Pagefile gates before any destructive effect (DT-8).
    match ops.pagefile_gates_clear() {
        Ok(true) => {}
        Ok(false) => {
            state.phase = RuntimePhase::Online;
            return Err(RuntimeError::new(
                RuntimeErrorClass::PagefileSafety,
                7,
                "pagefile gate refused teardown",
            ));
        }
        Err(e) => {
            state.phase = RuntimePhase::Online;
            return Err(e);
        }
    }

    ops.drain_io()?;
    unwind_from(state, ops, prev)?;
    state.phase = RuntimePhase::Stopped;
    state.stop_completed = true;
    state.lease_id = None;
    state.allocated_bytes = 0;
    Ok(RuntimeSummary {
        phase: RuntimePhase::Stopped,
        lease_id: None,
        allocated_bytes: 0,
        idempotent_stop: false,
        exit_code: 0,
    })
}

/// Reverse unwind from `from` phase inclusive of resources held at that phase.
fn unwind_from<O: RuntimeOps>(
    state: &mut RuntimeState,
    ops: &mut O,
    from: RuntimePhase,
) -> Result<(), RuntimeError> {
    // Order: REGISTER -> DESTROY -> free -> release (DT-7 reverse).
    let need_unreg = matches!(
        from,
        RuntimePhase::QueueRegistered | RuntimePhase::Online | RuntimePhase::Stopping
    ) || state.effects.queue_register > state.effects.queue_unregister;
    let need_destroy = need_unreg
        || matches!(from, RuntimePhase::DiskCreated)
        || state.effects.disk_create > state.effects.disk_destroy;
    let need_free = need_destroy
        || matches!(from, RuntimePhase::CudaReady)
        || state.effects.cuda_alloc > state.effects.cuda_free;
    let need_release = need_free
        || matches!(from, RuntimePhase::Leased)
        || (state.lease_id.is_some() && state.effects.lease_acquire > state.effects.lease_release);

    if need_unreg && state.effects.queue_register > state.effects.queue_unregister {
        ops.unregister_queue()?;
        state.effects.queue_unregister += 1;
    }
    if need_destroy && state.effects.disk_create > state.effects.disk_destroy {
        ops.destroy_disk()?;
        state.effects.disk_destroy += 1;
    }
    if need_free && state.effects.cuda_alloc > state.effects.cuda_free {
        ops.cuda_free()?;
        state.effects.cuda_free += 1;
        state.allocated_bytes = 0;
    }
    if need_release
        && let Some(id) = state.lease_id.take()
        && (state.effects.lease_release == 0
            || state.effects.lease_acquire > state.effects.lease_release)
    {
        ops.release_lease(id)?;
        state.effects.lease_release += 1;
    }
    Ok(())
}

// --- CLI helpers (pure; extracted from main for coverage) ---

/// Mode registered on `RamSharedWinSvc` during `install` or `repair`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InstallMode {
    /// Full StorPort CUDA VRAM disk + broker consumer (`--config <winsvc.toml>`).
    #[default]
    Storage,
    /// Authenticated vsock control-plane authority only (`authority-service`).
    AuthorityOnly,
}

/// Product CLI verbs accepted by the binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProductCommand {
    Install {
        manifest: String,
        mode: InstallMode,
    },
    Repair {
        manifest: String,
        mode: InstallMode,
    },
    Status {
        json: bool,
    },
    InspectGpus,
    InspectGpuActivity {
        supervised: bool,
    },
    InspectVms,
    InspectStorage {
        file: String,
    },
    InspectStorageDisk {
        disk_id: [u8; 16],
    },
    BenchmarkVolumes {
        confirm: bool,
        use_case: crate::storage_benchmark::BenchmarkUseCase,
        volumes: Vec<String>,
        json: bool,
    },
    Start,
    Stop,
    Uninstall,
    ProbeCuda {
        config: String,
    },
    Console {
        config: String,
        storage_only: bool,
    },
    Package {
        dir: String,
        version: String,
        commit: String,
        start_policy: StartPolicy,
    },
    VerifyPackage {
        dir: String,
    },
    BumpInfDriverVer {
        inf: String,
    },
    ScmDefault {
        config: Option<String>,
    },
    /// SCM authority-only mode, with no Windows storage runtime.
    AuthorityService,
}

/// Parse argv (skip program name). Rejects lab backend verbs.
pub fn parse_product_cli(args: &[String]) -> Result<ProductCommand, RuntimeError> {
    match args.first().map(String::as_str) {
        None | Some("") => Ok(ProductCommand::ScmDefault { config: None }),
        Some("authority-service") if args.len() == 1 => Ok(ProductCommand::AuthorityService),
        Some("--config") if args.len() == 2 => Ok(ProductCommand::ScmDefault {
            config: Some(require_config_flag(args)?),
        }),
        Some("install") => {
            let (manifest, mode) = parse_install_args(&args[1..])?;
            Ok(ProductCommand::Install { manifest, mode })
        }
        Some("repair") => {
            let (manifest, mode) = parse_install_args(&args[1..])?;
            Ok(ProductCommand::Repair { manifest, mode })
        }
        Some("status") if args.len() == 1 => Ok(ProductCommand::Status { json: false }),
        Some("status") if args.len() == 2 && args[1] == "--json" => {
            Ok(ProductCommand::Status { json: true })
        }
        Some("inspect-storage")
            if args.len() == 3
                && args[1] == "--file"
                && path_is_absolute_str(&args[2])
                && !args[2].chars().any(char::is_control) =>
        {
            Ok(ProductCommand::InspectStorage {
                file: args[2].clone(),
            })
        }
        Some("inspect-storage") if args.len() == 3 && args[1] == "--disk-id" => {
            let disk_id = crate::host_storage::parse_disk_id_hex(&args[2])
                .map_err(|msg| RuntimeError::new(RuntimeErrorClass::Config, 2, msg))?;
            Ok(ProductCommand::InspectStorageDisk { disk_id })
        }
        Some("inspect-vms") if args.len() == 2 && args[1] == "--json" => {
            Ok(ProductCommand::InspectVms)
        }
        Some("inspect-gpus") if args.len() == 2 && args[1] == "--json" => {
            Ok(ProductCommand::InspectGpus)
        }
        Some("inspect-gpu-activity")
            if args.len() == 2 && matches!(args[1].as_str(), "--json" | "--supervised-json") =>
        {
            Ok(ProductCommand::InspectGpuActivity {
                supervised: args[1] == "--supervised-json",
            })
        }
        Some("benchmark-volumes") => {
            let parsed = crate::storage_benchmark::parse_benchmark_volumes_args(&args[1..])
                .map_err(|msg| RuntimeError::new(RuntimeErrorClass::Config, 2, msg))?;
            Ok(ProductCommand::BenchmarkVolumes {
                confirm: parsed.confirm,
                use_case: parsed.use_case,
                volumes: parsed.volumes,
                json: parsed.json,
            })
        }
        Some("start") if args.len() == 1 => Ok(ProductCommand::Start),
        Some("stop") if args.len() == 1 => Ok(ProductCommand::Stop),
        Some("uninstall") if args.len() == 1 => Ok(ProductCommand::Uninstall),
        Some("probe-cuda") => {
            let config = require_config_flag(args)?;
            Ok(ProductCommand::ProbeCuda { config })
        }
        Some("console") => {
            let config = require_config_flag(args)?;
            let storage_only = args.iter().any(|a| a == "--storage-only");
            if !storage_only {
                return Err(RuntimeError::new(
                    RuntimeErrorClass::Config,
                    2,
                    "console requires --storage-only",
                ));
            }
            Ok(ProductCommand::Console {
                config,
                storage_only: true,
            })
        }
        Some("package") => parse_package_args(&args[1..]),
        Some("package-verify") => Ok(ProductCommand::VerifyPackage {
            dir: require_absolute_flag(args, "--dir")?,
        }),
        Some("package-bump-inf") => Ok(ProductCommand::BumpInfDriverVer {
            inf: require_absolute_flag(args, "--inf")?,
        }),
        Some("run") | Some("stop-console") | Some("lab") | Some("start-scripts") => {
            Err(RuntimeError::new(
                RuntimeErrorClass::Config,
                2,
                "lab backend command removed from product CLI",
            ))
        }
        Some(other) => Err(RuntimeError::new(
            RuntimeErrorClass::Config,
            2,
            format!("unknown command: {other}"),
        )),
    }
}

/// `package --dir <absolute> --version <version> --commit <hex>
/// [--start-policy demand|automatic]`.
///
/// Packaging is host-independent: it is the same parser on Linux and Windows,
/// and it refuses flags it does not know instead of ignoring them.
fn parse_package_args(args: &[String]) -> Result<ProductCommand, RuntimeError> {
    let mut dir = None;
    let mut version = None;
    let mut commit = None;
    let mut start_policy = StartPolicy::Demand;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args.get(index + 1).ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorClass::Config,
                2,
                format!("{flag} requires a value"),
            )
        })?;
        match flag {
            "--dir" => {
                if value.is_empty() || !path_is_absolute_str(value) {
                    return Err(RuntimeError::new(
                        RuntimeErrorClass::Config,
                        2,
                        "--dir requires an absolute package directory",
                    ));
                }
                dir = Some(value.clone());
            }
            "--version" => version = Some(value.clone()),
            "--commit" => commit = Some(value.clone()),
            "--start-policy" => {
                start_policy = match value.as_str() {
                    "demand" => StartPolicy::Demand,
                    "automatic" => StartPolicy::Automatic,
                    _ => {
                        return Err(RuntimeError::new(
                            RuntimeErrorClass::Config,
                            2,
                            "--start-policy must be demand or automatic",
                        ));
                    }
                };
            }
            other => {
                return Err(RuntimeError::new(
                    RuntimeErrorClass::Config,
                    2,
                    format!("unknown package flag: {other}"),
                ));
            }
        }
        index += 2;
    }
    Ok(ProductCommand::Package {
        dir: dir.ok_or_else(|| {
            RuntimeError::new(RuntimeErrorClass::Config, 2, "package requires --dir")
        })?,
        version: version.ok_or_else(|| {
            RuntimeError::new(RuntimeErrorClass::Config, 2, "package requires --version")
        })?,
        commit: commit.ok_or_else(|| {
            RuntimeError::new(RuntimeErrorClass::Config, 2, "package requires --commit")
        })?,
        start_policy,
    })
}

fn parse_install_args(args: &[String]) -> Result<(String, InstallMode), RuntimeError> {
    let mut manifest: Option<String> = None;
    let mut mode: Option<InstallMode> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--manifest" => {
                if manifest.is_some() {
                    return Err(RuntimeError::new(
                        RuntimeErrorClass::Config,
                        2,
                        "duplicate --manifest flag",
                    ));
                }
                let path = args.get(index + 1).cloned().ok_or_else(|| {
                    RuntimeError::new(RuntimeErrorClass::Config, 2, "--manifest requires a path")
                })?;
                if path.is_empty() || !path_is_absolute_str(&path) {
                    return Err(RuntimeError::new(
                        RuntimeErrorClass::Config,
                        2,
                        "--manifest path must be absolute",
                    ));
                }
                manifest = Some(path);
                index += 2;
            }
            "--authority-only" => {
                if mode.is_some() {
                    return Err(RuntimeError::new(
                        RuntimeErrorClass::Config,
                        2,
                        "duplicate --authority-only flag",
                    ));
                }
                mode = Some(InstallMode::AuthorityOnly);
                index += 1;
            }
            other => {
                return Err(RuntimeError::new(
                    RuntimeErrorClass::Config,
                    2,
                    format!("unknown install option: {other}"),
                ));
            }
        }
    }
    let manifest = manifest.ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorClass::Config,
            2,
            "missing --manifest <absolute-path>",
        )
    })?;
    Ok((manifest, mode.unwrap_or(InstallMode::Storage)))
}

fn require_config_flag(args: &[String]) -> Result<String, RuntimeError> {
    require_absolute_flag(args, "--config")
}

fn require_absolute_flag(args: &[String], flag: &str) -> Result<String, RuntimeError> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            let path = args.get(i + 1).cloned().ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorClass::Config,
                    2,
                    format!("{flag} requires a path"),
                )
            })?;
            if path.is_empty() || !path_is_absolute_str(&path) {
                return Err(RuntimeError::new(
                    RuntimeErrorClass::Config,
                    2,
                    format!("{flag} path must be absolute"),
                ));
            }
            return Ok(path);
        }
        i += 1;
    }
    Err(RuntimeError::new(
        RuntimeErrorClass::Config,
        2,
        format!("missing {flag} <absolute-path>"),
    ))
}

fn path_is_absolute_str(p: &str) -> bool {
    // Windows absolute: C:\... or \\?\... ; Unix: /
    if p.starts_with('/') {
        return true;
    }
    let bytes = p.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    p.starts_with(r"\\")
}

/// SCM and console must select the same storage-only runtime (no lab PS1).
pub fn product_runtime_selected(cmd: &ProductCommand) -> bool {
    matches!(
        cmd,
        ProductCommand::Console {
            storage_only: true,
            ..
        } | ProductCommand::ScmDefault { .. }
            | ProductCommand::ProbeCuda { .. }
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod vm_probe_cli_tests {
    #[test]
    fn storage_inspection_requires_one_absolute_file_without_extra_arguments() {
        use super::{ProductCommand, parse_product_cli};
        assert_eq!(
            parse_product_cli(&[
                "inspect-storage".into(),
                "--file".into(),
                r"C:\data\disk.vhdx".into()
            ]),
            Ok(ProductCommand::InspectStorage {
                file: r"C:\data\disk.vhdx".into()
            })
        );
        for args in [
            vec!["inspect-storage"],
            vec!["inspect-storage", "--file", "relative"],
            vec!["inspect-storage", "--file", r"C:\data\disk.vhdx", "--start"],
            vec!["inspect-storage", "--disk-id", "short"],
            vec![
                "inspect-storage",
                "--disk-id",
                "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            ],
        ] {
            assert!(
                parse_product_cli(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err()
            );
        }
        assert_eq!(
            parse_product_cli(&[
                "inspect-storage".into(),
                "--disk-id".into(),
                "0123456789abcdef0123456789abcdef".into(),
            ]),
            Ok(ProductCommand::InspectStorageDisk {
                disk_id: [
                    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89,
                    0xab, 0xcd, 0xef,
                ],
            })
        );
    }

    #[test]
    fn vm_inspection_cli_is_read_only_and_needs_no_storage_configuration() {
        use super::{ProductCommand, parse_product_cli};
        assert_eq!(
            parse_product_cli(&["inspect-vms".into(), "--json".into()]),
            Ok(ProductCommand::InspectVms)
        );
        for values in [
            vec!["inspect-vms"],
            vec!["inspect-vms", "--json", "--start"],
            vec!["inspect-vms", "--config", "C:\\config.toml"],
        ] {
            assert!(
                parse_product_cli(&values.into_iter().map(String::from).collect::<Vec<_>>())
                    .is_err()
            );
        }
        assert_eq!(
            parse_product_cli(&[
                "package-bump-inf".into(),
                "--inf".into(),
                "C:\\ramshared\\ramshared.inf".into(),
            ]),
            Ok(ProductCommand::BumpInfDriverVer {
                inf: "C:\\ramshared\\ramshared.inf".into(),
            })
        );
        assert!(parse_product_cli(&["package-bump-inf".into()]).is_err());
    }
}
