// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/product_online/gates.rs
//! Volume lock, wipe, and gate adapters for `product_online`.

use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use ramshared_block::VramBackend;

use crate::host_safety::pagefile_may_target_volume;
use crate::service::{DiskControl, PagefileGates, TeardownTarget, WipeVram};
use crate::windows_driver::WindowsDriverLink;
use crate::windows_host::WindowsHostState;

use super::teardown::teardown_diag;

pub(super) struct LinkDisk<'a> {
    pub(super) link: &'a mut WindowsDriverLink,
    pub(super) unregistered: bool,
    pub(super) destroyed: bool,
}

impl DiskControl for LinkDisk<'_> {
    fn create_disk(&mut self, _: u64, _: u32) -> Result<(), String> {
        Ok(())
    }

    fn destroy_disk(&mut self) -> Result<(), String> {
        if self.destroyed {
            return Ok(());
        }
        self.link.destroy_disk().map_err(|e| e.to_string())?;
        self.destroyed = true;
        Ok(())
    }

    fn register_queue(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn unregister_queue(&mut self) -> Result<(), String> {
        if self.unregistered {
            return Ok(());
        }
        self.link.unregister_queue().map_err(|e| e.to_string())?;
        self.unregistered = true;
        Ok(())
    }
}

pub(super) struct BackendWipe<'a, M> {
    pub(super) backend: &'a mut VramBackend<M>,
}

impl<M: ramshared_vram::VramMemory> WipeVram for BackendWipe<'_, M> {
    fn zero(&mut self) -> Result<(), String> {
        self.backend.zero().map_err(|e| e.to_string())
    }
}

pub(super) struct HostGates {
    locked: Option<crate::windows_host::LockedVolume>,
    volume_letter: char,
    volume_mount_path: Option<std::path::PathBuf>,
    volume_device_path: Option<String>,
    raw_disk: bool,
    /// Validated at construction (letter/serial/size shape); kept for diagnostics.
    target: TeardownTarget,
    target_serial: String,
    target_size: u64,
    identity_verified: bool,
    expected_disk_number: Option<u32>,
    pagefile_snapshot: Mutex<Option<Vec<String>>>,
}

impl HostGates {
    pub(super) fn new(
        volume_letter: char,
        volume_mount_path: Option<std::path::PathBuf>,
        serial: &str,
        size_bytes: u64,
    ) -> Result<Self, String> {
        Ok(Self {
            locked: None,
            volume_letter,
            volume_mount_path,
            volume_device_path: None,
            raw_disk: false,
            target: TeardownTarget::new(volume_letter, serial, size_bytes)?,
            target_serial: serial.to_string(),
            target_size: size_bytes,
            identity_verified: false,
            expected_disk_number: None,
            pagefile_snapshot: Mutex::new(None),
        })
    }

    fn target_serial(&self) -> &str {
        &self.target_serial
    }

    fn target_size(&self) -> u64 {
        self.target_size
    }

    pub(super) fn accept_observed_identity(
        &mut self,
        observed: crate::service::ObservedVolumeIdentity,
        disk_number: u32,
        volume_device_path: Option<String>,
    ) -> Result<(), String> {
        self.target.verify_unique(&[observed])?;
        if let Some(path) = volume_device_path.as_deref()
            && (!path.starts_with(r"\\?\Volume{") || !path.ends_with('}'))
        {
            return Err("observed volume device path is invalid".into());
        }
        self.identity_verified = true;
        self.expected_disk_number = Some(disk_number);
        self.raw_disk = volume_device_path.is_none();
        self.volume_device_path = volume_device_path;
        Ok(())
    }

    pub(super) fn expected_disk_number(&self) -> u32 {
        self.expected_disk_number.unwrap_or(u32::MAX)
    }

    pub(super) fn volume_device_path(&self) -> Option<&str> {
        self.volume_mount_path
            .as_ref()
            .and(self.volume_device_path.as_deref())
    }

    pub(super) fn filter_pagefiles(
        &self,
        rows: Vec<crate::windows_host::PagefileIdentity>,
    ) -> Result<Vec<String>, String> {
        rows.into_iter().try_fold(Vec::new(), |mut found, row| {
            if pagefile_may_target_volume(&row.name, self.volume_letter)? {
                found.push(row.name);
            }
            Ok(found)
        })
    }

    pub(super) fn cache_pagefiles(&self, rows: Vec<String>) {
        if let Ok(mut snapshot) = self.pagefile_snapshot.lock() {
            *snapshot = Some(rows);
        }
    }

    /// Install a volume lock obtained while the I/O pump was still running.
    pub(super) fn take_locked(&mut self, vol: crate::windows_host::LockedVolume) {
        self.volume_letter = vol.letter;
        self.locked = Some(vol);
    }
}

impl PagefileGates for HostGates {
    fn verify_volume_identity(&self, letter: char) -> Result<(), String> {
        teardown_diag(&format!(
            "teardown phase=Identity letter={letter} serial={} size={}",
            self.target_serial(),
            self.target_size()
        ));
        if letter.to_ascii_uppercase() != self.volume_letter || !self.identity_verified {
            let msg = "live letter-to-disk identity was not verified".to_string();
            teardown_diag(&format!("teardown phase=Identity FAIL: {msg}"));
            return Err(msg);
        }
        teardown_diag(&format!(
            "teardown phase=Identity OK (live exact) serial={} size={}",
            self.target_serial(),
            self.target_size()
        ));
        Ok(())
    }

    fn active_pagefiles(&self) -> Result<Vec<String>, String> {
        // Storage-only DT-8: refuse only pagefiles on the product volume letter,
        // not the system C: pagefile (which is expected on a normal host).
        let letter = self
            .locked
            .as_ref()
            .map(|v| v.letter)
            .unwrap_or(self.volume_letter);
        let gate = if self.locked.is_some() {
            "GateB"
        } else {
            "GateA"
        };
        teardown_diag(&format!(
            "teardown phase={gate} pagefile query letter={letter}"
        ));
        if let Ok(mut snapshot) = self.pagefile_snapshot.lock() {
            if let Some(filtered) = snapshot.take() {
                teardown_diag(&format!(
                    "teardown phase={gate} pagefiles_on_volume={}",
                    filtered.len()
                ));
                return Ok(filtered);
            }
        } else {
            return Err("pagefile snapshot lock poisoned".into());
        }
        WindowsHostState::active_pagefiles()
            .map_err(|e| e.to_string())
            .and_then(|v| self.filter_pagefiles(v))
            .map_err(|e| {
                teardown_diag(&format!("teardown phase={gate} FAIL: {e}"));
                e.to_string()
            })
    }

    fn has_mounted_volume(&self) -> bool {
        !self.raw_disk
    }

    fn lock_volume(&mut self, letter: char) -> Result<(), String> {
        self.volume_letter = letter.to_ascii_uppercase();
        if self.locked.is_some() {
            return Ok(());
        }
        teardown_diag(&format!(
            "teardown phase=VolumeLock begin letter={}",
            self.volume_letter
        ));
        // Brief settle so filesystem handles from the last I/O round close.
        thread::sleep(Duration::from_millis(100));
        // Direct lock (no helper-thread timeout). Abandoned CreateFile threads
        // from timed-out attempts wedged the volume; pre-stop harness lock_ok
        // proves CreateFile returns promptly when the stack is not piled up.
        let result = if let Some(path) = self.volume_device_path() {
            WindowsHostState::lock_product_volume_path(
                path,
                self.volume_letter,
                self.expected_disk_number,
            )
        } else {
            WindowsHostState::lock_product_volume(self.volume_letter, self.expected_disk_number)
        };
        match result {
            Ok(vol) => {
                self.locked = Some(vol);
                teardown_diag("teardown phase=VolumeLock OK");
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                teardown_diag(&format!("teardown phase=VolumeLock FAIL: {msg}"));
                Err(msg)
            }
        }
    }

    fn unlock_volume(&mut self) -> Result<(), String> {
        // Drop LockedVolume → FSCTL_UNLOCK + CloseHandle (DT-8).
        teardown_diag("teardown phase=Unlock");
        self.locked = None;
        Ok(())
    }

    fn flush_and_dismount(&mut self) -> Result<(), String> {
        teardown_diag("teardown phase=FlushDismount begin");
        let vol = self
            .locked
            .as_ref()
            .ok_or_else(|| "volume is not exclusively locked".to_string())?;
        match WindowsHostState::flush_and_dismount(vol) {
            Ok(()) => {
                teardown_diag("teardown phase=FlushDismount OK");
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                teardown_diag(&format!("teardown phase=FlushDismount FAIL: {msg}"));
                Err(msg)
            }
        }
    }

    fn volume_locked(&self) -> bool {
        self.locked.is_some()
    }
}
