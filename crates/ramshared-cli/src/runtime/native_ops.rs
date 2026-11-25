// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/runtime/native_ops.rs
// Native mutating lifecycle entrypoints (SPEC zero-script-native-runtime
// ITEM-3). Each entrypoint validates its inputs, performs the native syscall
// or sysfs transaction, and leaves the caller holding the same evidence it
// had before: success is never inferred from a subprocess exit code.

use std::path::Path;
use std::time::Duration;

use rustix::io::Errno;

use super::linux_swap_sys::{
    NativeLifecycleError, activate_swap_native, deactivate_swap_native, format_swap_device_native,
};
use super::preflight_procfs::ProcfsPreflight;
use super::zram_sysfs::{ZramError, ZramSysfs};

const ZRAM_CONFIGURE_RETRIES: u32 = 20;
const ZRAM_CONFIGURE_BACKOFF: Duration = Duration::from_millis(100);

/// Drain the target through the native `swapoff(2)` path.
///
/// `deactivate_swap_native` re-reads `/proc/meminfo` and `/proc/swaps` so the
/// feasibility gate and the syscall see the same snapshot; a refused gate
/// never reaches the kernel.
pub fn execute_native_swapoff(
    target: &Path,
    absorber: Option<&ramshared_block::swap_absorber::SwapAbsorber>,
) -> Result<(), NativeLifecycleError> {
    let mut preflight = ProcfsPreflight::new();
    deactivate_swap_native(target, &mut preflight, absorber)
}

/// Format the swap v1 header in place, then activate it with `swapon(2)`.
///
/// A failed activation scrubs the magic inside `activate_swap_with`, so an
/// unformatted or unarmed device is never picked up by `swapon -a`.
pub fn execute_native_swapon(
    target: &Path,
    total_bytes: u64,
    uuid: [u8; 16],
    priority: u16,
) -> Result<(), NativeLifecycleError> {
    format_swap_device_native(target, total_bytes, uuid)?;
    activate_swap_native(target, priority)
}

/// Activate an already formatted swap device through `swapon(2)`.
pub fn execute_native_swap_activate(
    target: &Path,
    priority: u16,
) -> Result<(), NativeLifecycleError> {
    activate_swap_native(target, priority)
}

/// Configure zram writeback through the pinned sysfs descriptor. A disksize
/// failure rolls the device back before the error is returned.
pub fn execute_native_zram_configure(
    zram_id: u32,
    backing_dev: &Path,
    disksize_bytes: u64,
) -> Result<(), ZramError> {
    configure_zram(ZramSysfs::open, zram_id, backing_dev, disksize_bytes)
}

/// Reset one zram device through its pinned sysfs descriptor.
pub fn execute_native_zram_reset(zram_id: u32) -> Result<(), ZramError> {
    reset_zram(ZramSysfs::open, zram_id)
}

/// Shared configure body. The opener is a parameter so the sysfs transaction
/// is testable against a pinned temporary root without a real zram device.
fn configure_zram<F>(
    open: F,
    zram_id: u32,
    backing_dev: &Path,
    disksize_bytes: u64,
) -> Result<(), ZramError>
where
    F: FnOnce(u32) -> Result<ZramSysfs, ZramError>,
{
    let zram = open(zram_id)?;
    retry_busy(
        || zram.configure(backing_dev, disksize_bytes),
        ZRAM_CONFIGURE_RETRIES,
        || std::thread::sleep(ZRAM_CONFIGURE_BACKOFF),
    )
}

/// A newly allocated zram device can be briefly claimed by a udev probe when
/// its backing device is attached; the kernel then answers `EBUSY` for the
/// size write. Retry only that transient answer, and only a bounded number of
/// times. Any other refusal is returned immediately.
fn retry_busy<F, S>(mut attempt: F, retries: u32, mut backoff: S) -> Result<(), ZramError>
where
    F: FnMut() -> Result<(), ZramError>,
    S: FnMut(),
{
    let mut last = None;
    for index in 0..=retries {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) if is_transient_busy(&error) && index < retries => {
                last = Some(error);
                backoff();
            }
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or(ZramError::RollbackFailed))
}

fn is_transient_busy(error: &ZramError) -> bool {
    match error {
        ZramError::RollbackFailed => true,
        ZramError::Io(error) => error.raw_os_error() == Some(Errno::BUSY.raw_os_error()),
        ZramError::SymlinkRefused | ZramError::NotSysfs | ZramError::InvalidInput(_) => false,
    }
}

fn reset_zram<F>(open: F, zram_id: u32) -> Result<(), ZramError>
where
    F: FnOnce(u32) -> Result<ZramSysfs, ZramError>,
{
    let zram = open(zram_id)?;
    zram.reset()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn zram_configure_retries_busy_and_bounds_attempts() {
        let calls = std::cell::Cell::new(0);
        let result = retry_busy(
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err(ZramError::Io(std::io::Error::from_raw_os_error(
                        Errno::BUSY.raw_os_error(),
                    )))
                } else {
                    Ok(())
                }
            },
            5,
            || {},
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls.get(), 3);

        let exhausted = std::cell::Cell::new(0);
        let result = retry_busy(
            || {
                exhausted.set(exhausted.get() + 1);
                Err(ZramError::Io(std::io::Error::from_raw_os_error(
                    Errno::BUSY.raw_os_error(),
                )))
            },
            3,
            || {},
        );
        assert!(matches!(result, Err(ZramError::Io(_))), "{result:?}");
        assert_eq!(exhausted.get(), 4);

        let other = std::cell::Cell::new(0);
        let result = retry_busy(
            || {
                other.set(other.get() + 1);
                Err(ZramError::NotSysfs)
            },
            3,
            || {},
        );
        assert!(matches!(result, Err(ZramError::NotSysfs)), "{result:?}");
        assert_eq!(other.get(), 1);
    }

    #[test]
    fn a_missing_swap_target_is_an_io_error_before_activation() {
        let path =
            std::env::temp_dir().join(format!("ramshared-native-swapon-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let error = execute_native_swapon(&path, 1 << 20, [0u8; 16], 200).unwrap_err();
        assert!(matches!(error, NativeLifecycleError::Io(_)), "{error:?}");
    }

    #[test]
    fn native_swapon_formats_then_refuses_a_header_size_mismatch() {
        // A one-page file cannot hold the 100-page header this call writes, so
        // the kernel refuses activation even when the test host is root. The
        // helper then scrubs the magic; no live swap is created.
        let path = std::env::temp_dir().join(format!(
            "ramshared-native-swapon-{}.img",
            std::process::id()
        ));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let error = execute_native_swapon(&path, 100 * 4096, [4u8; 16], 200).unwrap_err();
        assert!(
            matches!(
                error,
                NativeLifecycleError::Syscall {
                    op: "swapon(2)",
                    ..
                }
            ),
            "{error:?}"
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[4096 - 10..], &[0u8; 10]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn native_swapoff_refuses_a_target_that_is_not_in_proc_swaps() {
        let error =
            execute_native_swapoff(Path::new("/dev/ramshared-not-a-swap"), None).unwrap_err();
        assert!(
            matches!(error, NativeLifecycleError::Syscall { op: "swapoff(2)", errno } if errno == libc::ENOENT),
            "{error:?}"
        );
    }

    #[test]
    fn native_swap_activate_refuses_an_unformatted_regular_file() {
        let path = std::env::temp_dir().join(format!(
            "ramshared-native-activate-{}.img",
            std::process::id()
        ));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let error = execute_native_swap_activate(&path, 200).unwrap_err();
        assert!(
            matches!(
                error,
                NativeLifecycleError::Syscall {
                    op: "swapon(2)",
                    ..
                }
            ),
            "{error:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zram_configure_and_reset_run_against_a_pinned_temporary_root() {
        let root =
            std::env::temp_dir().join(format!("ramshared-native-zram-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("zram0")).unwrap();
        for name in ["reset", "backing_dev", "disksize"] {
            std::fs::write(root.join("zram0").join(name), b"").unwrap();
        }
        configure_zram(
            |id| ZramSysfs::open_root_checked(&root, id, false),
            0,
            Path::new("/dev/nbd0"),
            1 << 20,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("zram0/disksize")).unwrap(),
            "1048576"
        );
        reset_zram(|id| ZramSysfs::open_root_checked(&root, id, false), 0).unwrap();
        assert_eq!(std::fs::read(root.join("zram0/reset")).unwrap(), b"1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn native_zram_configure_on_a_missing_device_is_total() {
        let error =
            execute_native_zram_configure(u32::MAX, Path::new("/dev/ramshared-missing"), 1 << 20)
                .unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn native_zram_reset_on_a_missing_device_is_total() {
        // A container or host without that device number must return an error,
        // never panic or silently claim the reset.
        let error = execute_native_zram_reset(u32::MAX);
        assert!(error.is_err());
    }

    #[test]
    fn live_root_ephemeral_zram_native_swapon_swapoff_roundtrip() {
        if !rustix::process::geteuid().is_root() {
            return;
        }
        let hot_add = Path::new("/sys/class/zram-control/hot_add");
        let hot_remove = Path::new("/sys/class/zram-control/hot_remove");
        let Ok(raw_id) = std::fs::read_to_string(hot_add) else {
            return;
        };
        let zram_id: u32 = raw_id.trim().parse().unwrap();
        let dev_path = std::path::PathBuf::from(format!("/dev/zram{zram_id}"));
        let zram = ZramSysfs::open(zram_id).unwrap();
        let _ = zram.set_algorithm("lz4");
        zram.set_disksize(16 << 20).unwrap();

        execute_native_swapon(&dev_path, 16 << 20, [0x7A; 16], 250).unwrap();
        let swaps_active = std::fs::read_to_string("/proc/swaps").unwrap();
        assert!(
            swaps_active.contains(&format!("/dev/zram{zram_id}")),
            "expected /dev/zram{zram_id} in /proc/swaps: {swaps_active}"
        );

        execute_native_swapoff(&dev_path, None).unwrap();
        let swaps_after = std::fs::read_to_string("/proc/swaps").unwrap();
        assert!(
            !swaps_after.contains(&format!("/dev/zram{zram_id}")),
            "expected /dev/zram{zram_id} removed from /proc/swaps: {swaps_after}"
        );

        execute_native_zram_reset(zram_id).unwrap();
        std::fs::write(hot_remove, zram_id.to_string().as_bytes()).unwrap();
        assert!(!dev_path.exists());
    }
}
