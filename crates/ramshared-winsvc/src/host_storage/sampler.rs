// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/sampler.rs
//! Independent native WSL2 storage observations; bounded child custody per disk ID.
use super::HostStorageReport;
use crate::native_sampler::{NativeSampler, ProbeKind};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_DISK_SAMPLERS: usize = 8;
const POLL: Duration = Duration::from_millis(10);

static OWNER: NativeSampler<HostStorageReport> = NativeSampler::new();
static PRODUCT_EXE: Mutex<Option<PathBuf>> = Mutex::new(None);
static DISK_SAMPLERS: Mutex<Vec<([u8; 16], &'static NativeSampler<HostStorageReport>)>> =
    Mutex::new(Vec::new());

pub fn configure_product_executable(executable: PathBuf) {
    if !executable.is_absolute() || !executable.is_file() {
        return;
    }
    *PRODUCT_EXE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(executable);
}

pub fn start(executable: PathBuf, file: PathBuf) -> Result<(), String> {
    OWNER.start(executable, ProbeKind::Storage(file))
}

pub fn observation() -> Option<HostStorageReport> {
    OWNER.observation()
}

pub fn diagnostic() -> Option<String> {
    OWNER.diagnostic()
}

pub fn sample_for_disk(
    disk_id: [u8; 16],
    max_wait: Duration,
) -> Option<(HostStorageReport, Instant)> {
    let (sampler, newly_created) = sampler_for_disk(disk_id)?;
    let check = || {
        sampler
            .observation_with_start()
            .filter(|(report, _)| report.virtual_disk_id == Some(disk_id))
    };
    if let Some(hit) = check() {
        return Some(hit);
    }
    if !newly_created || max_wait.is_zero() {
        return None;
    }
    let deadline = Instant::now().checked_add(max_wait)?;
    loop {
        if let Some(hit) = check() {
            return Some(hit);
        }
        if sampler.stopped() || Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

fn sampler_for_disk(
    disk_id: [u8; 16],
) -> Option<(&'static NativeSampler<HostStorageReport>, bool)> {
    let mut list = DISK_SAMPLERS.lock().ok()?;
    if let Some((_, existing)) = list.iter().find(|(id, _)| *id == disk_id) {
        return Some((*existing, false));
    }
    if list.len() >= MAX_DISK_SAMPLERS {
        return None;
    }
    let executable = PRODUCT_EXE.lock().ok()?.clone()?;
    let sampler: &'static NativeSampler<HostStorageReport> =
        Box::leak(Box::new(NativeSampler::new()));
    if sampler
        .start(executable, ProbeKind::StorageDisk(disk_id))
        .is_err()
    {
        return None;
    }
    list.push((disk_id, sampler));
    Some((sampler, true))
}

pub fn stop_and_wait() -> Result<(), String> {
    let mut result = OWNER.stop_and_wait();
    let samplers: Vec<&'static NativeSampler<HostStorageReport>> = {
        let mut list = DISK_SAMPLERS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *list)
            .into_iter()
            .map(|(_, s)| s)
            .collect()
    };
    *PRODUCT_EXE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
    for sampler in samplers {
        if let Err(err) = sampler.stop_and_wait()
            && result.is_ok()
        {
            result = Err(err);
        }
    }
    result
}
