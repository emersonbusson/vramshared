// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_worker_request.rs
//! Typed isolated-worker request and custody of the inherited data socket.
use ramshared_block::GpuWorkerConfig;
use ramshared_vram::GpuAdapterIdentity;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

const MAX_ADAPTER_IDENTITY_BYTES: usize = 256;

#[derive(Clone, Debug)]
pub(super) struct WorkerSpawnParams {
    pub(super) target_bytes: u64,
    pub(super) chunk_bytes: u64,
    pub(super) reserve_floor: u64,
    pub(super) expected_adapter: Option<GpuAdapterIdentity>,
    pub(super) generation: Option<std::sync::Arc<super::native_cache::NativeCacheSession>>,
    pub(super) host_catalog: Option<ramshared_wsl2d::host_catalog::SharedHostCatalog>,
}

#[derive(Debug)]
pub(super) struct WorkerRequest {
    pub(super) fd: i32,
    pub(super) config: GpuWorkerConfig,
    pub(super) expected_adapter: Option<GpuAdapterIdentity>,
    pub(super) generation: Option<(i32, u64)>,
    pub(super) catalog_fd: Option<i32>,
}

impl WorkerRequest {
    pub(super) fn allows_backend(&self, backend: &str) -> bool {
        self.expected_adapter
            .as_ref()
            .is_none_or(|expected| expected.backend == backend)
    }
    pub(super) fn accepts_adapter(&self, identity: Option<&GpuAdapterIdentity>) -> bool {
        self.expected_adapter
            .as_ref()
            .is_none_or(|expected| identity == Some(expected))
    }
}

impl WorkerSpawnParams {
    pub(super) fn arguments(&self, fd: i32) -> Result<Vec<String>, String> {
        let mut args = vec![
            "--fd".into(),
            fd.to_string(),
            "--target-bytes".into(),
            self.target_bytes.to_string(),
            "--chunk-bytes".into(),
            self.chunk_bytes.to_string(),
            "--reserve-floor".into(),
            self.reserve_floor.to_string(),
            "--compression".into(),
        ];
        if let Some(identity) = &self.expected_adapter {
            args.push("--adapter-identity".into());
            args.push(serde_json::to_string(identity).map_err(|error| error.to_string())?);
        }
        parse_isolated_gpu_worker_args(&args)?;
        Ok(args)
    }
}

/// Complete parent-side identity proof before an active worker is exposed.
/// Unavailable workers advertise no capacity and grant no adapter authority.
pub(super) fn complete_startup_binding(
    client: &mut ramshared_block::IpcCacheClient,
) -> Result<(), String> {
    use ramshared_block::BestEffortCache;
    if client.state() != ramshared_block::CacheState::Active {
        return Ok(());
    }
    client.refresh_cached_bytes().map_err(str::to_string)?;
    validate_startup_binding(client)
}

/// Recheck a queued result at handoff without driver or socket I/O.
pub(super) fn validate_startup_binding(
    client: &ramshared_block::IpcCacheClient,
) -> Result<(), String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("worker startup clock is unavailable: {error}"))?;
    let now = u64::try_from(stamp.as_millis())
        .map_err(|_| "worker startup clock overflows milliseconds")?;
    validate_startup_binding_at(client, now)
}

fn validate_startup_binding_at(
    client: &ramshared_block::IpcCacheClient,
    now: u64,
) -> Result<(), String> {
    use ramshared_block::BestEffortCache;
    if client.state() != ramshared_block::CacheState::Active {
        return Ok(());
    }
    let budget = client
        .gpu_budget_telemetry()
        .filter(|sample| {
            sample
                .trusted_available_at(now, ramshared_vram::GPU_BUDGET_MAX_AGE_MS)
                .is_some()
        })
        .ok_or("active worker did not prove a fresh driver budget")?;
    if client.bound_adapter_identity().is_none()
        || budget.adapter.as_ref() != client.bound_adapter_identity()
    {
        return Err("active worker did not prove a stable adapter binding".into());
    }
    Ok(())
}

/// Validate a duplicate without taking ownership of untrusted numeric input. This runs
/// before driver libraries or additional threads are started in the child.
pub(super) fn duplicate_inherited_socket(raw: i32) -> io::Result<UnixStream> {
    // SAFETY: fcntl accepts a numeric descriptor, including invalid values,
    // without dereferencing pointers. Success creates a fresh exclusively owned
    // descriptor; no BorrowedFd is constructed from unproven numeric input.
    let duplicate = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful F_DUPFD_CLOEXEC returned this fresh owned fd.
    let held = unsafe { OwnedFd::from_raw_fd(duplicate) };
    if rustix::net::sockopt::socket_type(&held)? != rustix::net::SocketType::STREAM
        || !rustix::net::getpeername(&held)?
            .is_some_and(|peer| peer.address_family() == rustix::net::AddressFamily::UNIX)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "isolated worker needs a connected Unix stream socket",
        ));
    }
    // SAFETY: fcntl accepts this raw numeric input without pointers. Retain
    // the original until process exit; closing it here could invalidate a Rust
    // owner if an internal caller supplied a borrowed descriptor. CLOEXEC keeps
    // both aliases out of any subsequently executed driver helper.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UnixStream::from(held))
}

/// Arm and verify parent lifetime using the socket creator's kernel identity.
/// This also handles reparenting to a subreaper rather than assuming PID 1.
pub(super) fn bind_parent_lifetime(socket: &UnixStream) -> io::Result<()> {
    let creator = rustix::net::sockopt::socket_peercred(socket)?.pid;
    verify_parent_binding(creator, rustix::process::getppid, arm_parent_death)
}

fn arm_parent_death() -> io::Result<()> {
    // SAFETY: PR_SET_PDEATHSIG consumes only scalar arguments. Use unsigned
    // long varargs, as required by the ABI; no pointer or borrowed fd is used.
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGTERM as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn verify_parent_binding(
    creator: rustix::process::Pid,
    parent: impl Fn() -> Option<rustix::process::Pid>,
    arm: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let changed =
        || io::Error::other("isolated GPU worker no longer belongs to its socket creator");
    if parent() != Some(creator) {
        return Err(changed());
    }
    arm()?;
    if parent() != Some(creator) {
        return Err(changed());
    }
    Ok(())
}

fn parse_adapter_identity(text: &str) -> Result<GpuAdapterIdentity, String> {
    if text.len() > MAX_ADAPTER_IDENTITY_BYTES {
        return Err("isolated GPU worker identity exceeds its byte limit".into());
    }
    let identity: GpuAdapterIdentity = serde_json::from_str(text)
        .map_err(|error| format!("invalid isolated GPU worker identity: {error}"))?;
    if !identity.is_stable_driver_identity() {
        return Err("isolated GPU worker identity is not stable".into());
    }
    Ok(identity)
}

pub(super) fn parse_isolated_gpu_worker_args(args: &[String]) -> Result<WorkerRequest, String> {
    if args.len() > 18 {
        return Err("isolated GPU worker argument count exceeds its limit".into());
    }
    let mut expected_adapter = None;
    let (mut generation_fd, mut generation_id) = (None, None);
    let mut catalog_fd = None;
    let mut fd_raw = None;
    let mut reserve_floor_raw = None;
    let mut config = GpuWorkerConfig::default();
    let mut seen = std::collections::HashSet::new();
    let mut values = args.iter();
    while let Some(option) = values.next() {
        if !seen.insert(option.as_str()) {
            return Err(format!("duplicate isolated GPU worker option {option}"));
        }
        let mut value = || {
            values
                .next()
                .ok_or_else(|| format!("missing value for isolated GPU worker option {option}"))
        };
        match option.as_str() {
            "--fd" => {
                let raw = value()?
                    .parse::<i32>()
                    .map_err(|_| "isolated GPU worker fd is invalid".to_string())?;
                if raw < 0 {
                    return Err("isolated GPU worker fd must be nonnegative".into());
                }
                fd_raw = Some(raw);
            }
            "--target-bytes" => {
                config.target_bytes = value()?
                    .parse::<u64>()
                    .map_err(|_| "isolated GPU worker target is invalid".to_string())?;
                if config.target_bytes == 0 {
                    return Err("isolated GPU worker target must be positive".into());
                }
            }
            "--chunk-bytes" => {
                config.chunk_bytes = value()?
                    .parse::<usize>()
                    .map_err(|_| "isolated GPU worker chunk size is invalid".to_string())?;
                if config.chunk_bytes == 0 {
                    return Err("isolated GPU worker chunk size must be positive".into());
                }
            }
            "--reserve-floor" => {
                // Presence is mandatory even though `0` is a legal value
                // (DT-9 case 3). A missing flag must fail closed rather than
                // fall back to `GpuWorkerConfig::default()` and silently
                // undercut the sealed floor (RF-1, RF-5).
                let raw = value()?
                    .parse::<u64>()
                    .map_err(|_| "isolated GPU worker reserve floor is invalid".to_string())?;
                config.reserve_floor_bytes = raw;
                reserve_floor_raw = Some(raw);
            }
            "--compression" => {
                config.compression_enabled = true;
            }
            "--adapter-identity" => {
                expected_adapter = Some(parse_adapter_identity(value()?)?);
            }
            "--configuration-fd" => {
                let fd = value()?
                    .parse::<i32>()
                    .map_err(|_| "configuration fd is invalid")?;
                if fd < 3 {
                    return Err("configuration fd must be separate from standard streams".into());
                }
                generation_fd = Some(fd);
            }
            "--configuration-generation" => {
                let generation = value()?
                    .parse::<u64>()
                    .map_err(|_| "configuration generation is invalid")?;
                if generation == 0 {
                    return Err("configuration generation must be positive".into());
                }
                generation_id = Some(generation);
            }
            arg if arg == ramshared_wsl2d::host_catalog::CATALOG_FD_ARG => {
                let fd = value()?
                    .parse::<i32>()
                    .map_err(|_| "catalog fd is invalid")?;
                if fd < 3 {
                    return Err("catalog fd must be separate from standard streams".into());
                }
                catalog_fd = Some(fd);
            }
            _ => return Err(format!("unknown isolated GPU worker option {option}")),
        }
    }
    let fd_raw = fd_raw.ok_or_else(|| "missing --fd for isolated gpu worker".to_string())?;
    reserve_floor_raw
        .ok_or_else(|| "missing --reserve-floor for isolated gpu worker".to_string())?;
    let generation = match (generation_fd, generation_id) {
        (None, None) => None,
        (Some(fd), Some(id)) if expected_adapter.is_some() => Some((fd, id)),
        _ => return Err("configured worker needs descriptor, generation and exact adapter".into()),
    };
    if let Some(fd) = catalog_fd
        && (fd == fd_raw || generation_fd == Some(fd))
    {
        return Err("catalog fd must not alias worker or configuration streams".into());
    }
    Ok(WorkerRequest {
        fd: fd_raw,
        config,
        expected_adapter,
        generation,
        catalog_fd,
    })
}

/// Open and pre-seed an anonymous non-blocking host catalog notification
/// descriptor for inheritance by the `__gpu_worker` child.
pub(super) fn inherit_catalog_stream(
    command: &mut std::process::Command,
    catalog: &ramshared_wsl2d::host_catalog::SharedHostCatalog,
) -> io::Result<OwnedFd> {
    use std::os::fd::AsFd;
    use std::os::unix::process::CommandExt;

    let reader = catalog.subscribe()?;
    let dup = rustix::io::fcntl_dupfd_cloexec(reader.as_fd(), 3).map_err(io::Error::from)?;
    let raw = std::os::fd::AsRawFd::as_raw_fd(&dup);
    command.args([
        ramshared_wsl2d::host_catalog::CATALOG_FD_ARG,
        &raw.to_string(),
    ]);
    // SAFETY: after fork, perform only async-signal-safe fcntl on the
    // exclusively owned duplicate descriptor held alive across spawn().
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(raw, libc::F_GETFD);
            if flags < 0 || libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(dup)
}

/// Claim and validate an inherited non-blocking host catalog stream from the
/// same parent creator that opened the worker control socket.
pub(super) fn claim_inherited_catalog(
    catalog_fd: i32,
    worker_socket: &UnixStream,
) -> io::Result<std::sync::Arc<dyn ramshared_wsl2d::host_catalog::HostGpuCatalogProvider>> {
    let stream = duplicate_inherited_socket(catalog_fd)?;
    let socket_parent = rustix::net::sockopt::socket_peercred(worker_socket)?.pid;
    let catalog_parent = rustix::net::sockopt::socket_peercred(&stream)?.pid;
    if catalog_parent != socket_parent {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "catalog socket creator does not match isolated worker socket creator",
        ));
    }
    Ok(std::sync::Arc::new(
        ramshared_wsl2d::host_catalog::WorkerHostCatalog::from_stream(stream)?,
    ))
}

/// Transfer the worker socket through the child's standard input using the
/// native process launcher. All parent descriptors retain close-on-exec.
pub(super) fn spawn_socket_child(
    command: &mut std::process::Command,
    socket: std::os::unix::net::UnixStream,
) -> std::io::Result<std::process::Child> {
    let descriptor: std::os::fd::OwnedFd = socket.into();
    command.stdin(std::process::Stdio::from(descriptor)).spawn()
}

#[cfg(test)]
mod tests;
