// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/command_runner.rs
//! Bounded child process capture and native swap/zram command runner abstraction.
//!
//! Extracted from `cascade_io.rs` so command execution and sysfs zram/swap
//! helpers stay decoupled from cascade setup orchestration.

use super::*;

pub(super) fn command_label(command: &str, args: &[&str]) -> String {
    if args.is_empty() {
        command.to_string()
    } else {
        format!("{command} {}", args.join(" "))
    }
}

/// Runs a direct child with a bounded wait and captures its bounded output.
///
/// The child leads a private process group. Timeout and inherited-pipe cleanup
/// target exactly that group and prove the direct child reaped before return.
pub(super) fn run_command_bounded_for(
    command: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, CascadeError> {
    run_command_bounded_for_with_spawn(command, args, timeout, |_| {})
}

pub(super) fn run_command_bounded_for_with_spawn<F>(
    command: &str,
    args: &[&str],
    timeout: Duration,
    on_spawn: F,
) -> Result<String, CascadeError>
where
    F: FnOnce(u32),
{
    let label = command_label(command, args);
    let mut command = Command::new(command);
    command.args(args);
    // Every classified child is spawned here, so this is where the locale the
    // classifiers assume is pinned rather than inherited.
    //
    // `classify_zram_allocation_failure` and the teardown swapoff absence
    // check both read `strerror` and gettext text out of the tool's own
    // diagnostics: `Device or resource busy` decides retry, `Invalid argument`
    // beside `failed to set algorithm` decides advance, `No such file` decides
    // the swap is already gone. glibc and util-linux translate all of those
    // under a non-C `LC_MESSAGES`, and a translated line falls through every
    // matcher to the arm that stops the walk. `LC_ALL` outranks `LC_*` and
    // `LANG` for every category including `LC_MESSAGES`, so this one pin is
    // the whole contract the matchers already depend on.
    command.env("LC_ALL", "C");
    let output = bounded_process::run_capture_command(
        &mut command,
        &label,
        timeout,
        COMMAND_OUTPUT_LIMIT,
        on_spawn,
    )
    .map_err(|error| CascadeError::Shell {
        cmd: label,
        msg: error.to_string(),
    })?;
    // `run_capture_command` returns Ok only for a successful exit status;
    // non-zero and signalled children arrive here as `ProcessSpawnError`,
    // whose `Display` already carries the exit code and stderr.
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(super) fn run_command_bounded(command: &str, args: &[&str]) -> Result<String, CascadeError> {
    run_command_bounded_for(command, args, SHORT_COMMAND_TIMEOUT)
}

pub(super) trait CommandRunner {
    fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError>;

    fn run_bounded(
        &self,
        command: &str,
        args: &[&str],
        _timeout: Duration,
    ) -> Result<String, CascadeError> {
        self.run(command, args)
    }

    /// Native `swapoff(2)`. Test runners inherit the scripted command so every
    /// isolated fixture stays hermetic; the production runner overrides this
    /// with the direct syscall and never spawns an interpreter.
    fn native_swapoff(
        &self,
        path: &Path,
        absorber: Option<&ramshared_block::swap_absorber::SwapAbsorber>,
    ) -> Result<(), CascadeError> {
        let _ = absorber;
        let device = path.display().to_string();
        self.run_bounded("swapoff", &["--", &device], SWAPOFF_TIMEOUT)
            .map(|_| ())
    }

    /// Native zram reset. Same injection contract as [`Self::native_swapoff`].
    fn native_zram_reset(&self, path: &Path) -> Result<(), CascadeError> {
        let device = path.display().to_string();
        self.run("zramctl", &["-r", &device]).map(|_| ())
    }

    /// Allocate one uninitialized zram device.
    fn native_zram_allocate(&self) -> Result<String, CascadeError> {
        self.run("zramctl", &["--find"])
    }

    /// Bind the backing device before the size is set.
    fn native_zram_bind_backing(&self, zdev: &str, backing_dev: &str) -> Result<(), CascadeError> {
        let name = zdev.rsplit('/').next().unwrap_or(zdev);
        let attr = format!("/sys/block/{name}/backing_dev");
        self.run("sh", &["-c", &format!("printf %s {backing_dev} > {attr}")])
            .map(|_| ())
    }

    /// Request the compressor before the size is set.
    fn native_zram_set_algorithm(&self, zdev: &str, algorithm: &str) -> Result<(), CascadeError> {
        let name = zdev.rsplit('/').next().unwrap_or(zdev);
        self.run(
            "bash",
            &[
                "-c",
                &format!("echo {algorithm} > /sys/block/{name}/comp_algorithm"),
            ],
        )
        .map(|_| ())
    }

    /// Best-effort secondary recompression setup (`CONFIG_ZRAM_MULTI_COMP`).
    fn native_zram_configure_recomp(&self, _zdev: &str, _primary_algo: &str) {}

    /// Initialize the device with its size.
    fn native_zram_set_size(
        &self,
        zdev: &str,
        _backing_dev: &str,
        mb: u64,
    ) -> Result<(), CascadeError> {
        let name = zdev.rsplit('/').next().unwrap_or(zdev);
        let bytes = mb.saturating_mul(1024 * 1024);
        self.run(
            "bash",
            &["-c", &format!("echo {bytes} > /sys/block/{name}/disksize")],
        )
        .map(|_| ())
    }

    /// Activate an already formatted swap device.
    fn native_swap_activate(&self, path: &Path, priority: &str) -> Result<(), CascadeError> {
        let device = path.display().to_string();
        self.run("swapon", &["-p", priority, &device]).map(|_| ())
    }

    /// Format a fresh device and activate it.
    fn native_mkswap_and_swapon(
        &self,
        path: &Path,
        _total_bytes: u64,
        priority: &str,
    ) -> Result<(), CascadeError> {
        let device = path.display().to_string();
        self.run("mkswap", &[&device])?;
        self.run("swapon", &["-p", priority, &device])
            .map(|_| ())
            .map_err(|error| CascadeError::Shell {
                cmd: "swapon".into(),
                msg: format!("swapon outcome uncertain: {error}"),
            })
    }

    /// Arm finite NBD dead-connection timeout via genetlink in real attaches.
    fn post_nbd_attach(&self, _swap_dev: &str) {}
}

pub(super) struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
        run_command_bounded(command, args)
    }

    fn run_bounded(
        &self,
        command: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<String, CascadeError> {
        run_command_bounded_for(command, args, timeout)
    }

    fn native_swapoff(
        &self,
        path: &Path,
        absorber: Option<&ramshared_block::swap_absorber::SwapAbsorber>,
    ) -> Result<(), CascadeError> {
        crate::runtime::native_ops::execute_native_swapoff(path, absorber).map_err(|error| {
            CascadeError::Shell {
                cmd: "swapoff".into(),
                msg: error.to_string(),
            }
        })
    }

    fn native_zram_reset(&self, path: &Path) -> Result<(), CascadeError> {
        let zram_id = zram_id_from_path(path).ok_or_else(|| CascadeError::Shell {
            cmd: "zramctl".into(),
            msg: format!("{} is not a zram device", path.display()),
        })?;
        crate::runtime::native_ops::execute_native_zram_reset(zram_id).map_err(|error| {
            CascadeError::Shell {
                cmd: "zramctl".into(),
                msg: error.to_string(),
            }
        })
    }

    fn native_zram_allocate(&self) -> Result<String, CascadeError> {
        // Primary: the kernel control node allocates without a helper. A
        // container without the node keeps the legacy allocator as a bounded
        // fallback, never the other way round.
        zram_hot_add_from(Path::new("/sys/class/zram-control/hot_add"), &|| {
            let path = self.run("zramctl", &["--find"])?;
            let id = zram_id_from_path(Path::new(path.trim())).ok_or_else(|| {
                CascadeError::UnsafeContainment(
                    "fallback zram allocator returned an unverified device name".into(),
                )
            })?;
            let zram = crate::runtime::zram_sysfs::ZramSysfs::open(id)
                .map_err(|error| unproven_zram_preparation(&path, error))?;
            zram.verify_unconfigured()
                .map_err(|error| unproven_zram_preparation(&path, error))?;
            Ok(path)
        })
    }

    fn post_nbd_attach(&self, swap_dev: &str) {
        let secs = super::nbd_netlink::DEFAULT_DEAD_CONN_TIMEOUT_SECS;
        match super::nbd_netlink::arm_dead_conn_timeout(swap_dev, secs) {
            Ok(()) => eprintln!(
                "[up] armed NBD dead_conn_timeout={secs}s on {swap_dev} for bounded peer replacement"
            ),
            Err(error) => eprintln!(
                "[warn] NBD dead_conn_timeout netlink config unavailable on {swap_dev}: {error}"
            ),
        }
    }

    fn native_zram_bind_backing(&self, zdev: &str, backing_dev: &str) -> Result<(), CascadeError> {
        let id = zram_id_from_path(Path::new(zdev)).ok_or_else(|| CascadeError::Shell {
            cmd: "zram sysfs".into(),
            msg: format!("{zdev} is not a zram device"),
        })?;
        let zram = crate::runtime::zram_sysfs::ZramSysfs::open(id).map_err(|error| {
            CascadeError::Shell {
                cmd: "zram sysfs".into(),
                msg: error.to_string(),
            }
        })?;
        zram.verify_unconfigured()
            .map_err(|error| unproven_zram_preparation(zdev, error))?;
        zram.set_backing_dev(Path::new(backing_dev))
            .map_err(|error| CascadeError::Shell {
                cmd: "zram sysfs".into(),
                msg: error.to_string(),
            })
    }

    fn native_zram_set_algorithm(&self, zdev: &str, algorithm: &str) -> Result<(), CascadeError> {
        let id = zram_id_from_path(Path::new(zdev)).ok_or_else(|| CascadeError::Shell {
            cmd: "zram sysfs".into(),
            msg: format!("{zdev} is not a zram device"),
        })?;
        let zram = crate::runtime::zram_sysfs::ZramSysfs::open(id).map_err(|error| {
            CascadeError::Shell {
                cmd: "zram sysfs".into(),
                msg: error.to_string(),
            }
        })?;
        zram.set_algorithm(algorithm)
            .map_err(|error| CascadeError::Shell {
                cmd: "zram sysfs".into(),
                msg: error.to_string(),
            })
    }

    fn native_zram_configure_recomp(&self, zdev: &str, primary_algo: &str) {
        if primary_algo == "zstd" {
            return;
        }
        let Some(id) = zram_id_from_path(Path::new(zdev)) else {
            return;
        };
        let Ok(zram) = crate::runtime::zram_sysfs::ZramSysfs::open(id) else {
            return;
        };
        if zram.supports_recomp() && zram.set_recomp_algorithm("zstd", 1).is_ok() {
            eprintln!("[up] zram secondary recompression armed: zstd (priority=1) on {zdev}");
        }
    }

    fn native_zram_set_size(
        &self,
        zdev: &str,
        backing_dev: &str,
        mb: u64,
    ) -> Result<(), CascadeError> {
        let id = zram_id_from_path(Path::new(zdev)).ok_or_else(|| CascadeError::Shell {
            cmd: "zram sysfs".into(),
            msg: format!("{zdev} is not a zram device"),
        })?;
        let bytes = mb.saturating_mul(1024 * 1024);
        crate::runtime::native_ops::execute_native_zram_configure(id, Path::new(backing_dev), bytes)
            .map_err(|error| match error {
                crate::runtime::zram_sysfs::ZramError::PreparationUnproven(_) => {
                    unproven_zram_preparation(zdev, error)
                }
                error => CascadeError::Shell {
                    cmd: "zram sysfs".into(),
                    msg: error.to_string(),
                },
            })
    }

    fn native_swap_activate(&self, path: &Path, priority: &str) -> Result<(), CascadeError> {
        let priority = priority
            .parse::<u16>()
            .map_err(|error| CascadeError::Shell {
                cmd: "swapon".into(),
                msg: format!("invalid swap priority ({error})"),
            })?;
        crate::runtime::native_ops::execute_native_swap_activate(path, priority).map_err(|error| {
            CascadeError::Shell {
                cmd: "swapon".into(),
                msg: error.to_string(),
            }
        })
    }

    fn native_mkswap_and_swapon(
        &self,
        path: &Path,
        total_bytes: u64,
        priority: &str,
    ) -> Result<(), CascadeError> {
        let priority = priority
            .parse::<u16>()
            .map_err(|error| CascadeError::Shell {
                cmd: "swapon".into(),
                msg: format!("invalid swap priority ({error})"),
            })?;
        crate::runtime::native_ops::execute_native_swapon(
            path,
            total_bytes,
            fresh_swap_uuid(),
            priority,
        )
        .map_err(|error| CascadeError::Shell {
            cmd: "mkswap+swapon".into(),
            msg: format!("swapon outcome uncertain: {error}"),
        })
    }
}

fn unproven_zram_preparation(path: &str, error: impl std::fmt::Display) -> CascadeError {
    CascadeError::UnsafeContainment(format!(
        "cannot prove zram preparation for {path} ({error}); device and backend preserved"
    ))
}

pub(super) fn zram_hot_add_from(
    path: &Path,
    fallback: &dyn Fn() -> Result<String, CascadeError>,
) -> Result<String, CascadeError> {
    match std::fs::read_to_string(path) {
        Ok(value) => value
            .trim()
            .parse::<u32>()
            .map(|id| format!("/dev/zram{id}"))
            .map_err(|error| CascadeError::Shell {
                cmd: "zram hot_add".into(),
                msg: format!("malformed hot_add node ({error})"),
            }),
        Err(_) => fallback(),
    }
}

pub(super) fn zram_id_from_path(path: &Path) -> Option<u32> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("zram"))
        .and_then(|id| id.parse::<u32>().ok())
}

/// A fresh swap-header UUID. It is identity metadata, not a secret; the OS
/// CSPRNG is used when available and a time/pid value keeps the header unique
/// on hosts where `getrandom(2)` is unavailable.
pub(super) fn fresh_swap_uuid() -> [u8; 16] {
    let mut uuid = [0u8; 16];
    if let Ok(nonce) = ramshared_ipc::random_nonce() {
        uuid.copy_from_slice(&nonce[..16]);
        return uuid;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    uuid[..8].copy_from_slice(&(nanos as u64).to_le_bytes());
    uuid[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    uuid[12..].copy_from_slice(&(nanos >> 64).to_le_bytes()[..4]);
    uuid
}
