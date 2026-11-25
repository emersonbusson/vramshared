// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/unix_listener.rs
//! FD-relative Unix domain socket binding with inode pinning and backlog clamp.

use std::ffi::OsString;
use std::fs::File;
use std::os::unix::net::UnixListener;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SocketFileIdentity {
    device: u64,
    inode: u64,
    owner: u32,
}

fn socket_identity(stat: &rustix::fs::Stat) -> SocketFileIdentity {
    SocketFileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
        owner: stat.st_uid,
    }
}

pub(crate) struct SocketBindTarget {
    parent: File,
    parent_path: PathBuf,
    parent_identity: SocketFileIdentity,
    name: OsString,
}

pub(crate) fn open_socket_parent(path: &Path) -> std::io::Result<SocketBindTarget> {
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path has no filename",
            )
        })?
        .to_os_string();
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let anchor = if parent_path.is_absolute() { "/" } else { "." };
    let mut parent = File::from(
        rustix::fs::open(
            anchor,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    for component in parent_path.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "socket path contains an unsafe parent component",
                ));
            }
        };
        parent = File::from(
            rustix::fs::openat(
                &parent,
                component,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
    }
    let stat = rustix::fs::fstat(&parent).map_err(std::io::Error::from)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "socket parent is not a directory",
        ));
    }
    Ok(SocketBindTarget {
        parent,
        parent_path: parent_path.to_path_buf(),
        parent_identity: socket_identity(&stat),
        name,
    })
}

/// Validate a socket target without removing anything already present. A stale
/// pathname is still someone else's namespace entry and requires an explicit
/// operator cleanup; startup never guesses whether it is safe to unlink.
pub(crate) fn prepare_unix_socket_path(path: &Path) -> std::io::Result<SocketBindTarget> {
    let target = open_socket_parent(path)?;
    match rustix::fs::statat(
        &target.parent,
        &target.name,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Err(rustix::io::Errno::NOENT) => Ok(target),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to replace existing socket path {}",
                path.display()
            ),
        )),
        Err(error) => Err(std::io::Error::from(error)),
    }
}

/// Exact ownership token for a pathname created by this daemon. Cleanup is
/// FD-relative to the original parent and only removes the captured socket
/// device/inode/owner tuple. An ABA replacement therefore survives old cleanup.
pub(crate) struct OwnedUnixSocketPath {
    parent: File,
    /// `O_PATH` pins the original filesystem inode so an unlink/rebind cannot
    /// recycle the same dev+ino tuple while this cleanup token is alive.
    pinned: File,
    name: OsString,
    identity: SocketFileIdentity,
    pub(crate) armed: bool,
}

impl OwnedUnixSocketPath {
    pub(crate) fn remove_if_owned(&mut self) {
        if !self.armed {
            return;
        }
        let pinned_is_original = rustix::fs::fstat(&self.pinned).is_ok_and(|stat| {
            rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Socket
                && socket_identity(&stat) == self.identity
        });
        if !pinned_is_original {
            self.armed = false;
            return;
        }
        let current = rustix::fs::statat(
            &self.parent,
            &self.name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        );
        if let Ok(stat) = current
            && rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Socket
            && socket_identity(&stat) == self.identity
        {
            let _ = rustix::fs::unlinkat(&self.parent, &self.name, rustix::fs::AtFlags::empty());
            let _ = rustix::fs::fsync(&self.parent);
        }
        self.armed = false;
    }
}

impl Drop for OwnedUnixSocketPath {
    fn drop(&mut self) {
        self.remove_if_owned();
    }
}

/// Returns `min(somaxconn, tcp_max_syn_backlog)` from procfs, falling back to `128`.
pub(crate) fn system_max_backlog() -> i32 {
    static CACHED: OnceLock<i32> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let somaxconn: i32 = std::fs::read_to_string("/proc/sys/net/core/somaxconn")
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(128);
        let syn_backlog: i32 = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_max_syn_backlog")
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(128);
        std::cmp::min(somaxconn, syn_backlog)
    })
}

pub(crate) fn apply_listen_backlog<Fd: std::os::fd::AsFd>(fd: Fd) -> std::io::Result<()> {
    let system_max = system_max_backlog();
    // Limits default bound (e.g. 128 in std::net) or pushes it up to 4096 if allowed by kernel.
    let target = 4096;
    let backlog = std::cmp::min(target, system_max);
    rustix::net::listen(fd, backlog).map_err(std::io::Error::from)
}

pub(crate) fn bind_owned_unix_listener(
    path: &Path,
) -> std::io::Result<(UnixListener, OwnedUnixSocketPath)> {
    let target = prepare_unix_socket_path(path)?;
    let listener = UnixListener::bind(path)?;
    apply_listen_backlog(&listener)?;
    let stat = rustix::fs::statat(
        &target.parent,
        &target.name,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(std::io::Error::from)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Socket
        || stat.st_uid != rustix::process::geteuid().as_raw()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "bound socket has an unsafe type or owner",
        ));
    }
    let pinned = File::from(
        rustix::fs::openat(
            &target.parent,
            &target.name,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let pinned_stat = rustix::fs::fstat(&pinned).map_err(std::io::Error::from)?;
    if rustix::fs::FileType::from_raw_mode(pinned_stat.st_mode) != rustix::fs::FileType::Socket
        || socket_identity(&pinned_stat) != socket_identity(&stat)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "bound socket identity changed before it could be pinned",
        ));
    }
    let guard = OwnedUnixSocketPath {
        parent: target.parent,
        pinned,
        name: target.name,
        identity: socket_identity(&stat),
        armed: true,
    };
    let named_parent = open_socket_parent(path)?;
    if named_parent.parent_path != target.parent_path
        || named_parent.parent_identity != target.parent_identity
    {
        drop(listener);
        drop(guard);
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "socket parent identity changed during bind",
        ));
    }
    Ok((listener, guard))
}
