// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/native_cache/worker.rs
//! Inherited generation custody and request binding before driver discovery.
use super::{NativeCacheSession, load};
use crate::gpu_worker_request::WorkerRequest;
use ramshared_config::cache_configuration::{CacheConfiguration, lease_path};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

fn duplicate(raw: i32) -> io::Result<File> {
    // SAFETY: fcntl validates this numeric fd without dereferencing pointers.
    // Success gives a fresh owned descriptor distinct from standard streams.
    let fd = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful duplication above created this exclusive fd.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn clear_cloexec(raw: i32) -> io::Result<()> {
    // SAFETY: after fork or on an owned fd, perform only async-signal-safe fcntl.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn inherit(command: &mut Command, session: &NativeCacheSession) -> io::Result<()> {
    inherit_with(command, session, lease_path(), 0)
}
fn inherit_with(
    command: &mut Command,
    session: &NativeCacheSession,
    path: &Path,
    owner: u32,
) -> io::Result<()> {
    if !session.is_active() {
        return Err(io::Error::other("native cache generation is retired"));
    }
    let generation = session
        .generation()
        .ok_or_else(|| io::Error::other("approved cache generation is absent"))?;
    let lease = session
        .lease
        .as_ref()
        .ok_or_else(|| io::Error::other("native cache generation has no lease"))?;
    ramshared_exec::inherited_lease::validate(lease, path, owner)?;
    inherit_file(command, lease, generation)
}
fn inherit_file(command: &mut Command, lease: &File, generation: u64) -> io::Result<()> {
    let held = duplicate(lease.as_raw_fd())?;
    command.args([
        "--configuration-fd",
        &held.as_raw_fd().to_string(),
        "--configuration-generation",
        &generation.to_string(),
    ]);
    // SAFETY: after fork, perform only async-signal-safe fcntl on an owned fd.
    // The closure retains its File in the parent; changing the child descriptor
    // flag cannot alter the parent's descriptor flags or open-file lock.
    unsafe {
        command.pre_exec(move || clear_cloexec(held.as_raw_fd()));
    }
    Ok(())
}

pub(crate) fn admit(request: &WorkerRequest) -> Result<Option<File>, String> {
    admit_scoped(
        request,
        rustix::process::geteuid().as_raw(),
        ramshared_block::platform::probe_platform().platform,
        &ramshared_config::native_origin::default_native_origin_path(),
        lease_path(),
        0,
        load,
    )
}
fn admit_scoped(
    request: &WorkerRequest,
    euid: u32,
    platform: Option<ramshared_block::platform::PlatformKind>,
    origin_path: &Path,
    lease_file: &Path,
    owner: u32,
    load_policy: impl FnOnce(
        &ramshared_config::native_origin::NativeOriginManifest,
    ) -> Result<Option<CacheConfiguration>, String>,
) -> Result<Option<File>, String> {
    let Some((fd, generation)) = request.generation else {
        return Ok(None);
    };
    if euid != 0 || platform != Some(ramshared_block::platform::PlatformKind::NativeLinux) {
        return Err("configured cache worker requires root and measured native Linux scope".into());
    }
    super::protected_path(origin_path, owner)?;
    let origin = crate::manifest_file::read_bounded_manifest_file(origin_path, owner == 0)?;
    let origin = ramshared_config::native_origin::NativeOriginManifest::parse(
        std::str::from_utf8(&origin).map_err(|e| e.to_string())?,
    )?;
    admit_with(fd, generation, request, lease_file, owner, || {
        load_policy(&origin)?.ok_or_else(|| "approved configuration is absent".into())
    })
    .map(Some)
}
fn admit_with(
    fd: i32,
    generation: u64,
    request: &WorkerRequest,
    path: &Path,
    owner: u32,
    read: impl FnOnce() -> Result<CacheConfiguration, String>,
) -> Result<File, String> {
    let held = duplicate(fd).map_err(|e| e.to_string())?;
    ramshared_exec::inherited_lease::validate(&held, path, owner).map_err(|e| e.to_string())?;
    let policy = read()?;
    policy.validate()?;
    if policy.generation != generation {
        return Err("configured worker generation differs from current record".into());
    }
    let matches = policy
        .members
        .iter()
        .filter(|member| Some(&member.adapter) == request.expected_adapter.as_ref())
        .collect::<Vec<_>>();
    let [member] = matches.as_slice() else {
        return Err("configured worker has no unique approved member".into());
    };
    if request.config.target_bytes != member.target_bytes
        || request.config.reserve_floor_bytes != member.reserve_bytes
    {
        return Err("configured worker target or reserve differs from approval".into());
    }
    ramshared_exec::inherited_lease::validate(&held, path, owner).map_err(|e| e.to_string())?;
    // SAFETY: raw fd is a numeric input only. Preserve the inherited alias until
    // native process exit, but prevent it reaching later executable helpers.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(held)
}

#[cfg(test)]
mod tests;
