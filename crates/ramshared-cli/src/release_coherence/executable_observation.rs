// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/executable_observation.rs
//! File-bound executable observations for release-coherence comparison.
use std::path::{Path, PathBuf};

pub(crate) struct ObservedExecutable {
    pub(super) path: PathBuf,
    pub(super) metadata: std::fs::Metadata,
    pub(super) sha256: String,
    _file: std::fs::File,
    process: Option<std::os::fd::OwnedFd>,
}

impl ObservedExecutable {
    pub(super) fn open(path: PathBuf) -> Option<Self> {
        let mut file = crate::release_identity::open_regular_follow(&path)?;
        let metadata = file.metadata().ok().filter(|metadata| metadata.is_file())?;
        let sha256 = crate::installed_image::sha256_reader(&mut file)?;
        Some(Self {
            path,
            metadata,
            sha256,
            _file: file,
            process: None,
        })
    }

    /// Verify exact SHA-256 byte identity against `candidate`.
    pub(super) fn matches_target_bytes(&self, candidate: &Path) -> bool {
        sha256_target(candidate).is_some_and(|digest| digest == self.sha256)
    }

    pub(super) fn open_process(pid: u32) -> Option<Self> {
        use rustix::process::{Pid, PidfdFlags, pidfd_open};
        let pidfd = pidfd_open(
            Pid::from_raw(i32::try_from(pid).ok()?)?,
            PidfdFlags::empty(),
        )
        .ok()?;
        if !process_alive(&pidfd) {
            return None;
        }
        let mut image = Self::open(PathBuf::from(format!("/proc/{pid}/exe")))?;
        if !process_alive(&pidfd) {
            return None;
        }
        image.process = Some(pidfd);
        Some(image)
    }

    pub(super) fn still_matches(&self) -> bool {
        if self
            .process
            .as_ref()
            .is_some_and(|pidfd| !process_alive(pidfd))
        {
            return false;
        }
        std::fs::metadata(&self.path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .is_some_and(|metadata| {
                crate::installed_image::same_file_identity(&self.metadata, &metadata)
            })
            && sha256_target(&self.path).is_some_and(|digest| digest == self.sha256)
            && self.process.as_ref().is_none_or(process_alive)
    }
}

fn sha256_target(path: &Path) -> Option<String> {
    let mut file = crate::release_identity::open_regular_follow(path)?;
    crate::installed_image::sha256_reader(&mut file)
}

fn process_alive(pidfd: &std::os::fd::OwnedFd) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    matches!(poll(&mut fds, Some(&zero)), Ok(0)) && fds[0].revents().is_empty()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn executable_observation_refuses_fifo_without_waiting() {
        use rustix::fs::{CWD, Mode, OFlags, mkfifoat, open};
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = std::env::temp_dir().join(format!(
            "ramshared-observed-executable-fifo-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture directory");
        let fifo = dir.join("executable");
        mkfifoat(CWD, &fifo, Mode::RUSR | Mode::WUSR).expect("create writerless FIFO");
        for initial in [true, false] {
            let (sender, receiver) = mpsc::sync_channel(1);
            let path = fifo.clone();
            let worker = std::thread::spawn(move || {
                let refused = if initial {
                    ObservedExecutable::open(path).is_none()
                } else {
                    sha256_target(&path).is_none()
                };
                let _ = sender.send(refused);
            });
            let result = receiver.recv_timeout(Duration::from_secs(2));
            if result.is_err() {
                // Release a regressed blocking open before reporting failure.
                if let Ok(writer) = open(&fifo, OFlags::WRONLY | OFlags::NONBLOCK, Mode::empty()) {
                    drop(writer);
                    let _ = worker.join();
                }
                panic!("executable observation waited for a FIFO writer");
            }
            worker.join().expect("join FIFO observation");
            assert!(result.expect("receive refusal"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pinned_owned_child_exit_revokes_observation() {
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = OwnedChild(
            std::process::Command::new("/usr/bin/sleep")
                .arg("60")
                .spawn()
                .expect("spawn owned child"),
        );
        let image = ObservedExecutable::open_process(child.0.id()).expect("pin child image");
        assert!(image.still_matches());
        child.0.kill().expect("stop owned child");
        child.0.wait().expect("reap owned child");
        assert!(!process_alive(
            image.process.as_ref().expect("retained pidfd")
        ));
        assert!(!image.still_matches());
        assert!(ObservedExecutable::open_process(0).is_none());
    }

    #[test]
    fn in_place_byte_modification_revokes_observed_executable() {
        let dir =
            std::env::temp_dir().join(format!("ramshared-obs-exe-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("bin");
        let target = dir.join("target");
        std::fs::write(&path, b"version-a-binary").expect("write v1");
        std::fs::write(&target, b"version-a-binary").expect("write target v1");
        let obs = ObservedExecutable::open(path.clone()).expect("open");
        assert!(obs.still_matches());
        assert!(obs.matches_target_bytes(&target));

        std::fs::write(&target, b"version-b-binary").expect("mutate target");
        assert!(!obs.matches_target_bytes(&target));

        std::fs::write(&path, b"version-b-binary").expect("mutate in place");
        assert!(!obs.still_matches());
        let _ = std::fs::remove_dir_all(dir);
    }
}
