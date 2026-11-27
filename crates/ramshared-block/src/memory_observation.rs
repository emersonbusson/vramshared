// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/memory_observation.rs
//! Linux memory observations for admission. The cgroup result is explicitly
//! limited to the visible namespace and is not proof of host-wide ancestry.
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::path::Path;

use rustix::fs::{Mode, OFlags, open, openat};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CgroupIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CgroupSample {
    pub identity: CgroupIdentity,
    pub limit_bytes: Option<u64>,
    pub current_bytes: Option<u64>,
    pub sampled_at_unix_ms: u64,
}

const MAX_TEXT_BYTES: u64 = 64 * 1024;
const MAX_CGROUP_DEPTH: usize = 128;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_bounded(file: File) -> io::Result<String> {
    let mut bytes = Vec::new();
    file.take(MAX_TEXT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TEXT_BYTES {
        return Err(invalid("memory observation exceeds limit"));
    }
    String::from_utf8(bytes).map_err(|_| invalid("memory observation is not UTF-8"))
}

/// Parse one cgroup-v2 membership. Reject ambiguous or escaping paths.
fn cgroup_components(text: &str) -> io::Result<Vec<&str>> {
    let mut unified = text.lines().filter_map(|line| line.strip_prefix("0::"));
    let path = unified
        .next()
        .ok_or_else(|| invalid("cgroup v2 membership missing"))?;
    if unified.next().is_some() || !path.starts_with('/') || path.len() > 4096 {
        return Err(invalid("ambiguous cgroup membership"));
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    let components: Vec<_> = path[1..].split('/').collect();
    if components.len() > MAX_CGROUP_DEPTH
        || components.iter().any(|part| {
            part.is_empty() || *part == "." || *part == ".." || part.as_bytes().contains(&0)
        })
    {
        return Err(invalid("invalid cgroup path"));
    }
    Ok(components)
}

fn metric(directory: impl AsFd, name: &str) -> io::Result<String> {
    let fd = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    read_bounded(File::from(fd))
}

fn is_global_root(directory: impl AsFd) -> bool {
    // These controls exist only below the kernel's global cgroup v2 root.
    let absent =
        |name| metric(&directory, name).is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
    absent("cgroup.type")
        && absent("memory.current")
        && metric(&directory, "cgroup.controllers")
            .is_ok_and(|controllers| controllers.split_whitespace().any(|name| name == "memory"))
}

fn sample_group(
    directory: impl AsFd,
    sampled: u64,
    at_mount_root: bool,
) -> io::Result<CgroupSample> {
    let stat = rustix::fs::fstat(&directory)?;
    let identity = CgroupIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    };
    let maximum = match metric(&directory, "memory.max") {
        Ok(maximum) => maximum,
        Err(error)
            if at_mount_root
                && error.kind() == io::ErrorKind::NotFound
                && is_global_root(&directory) =>
        {
            return Ok(CgroupSample {
                identity,
                limit_bytes: None,
                current_bytes: None,
                sampled_at_unix_ms: sampled,
            });
        }
        Err(error) => return Err(error),
    };
    let current = metric(&directory, "memory.current")?;
    let limit_bytes = match maximum.trim() {
        "max" => None,
        value => Some(
            value
                .parse::<u64>()
                .map_err(|_| invalid("invalid memory.max"))?,
        ),
    };
    let current_bytes = current
        .trim()
        .parse::<u64>()
        .map_err(|_| invalid("invalid memory.current"))?;
    Ok(CgroupSample {
        identity,
        limit_bytes,
        current_bytes: Some(current_bytes),
        sampled_at_unix_ms: sampled,
    })
}

/// Walk the visible v2 hierarchy from its root to this process's cgroup.
/// Callers must independently prove that this root exposes every applicable
/// ancestor, and must revalidate process membership before admitting memory.
pub fn visible_cgroups(
    root: &Path,
    membership: &str,
    sampled_at_unix_ms: u64,
) -> io::Result<Vec<CgroupSample>> {
    visible_cgroups_inner(root, membership, sampled_at_unix_ms, true)
}

fn visible_cgroups_inner(
    root: &Path,
    membership: &str,
    sampled_at_unix_ms: u64,
    require_cgroupfs: bool,
) -> io::Result<Vec<CgroupSample>> {
    let components = cgroup_components(membership)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = open(root, flags, Mode::empty())?;
    let check_filesystem = |fd: &std::os::fd::OwnedFd| -> io::Result<()> {
        const CGROUP2_SUPER_MAGIC: u32 = 0x6367_7270;
        if require_cgroupfs && rustix::fs::fstatfs(fd)?.f_type as u32 != CGROUP2_SUPER_MAGIC {
            return Err(invalid("cgroup root is not cgroup v2"));
        }
        Ok(())
    };
    check_filesystem(&directory)?;
    let mut samples = Vec::with_capacity(components.len() + 1);
    samples.push(sample_group(&directory, sampled_at_unix_ms, true)?);
    for component in components {
        directory = openat(&directory, component, flags, Mode::empty())?;
        check_filesystem(&directory)?;
        samples.push(sample_group(&directory, sampled_at_unix_ms, false)?);
    }
    Ok(samples)
}

#[cfg(test)]
mod tests;
