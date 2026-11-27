// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/effect_binding/nbd_probe_lock.rs
//! Coordinate disconnect of an exact, bound NBD with cooperating block probes.

use super::{CascadeError, CommandRunner, EffectBoundDevice, prove_exact_swap_absent};
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use std::fs::File;
use std::io;
use std::thread::sleep;
use std::time::{Duration, Instant};

#[cfg(not(test))]
use std::os::unix::fs::MetadataExt;

const PROBE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);

impl EffectBoundDevice {
    /// The pinned file holds an exclusive BSD lock for the client call. It
    /// closes on return so the kernel can finish releasing the NBD config
    /// before the caller evaluates its terminal state.
    pub(in crate::cascade::cascade_io) fn disconnect_nbd(
        self,
        runner: &impl CommandRunner,
    ) -> Result<(), CascadeError> {
        #[cfg(not(test))]
        {
            lock_nbd_probe(&self.file, Duration::from_secs(2)).map_err(|error| {
                CascadeError::UnsafeContainment(format!(
                    "NBD probe lock unavailable for {} ({error}); backend preserved",
                    self.identity.path
                ))
            })?;
            self.prove_locked_node_is_named()?;
        }
        self.revalidate_named_identity()?;
        prove_exact_swap_absent(&self.identity)?;
        runner.run("nbd-client", &["-d", &self.identity.path])?;
        Ok(())
    }

    #[cfg(not(test))]
    fn prove_locked_node_is_named(&self) -> Result<(), CascadeError> {
        let inspect = |error| {
            CascadeError::Precondition(format!(
                "cannot verify locked NBD device node {} ({error})",
                self.identity.path
            ))
        };
        let pinned = self.file.metadata().map_err(inspect)?;
        let named = std::fs::symlink_metadata(&self.identity.path).map_err(inspect)?;
        if (pinned.dev(), pinned.ino()) != (named.dev(), named.ino()) {
            return Err(CascadeError::Precondition(format!(
                "locked NBD device node was replaced: {}",
                self.identity.path
            )));
        }
        Ok(())
    }
}

fn lock_nbd_probe(file: &File, timeout: Duration) -> io::Result<()> {
    let started = Instant::now();
    loop {
        match flock(file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(Errno::WOULDBLOCK | Errno::INTR) => (),
            Err(error) => return Err(error.into()),
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        sleep(PROBE_LOCK_POLL_INTERVAL.min(remaining));
        if started.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "waiting for a cooperating block probe",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascade::cascade_io::tests_suite::TestDir;
    use std::error::Error;

    #[test]
    fn exclusive_lock_is_held_until_descriptor_close() -> Result<(), Box<dyn Error>> {
        let directory = TestDir::new();
        let path = directory.path.join("block-device");
        File::create(&path)?;
        let device = File::open(&path)?;
        let probe = File::open(&path)?;
        lock_nbd_probe(&device, Duration::ZERO)?;
        assert_eq!(
            flock(&probe, FlockOperation::NonBlockingLockShared),
            Err(Errno::WOULDBLOCK)
        );
        drop(device);
        flock(&probe, FlockOperation::NonBlockingLockShared)?;
        Ok(())
    }

    #[test]
    fn probe_contention_times_out_without_stealing_shared_lock() -> Result<(), Box<dyn Error>> {
        let directory = TestDir::new();
        let path = directory.path.join("block-device");
        File::create(&path)?;
        let probe = File::open(&path)?;
        let device = File::open(&path)?;
        flock(&probe, FlockOperation::NonBlockingLockShared)?;
        assert!(matches!(
            lock_nbd_probe(&device, Duration::from_millis(5)),
            Err(error) if error.kind() == io::ErrorKind::TimedOut
        ));
        let observer = File::open(&path)?;
        flock(&observer, FlockOperation::NonBlockingLockShared)?;
        drop(observer);
        drop(probe);
        lock_nbd_probe(&device, Duration::ZERO)?;
        Ok(())
    }
}
