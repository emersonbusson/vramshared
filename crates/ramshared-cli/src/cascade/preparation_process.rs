// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/preparation_process.rs
//! Conservative process observation before storage preparation.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const PROC_FIELD_LIMIT: u64 = 16 * 1024;
const PF_KTHREAD: u64 = 0x0020_0000;

/// Pin the production process filesystem before trusting a census.
pub(super) fn open_verified_procfs(path: &Path) -> io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags, PROC_SUPER_MAGIC, fstatfs, open};
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    if fstatfs(&fd).map_err(io::Error::from)?.f_type != PROC_SUPER_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process root is not procfs",
        ));
    }
    Ok(fs::File::from(fd))
}

/// Refuse if the pathname stopped naming the pinned root during observation.
pub(super) fn verify_procfs_path(path: &Path, pinned: &fs::File) -> io::Result<()> {
    let named = fs::symlink_metadata(path)?;
    let held = pinned.metadata()?;
    if !named.is_dir() || (named.dev(), named.ino()) != (held.dev(), held.ino()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process root changed",
        ));
    }
    Ok(())
}

fn bounded_text(path: &Path) -> io::Result<String> {
    let mut value = String::new();
    fs::File::open(path)?
        .take(PROC_FIELD_LIMIT + 1)
        .read_to_string(&mut value)?;
    if value.len() as u64 > PROC_FIELD_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process field too large",
        ));
    }
    Ok(value)
}

fn process_stat(text: &str, pid: u32) -> io::Result<(char, u64, u64)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid process stat");
    if !text.starts_with(&format!("{pid} (")) {
        return Err(invalid());
    }
    let fields: Vec<_> = text
        .rsplit_once(") ")
        .ok_or_else(invalid)?
        .1
        .split_whitespace()
        .collect();
    let state = fields
        .first()
        .and_then(|s| (s.len() == 1).then(|| s.chars().next()).flatten())
        .ok_or_else(invalid)?;
    if !matches!(state, 'R' | 'S' | 'D' | 'T' | 't' | 'Z' | 'X' | 'I' | 'W') {
        return Err(invalid());
    }
    let flags = fields
        .get(6)
        .and_then(|s| s.parse().ok())
        .ok_or_else(invalid)?;
    let start = fields
        .get(19)
        .and_then(|s| s.parse().ok())
        .ok_or_else(invalid)?;
    Ok((state, flags, start))
}

/// A positive result is a veto, not an ownership proof. Unknown entries fail closed.
#[cfg(test)]
pub(super) fn possible_daemon_present(proc_root: &Path) -> io::Result<bool> {
    possible_daemon_present_with_identity(proc_root, None)
}

pub(super) fn possible_daemon_present_with_identity(
    proc_root: &Path,
    daemon_identity: Option<(u64, u64)>,
) -> io::Result<bool> {
    let root = fs::symlink_metadata(proc_root)?;
    if !root.is_dir() || root.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid process root",
        ));
    }
    for entry in fs::read_dir(proc_root)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|pid| *pid != 0)
        else {
            continue;
        };
        let base = entry.path();
        let observation = (|| -> io::Result<bool> {
            let before = process_stat(&bounded_text(&base.join("stat"))?, pid)?;
            if before.0 == 'Z' || before.0 == 'X' || before.1 & PF_KTHREAD != 0 {
                let after = process_stat(&bounded_text(&base.join("stat"))?, pid)?;
                if before != after {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "process changed",
                    ));
                }
                return Ok(false);
            }
            let comm = bounded_text(&base.join("comm"))?;
            if comm.trim_end() == "ramsharedd" {
                return Ok(true);
            }
            let executable = fs::read_link(base.join("exe"))?;
            let text = executable.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid executable path")
            })?;
            let text = text.strip_suffix(" (deleted)").unwrap_or(text);
            if !Path::new(text).is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "relative executable path",
                ));
            }
            let mut matched = Path::new(text)
                .file_name()
                .is_some_and(|name| name == "ramsharedd");
            let exe_identity = if let Some(identity) = daemon_identity {
                let metadata = fs::metadata(base.join("exe"))?;
                let current = (metadata.dev(), metadata.ino());
                matched |= current == identity;
                Some(current)
            } else {
                None
            };
            if bounded_text(&base.join("comm"))? != comm
                || fs::read_link(base.join("exe"))? != executable
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "process executable changed",
                ));
            }
            if let Some(identity) = exe_identity {
                let metadata = fs::metadata(base.join("exe"))?;
                if (metadata.dev(), metadata.ino()) != identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "process executable identity changed",
                    ));
                }
            }
            let after = process_stat(&bounded_text(&base.join("stat"))?, pid)?;
            if before.2 != after.2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "process replaced",
                ));
            }
            Ok(matched)
        })();
        match observation {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && matches!(fs::symlink_metadata(&base), Err(ref e) if e.kind() == io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn production_census_rejects_non_procfs_and_redirects() {
        let root =
            std::env::temp_dir().join(format!("ramshared-prep-procfs-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        assert!(open_verified_procfs(&root).is_err());
        let alias = root.join("redirect");
        symlink("/proc", &alias).unwrap();
        assert!(open_verified_procfs(&alias).is_err());
        let procfs = open_verified_procfs(Path::new("/proc")).unwrap();
        assert!(verify_procfs_path(Path::new("/proc"), &procfs).is_ok());
        assert!(verify_procfs_path(&root, &procfs).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detects_daemon_executable_through_a_hard_link() {
        use std::os::unix::fs::MetadataExt;
        let root =
            std::env::temp_dir().join(format!("ramshared-prep-alias-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let daemon = root.join("ramsharedd");
        let alias = root.join("alternate-name");
        std::fs::write(&daemon, b"fixture").unwrap();
        std::fs::hard_link(&daemon, &alias).unwrap();
        let metadata = std::fs::metadata(&daemon).unwrap();
        let proc = root.join("42");
        std::fs::create_dir(&proc).unwrap();
        std::fs::write(
            proc.join("stat"),
            "42 (alternate) S 1 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 0 123\n",
        )
        .unwrap();
        std::fs::write(proc.join("comm"), "alternate\n").unwrap();
        symlink(&alias, proc.join("exe")).unwrap();
        assert!(
            possible_daemon_present_with_identity(&root, Some((metadata.dev(), metadata.ino())))
                .unwrap()
        );
        assert!(
            !possible_daemon_present_with_identity(
                &root,
                Some((metadata.dev(), metadata.ino() + 1))
            )
            .unwrap()
        );
        std::fs::remove_file(&alias).unwrap();
        assert!(
            possible_daemon_present_with_identity(&root, Some((metadata.dev(), metadata.ino())))
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detects_unregistered_daemon_and_refuses_unobservable_process() {
        let root = std::env::temp_dir().join(format!("ramshared-prep-proc-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        assert!(!possible_daemon_present(&root).unwrap());
        let proc = root.join("42");
        std::fs::create_dir(&proc).unwrap();
        std::fs::write(
            proc.join("stat"),
            "42 (ramsharedd) S 1 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 0 123\n",
        )
        .unwrap();
        std::fs::write(proc.join("comm"), "ramsharedd\n").unwrap();
        assert!(possible_daemon_present(&root).unwrap());
        std::fs::write(proc.join("comm"), "other\n").unwrap();
        symlink(
            "/opt/ramshared/releases/v1/bin/ramsharedd (deleted)",
            proc.join("exe"),
        )
        .unwrap();
        assert!(possible_daemon_present(&root).unwrap());
        std::fs::remove_file(proc.join("exe")).unwrap();
        assert!(possible_daemon_present(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignores_verified_non_daemon_kernel_threads_and_zombies() {
        let root =
            std::env::temp_dir().join(format!("ramshared-prep-proc-clean-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("not-a-pid")).unwrap();
        let proc = root.join("42");
        std::fs::create_dir(&proc).unwrap();
        let stat = |state: &str, flags: u64| {
            format!("42 (fixture) {state} 1 1 1 0 0 {flags} 0 0 0 0 0 0 0 0 0 0 0 1 0 123\n")
        };
        std::fs::write(proc.join("stat"), stat("S", 0)).unwrap();
        std::fs::write(proc.join("comm"), "other\n").unwrap();
        symlink("/usr/bin/other", proc.join("exe")).unwrap();
        assert!(!possible_daemon_present(&root).unwrap());
        std::fs::remove_file(proc.join("exe")).unwrap();
        symlink("relative-program", proc.join("exe")).unwrap();
        assert!(possible_daemon_present(&root).is_err());
        std::fs::remove_file(proc.join("exe")).unwrap();
        use std::os::unix::ffi::OsStringExt;
        symlink(
            std::ffi::OsString::from_vec(vec![b'/', 0xff]),
            proc.join("exe"),
        )
        .unwrap();
        assert!(possible_daemon_present(&root).is_err());
        std::fs::remove_file(proc.join("exe")).unwrap();
        std::fs::write(proc.join("stat"), stat("Z", 0)).unwrap();
        assert!(!possible_daemon_present(&root).unwrap());
        std::fs::write(proc.join("stat"), stat("S", PF_KTHREAD)).unwrap();
        assert!(!possible_daemon_present(&root).unwrap());
        std::fs::write(proc.join("stat"), "invalid").unwrap();
        assert!(possible_daemon_present(&root).is_err());
        std::fs::write(proc.join("stat"), stat("?", 0)).unwrap();
        assert!(possible_daemon_present(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_redirected_root_and_oversized_process_fields() {
        let root = std::env::temp_dir().join(format!(
            "ramshared-prep-proc-invalid-{}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let alias = root.with_extension("link");
        symlink(&root, &alias).unwrap();
        assert!(possible_daemon_present(&alias).is_err());
        let proc = root.join("9");
        std::fs::create_dir(&proc).unwrap();
        std::fs::write(proc.join("stat"), vec![b'x'; PROC_FIELD_LIMIT as usize + 1]).unwrap();
        assert!(possible_daemon_present(&root).is_err());
        std::fs::remove_file(alias).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "read-only live proc observation; host-dependent"]
    fn live_preparation_process_census_is_observable() {
        let observed = possible_daemon_present(Path::new("/proc"));
        println!("live process census: {observed:?}");
        assert!(observed.is_ok(), "live process census is unavailable");
    }
}
