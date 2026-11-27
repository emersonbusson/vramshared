// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/zram_setup.rs
//! ZRAM tier setup: allocate the device, choose and verify its compression
//! algorithm, and roll the tier back when an attempt fails.
//!
//! Extracted from `cascade_io.rs` as one band of the boundary plan in
//! `workspace module layout`. This band owns the ZRAM
//! allocation lifecycle and its bounded retry: a failure is classified, and
//! a retry happens only for a signature the kernel actually recovers from.
//! It does not attach NBD, drive the daemon or touch the swap table.

#[allow(unused_imports)]
use super::*;

pub(super) fn parse_zram_device(output: &str) -> Option<String> {
    let device = output.trim();
    let suffix = device.strip_prefix("/dev/zram")?;
    (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| device.to_string())
}

/// The kernel's `comp_algorithm` line lists every compressor the device
/// offers and brackets the active one: `[lzo-rle] lzo lz4 zstd deflate`.
/// Return that bracketed name, or `None` when the line is malformed or names
/// no active compressor. One spelling of the kernel's answer, used by the
/// setup verification, by the compression sample, and by the tests that prove
/// it.
///
/// A token is a bracketed name only when it is exactly `[name]`: one bracket
/// pair, a non-empty interior, and no bracket inside it. `[a][b]` and
/// `[[zstd]]` are not names, and a line that brackets two tokens does not name
/// one active compressor. Anything else is malformed and refuses, which is
/// the documented contract — a junk name would otherwise reach the setup
/// verification and the published sample.
pub fn parse_active_zram_algorithm(text: &str) -> Option<&str> {
    let mut active: Option<&str> = None;
    for token in text.split_whitespace() {
        let Some(name) = token
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .filter(|name| !name.is_empty() && !name.contains(['[', ']']))
        else {
            continue;
        };
        if active.is_some() {
            return None;
        }
        active = Some(name);
    }
    active
}

/// Read the kernel's own record of which compressor `device` is using.
/// `device` is the exact block name (`zram7`), never a path — the sysfs
/// directory is derived from the caller's root so a test never touches a
/// host device.
fn read_zram_active_algorithm(sys_block: &Path, device: &str) -> Result<String, CascadeError> {
    let path = sys_block.join(device).join("comp_algorithm");
    let text = fs::read_to_string(&path).map_err(|error| {
        CascadeError::Precondition(format!(
            "cannot read zram comp_algorithm for {device} at {}: {error}",
            path.display()
        ))
    })?;
    parse_active_zram_algorithm(&text)
        .map(str::to_string)
        .ok_or_else(|| {
            CascadeError::Precondition(format!(
                "zram comp_algorithm for {device} names no active compressor ({text:?})"
            ))
        })
}

/// Fail closed unless the kernel is actually using the compressor this walk
/// requested. `zramctl --algorithm` is a request; only `comp_algorithm` is
/// the kernel's answer. Day-0 forbids activating a device on a compressor
/// the operator was not asked for — the success line would otherwise print a
/// name that was never measured.
fn verify_zram_active_algorithm(
    sys_block: &Path,
    device: &str,
    requested: &str,
) -> Result<String, CascadeError> {
    let active = read_zram_active_algorithm(sys_block, device)?;
    if active != requested {
        return Err(CascadeError::Precondition(format!(
            "zram {device} is compressing with {active}, not the requested {requested}; \
             refusing to activate a device whose compressor the kernel did not keep"
        )));
    }
    Ok(active)
}

pub(super) fn rollback_zram_tier<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    zram_device: &str,
    active: bool,
) -> bool {
    if zram_device.is_empty() {
        return true;
    }
    let identity = read_bound_device_record(&paths.zram_dev_file);
    let Ok(identity) = identity else {
        return false;
    };
    if identity.path != canonicalize_swap_path(zram_device) {
        return false;
    }
    if prove_only_exact_provisional_device_live(&identity).is_err()
        || revalidate_bound_device(&identity).is_err()
    {
        return false;
    }
    let pinned = match bind_device_for_effect(&identity) {
        Ok(pinned) => pinned,
        Err(_) => return false,
    };
    if active {
        let Ok(swaps) = read_swaps() else {
            return false;
        };
        if swaps
            .iter()
            .any(|entry| entry.canonical_path() == identity.path)
            && runner
                .native_swapoff(Path::new(pinned.path()), None)
                .is_err()
        {
            return false;
        }
    }
    if prove_exact_swap_absent(&identity).is_err() || pinned.revalidate_named_identity().is_err() {
        return false;
    }
    // zramctl derives its sysfs target from the canonical device name and the
    // kernel zram driver refuses reset with EBUSY while any opener holds the fd.
    drop(pinned);
    if runner.native_zram_reset(Path::new(zram_device)).is_err() {
        return false;
    }
    let Ok(live_devices) = detect_live_managed_devices() else {
        return false;
    };
    if live_devices
        .iter()
        .any(|observed| observed.path == identity.path)
    {
        return false;
    }
    remove_runtime_file(&paths.zram_dev_file);
    true
}

pub(super) fn error_after_zram_rollback<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    zram_device: &str,
    may_be_active: bool,
    primary: CascadeError,
) -> CascadeError {
    if rollback_zram_tier(runner, paths, zram_device, may_be_active) {
        primary
    } else {
        CascadeError::UnsafeContainment(format!(
            "zram setup failed and exact rollback could not be proven ({primary}); device and ownership evidence were preserved for attended recovery"
        ))
    }
}

fn reconcile_malformed_zram_allocation<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    before: &[BoundDeviceIdentity],
    malformed_output: &str,
) -> Result<bool, CascadeError> {
    let after = detect_live_managed_devices().map_err(|error| {
        CascadeError::UnsafeContainment(format!(
            "zramctl returned malformed success output and post-allocation state is unreadable ({error}); no reset was attempted"
        ))
    })?;
    if sorted_device_set(&after) == sorted_device_set(before) {
        return Ok(false);
    }
    let Some(device) = exact_single_device_delta(before, &after, ManagedDeviceKind::Zram) else {
        return Err(CascadeError::UnsafeContainment(format!(
            "zramctl returned malformed success output ({malformed_output:?}) and the allocation delta is ambiguous; no device was reset"
        )));
    };
    if let Err(error) = write_bound_device_record(&paths.zram_dev_file, &device) {
        return Err(CascadeError::UnsafeContainment(format!(
            "new exact zram device {} was reconciled after malformed output but its ownership record could not be sealed ({error}); device preserved",
            device.path
        )));
    }
    let pinned = bind_device_for_effect(&device).map_err(|error| {
        CascadeError::UnsafeContainment(format!(
            "new exact zram device {} could not be fd/dev_t-bound before reset ({error}); ownership evidence preserved",
            device.path
        ))
    })?;
    prove_exact_swap_absent(&device).map_err(|error| {
        CascadeError::UnsafeContainment(format!(
            "new exact zram device {} is not proven inactive ({error}); ownership evidence preserved",
            device.path
        ))
    })?;
    // zramctl derives the sysfs target from the canonical device name and the
    // kernel zram driver refuses reset with EBUSY while any opener holds the fd.
    // Revalidate the exact descriptor first, then close before invoking reset.
    pinned.revalidate_named_identity().map_err(|error| {
        CascadeError::UnsafeContainment(format!(
            "new exact zram device {} changed identity before reset ({error}); ownership evidence preserved",
            device.path
        ))
    })?;
    drop(pinned);
    runner
        .native_zram_reset(Path::new(&device.path))
        .map_err(|error| {
            CascadeError::UnsafeContainment(format!(
                "reset of reconciled zram device {} is unproven ({error}); ownership evidence preserved",
                device.path
            ))
        })?;
    let final_devices = detect_live_managed_devices().map_err(|error| {
        CascadeError::UnsafeContainment(format!(
            "reset completion for reconciled zram device {} is unreadable ({error}); ownership evidence preserved",
            device.path
        ))
    })?;
    if sorted_device_set(&final_devices) != sorted_device_set(before) {
        return Err(CascadeError::UnsafeContainment(format!(
            "managed-device cardinality did not return to the exact pre-allocation snapshot after resetting {}; ownership evidence preserved",
            device.path
        )));
    }
    remove_runtime_file(&paths.zram_dev_file);
    Ok(true)
}

/// How many times one compressor may attempt `zramctl --find` before the walk
/// gives up. A device that is still in use at the end of this budget is
/// reported as such; the compressor is never changed to work around it.
pub(super) const ZRAM_ALLOCATION_ATTEMPTS: u32 = 3;
/// Pause between attempts of the same compressor while the allocator reports
/// the device as still in use.
const ZRAM_ALLOCATION_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Why one `zramctl --find --algorithm <algo>` attempt failed, and whether the
/// walk may move on to another compressor.
///
/// Only "this kernel does not offer that compressor" is evidence about the
/// compressor, and it is the sole reason to advance. A device that is still
/// busy, or a pool with no free device, says nothing about the compressor:
/// treating either as "try the next algorithm" silently replaces the requested
/// compressor with a lesser one and gives the operator no signal that anything
/// changed. invariant checkretries a classified transient of the *same*
/// operation; Day-0 forbids the silent degraded path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ZramAllocationFailure {
    /// The kernel refused this compressor name. Advance to the next one.
    AlgorithmUnsupported,
    /// The allocator could not take a device right now. Retry this same
    /// compressor under the attempt budget.
    DeviceTransient,
    /// Everything else — capacity, protocol, spawn, timeout. Fail closed with
    /// the primary error rather than burning the rest of the list.
    Fatal,
}

/// Classify a failed `zramctl --find` attempt from the tool's own diagnostics.
///
/// util-linux reports both "this compressor is unknown" and "this device is
/// still in use" through `failed to set algorithm`, distinguished by
/// `strerror`; `failed to reset` is the reuse path and is busy-or-worse.
/// Device-in-use is checked first because it is a device state and not a
/// compressor answer. One spelling of the decision, used by the walk and by
/// the tests that prove the walk.
///
/// The matchers read English `strerror` and gettext text, which is only a
/// stable contract because the spawn helper pins `LC_ALL=C` on every child.
/// Without that pin a translated line matches nothing and lands in the
/// fall-through, which stops the whole `up` instead of advancing or retrying.
pub(super) fn classify_zram_allocation_failure(error: &CascadeError) -> ZramAllocationFailure {
    let CascadeError::Shell { msg, .. } = error else {
        return ZramAllocationFailure::Fatal;
    };
    if zram_device_is_busy(msg) {
        return ZramAllocationFailure::DeviceTransient;
    }
    if zram_algorithm_is_unknown(msg) {
        return ZramAllocationFailure::AlgorithmUnsupported;
    }
    ZramAllocationFailure::Fatal
}

/// util-linux `strerror(errno)` spellings for a zram target that is not
/// available yet (EBUSY), whether the tool was resetting a reusable device or
/// writing `comp_algorithm` onto one that is already initialized.
///
/// `EBUSY` is a belt-and-braces token, not a locale safety net: `strerror`
/// prints the English or translated text, never the symbolic name. The
/// reliable spelling is the `strerror` one, guaranteed by the spawn helper's
/// `LC_ALL=C`.
pub(super) fn zram_device_is_busy(msg: &str) -> bool {
    msg.contains("Device or resource busy")
        || msg.contains("EBUSY")
        || msg.contains("no free zram device found")
}

/// The kernel refused this compressor name (`comp_algorithm` rejected it).
/// Any other errno on the same write is a device or capacity answer and is not
/// a licence to change compressor.
///
/// `EINVAL` is a belt-and-braces token, not a locale safety net — see
/// [`zram_device_is_busy`].
pub(super) fn zram_algorithm_is_unknown(msg: &str) -> bool {
    msg.contains("failed to set algorithm")
        && (msg.contains("Invalid argument") || msg.contains("EINVAL"))
}

/// The one refusal every zram walk failure carries. The product never falls
/// back to writing an unbound `zram0` sysfs node: that would mutate a device
/// this run does not own. One spelling, used by every exit and by the tests
/// that prove the exit.
pub(super) fn zram_allocator_unavailable(primary: &str) -> CascadeError {
    CascadeError::Precondition(format!(
        "zram is unavailable through ownership-creating zramctl --find ({primary}); \
         the unbound zram0 sysfs fallback is disabled to avoid mutating a foreign device. \
         Try --zram 0 or repair zramctl/modprobe."
    ))
}

/// Bind a backing device to a zram node **before** it is sized.
///
/// The kernel refuses the reverse order with
/// `zram: Can't setup backing device for initialized device`, and a zram
/// node with no backing device is a tier whose only copy is the RAM it
/// compresses into — the orphan-page condition this whole contract exists
/// to make impossible. Failing here refuses the tier rather than creating
/// one that can lose a page.
///
/// SPEC `tier-ownership-orphan-page-elimination` DT-2, ITEM-1.
pub(super) fn bind_zram_backing_device(
    runner: &impl CommandRunner,
    zdev: &str,
    backing_dev: &str,
) -> Result<(), CascadeError> {
    if backing_dev.is_empty() {
        return Err(CascadeError::Precondition(
            "a zram tier without a backing device would hold the only copy of its pages".into(),
        ));
    }
    // `zramctl` has no `--backing-dev`; the kernel's sysfs is the only
    // interface. The write happens before any size is set, which is the
    // order the kernel requires.
    // The sysfs node is named by the bare device name. The caller may pass
    // either `/dev/zram0` or `zram0`; the kernel path takes the latter.
    runner
        .native_zram_bind_backing(zdev, backing_dev)
        .map_err(|error| match error {
            error @ CascadeError::UnsafeContainment(_) => error,
            error => CascadeError::Precondition(format!(
                "cannot bind backing device {backing_dev} to {zdev} before sizing it: {error}"
            )),
        })?;
    Ok(())
}

/// The kernel takes the compressor only before the size is set, so the
/// setup writes `comp_algorithm` on the uninitialized device and classifies
/// the refusal here. `bash` reports the errno wording (`Invalid argument`)
/// that the capability classifier reads; `sh` collapses it to `I/O error`,
/// which would read every refusal as a device failure.
fn set_zram_algorithm(
    runner: &impl CommandRunner,
    zdev: &str,
    algo: &str,
) -> Result<(), CascadeError> {
    runner
        .native_zram_set_algorithm(zdev, algo)
        .map_err(|error| match error {
            CascadeError::Shell { cmd, msg } => CascadeError::Shell {
                cmd,
                msg: format!("failed to set algorithm: {msg}"),
            },
            other => other,
        })?;
    Ok(())
}

/// The size initializes the device: the kernel allocates the compression
/// streams here and refuses the write if the bound backing device cannot
/// hold the tier. `bash` for the errno wording, same reason as the
/// compressor write.
fn set_zram_size(
    runner: &impl CommandRunner,
    zdev: &str,
    backing_dev: &str,
    mb: u64,
) -> Result<(), CascadeError> {
    runner
        .native_zram_set_size(zdev, backing_dev, mb)
        .map_err(|error| match error {
            error @ CascadeError::UnsafeContainment(_) => error,
            error => CascadeError::Precondition(format!(
                "cannot set the zram tier size on {zdev}: {error}; a bound backing device \
                 must be at least as large as the tier"
            )),
        })?;
    Ok(())
}

/// The recorded Tier 1 device when it is live in the swap table, with its
/// current backing binding read from sysfs (`None` when the attribute will
/// not read). A record whose device is not live is stale: there is no tier
/// to no-op over, and the caller proceeds to allocate.
fn live_recorded_zram_tier(paths: &RuntimePaths) -> Option<(String, Option<String>)> {
    let record = read_bound_device_record(&paths.zram_dev_file).ok()?;
    if record.kind != ManagedDeviceKind::Zram {
        return None;
    }
    let name = record.path.rsplit('/').next()?.to_string();
    let swaps = super::read_swaps().ok()?;
    let live = swaps.iter().any(|entry| {
        !entry.is_ghost()
            && Path::new(&entry.filename)
                .file_name()
                .and_then(|n| n.to_str())
                == Some(name.as_str())
    });
    if !live {
        return None;
    }
    let backing = fs::read_to_string(paths.sys_block.join(&name).join("backing_dev"))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|value| !value.is_empty());
    Some((record.path, backing))
}

/// Reset a device this attempt just allocated and prove the managed-device
/// set returned to its pre-allocation snapshot, then hand back the primary
/// error. A device that cannot be proven gone keeps the containment refusal
/// instead: a half-allocated zram device is evidence, not a tier.
fn abandon_allocated_zram_device<R: CommandRunner>(
    runner: &R,
    zdev: &str,
    before: &[BoundDeviceIdentity],
    primary: CascadeError,
) -> CascadeError {
    if matches!(primary, CascadeError::UnsafeContainment(_)) {
        return primary;
    }
    if let Err(error) = runner.native_zram_reset(Path::new(zdev)) {
        return CascadeError::UnsafeContainment(format!(
            "zram setup refused after allocating {zdev} and its reset is unproven ({error}); \
             device preserved for attended recovery ({primary})"
        ));
    }
    match detect_live_managed_devices() {
        Ok(after) if sorted_device_set(&after) == sorted_device_set(before) => primary,
        Ok(_) => CascadeError::UnsafeContainment(format!(
            "managed-device cardinality did not return to the pre-allocation snapshot after \
             resetting {zdev}; ownership evidence preserved ({primary})"
        )),
        Err(error) => CascadeError::UnsafeContainment(format!(
            "reset of {zdev} completed but the managed-device set is unreadable ({error}); \
             ownership evidence preserved ({primary})"
        )),
    }
}

pub(super) fn setup_zram_with<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    mb: u64,
    prio: i32,
    backing_dev: Option<&str>,
) -> Result<String, CascadeError> {
    setup_zram_with_custody(runner, paths, mb, prio, backing_dev, None)
}

pub(super) fn setup_zram_with_custody<R: CommandRunner>(
    runner: &R,
    paths: &RuntimePaths,
    mb: u64,
    prio: i32,
    backing_dev: Option<&str>,
    writeback_binding: Option<LifecycleBinding>,
) -> Result<String, CascadeError> {
    if mb == 0 {
        eprintln!("[up] zram skipped (--zram 0)");
        return Ok(String::new());
    }
    let _ = runner.run("modprobe", &["zram"]);
    // re-arming an already-correct tier is a no-op. A managed
    // zram device recorded by a previous arm and live in the swap table IS
    // the tier; allocating again would leave two devices and two bindings
    // for one tier. A live tier whose binding does not match the request is
    // refused rather than doubled — reconfiguration is `ramshared down`
    // followed by `up`, never a second allocation.
    if let Some((existing, current)) = live_recorded_zram_tier(paths) {
        if writeback_binding.is_some() {
            return Err(CascadeError::UnsafeContainment(format!(
                "existing zram {existing} appeared during writeback startup; backend preserved"
            )));
        }
        let requested = backing_dev.unwrap_or("none");
        match current {
            Some(bound) if bound == requested => {
                eprintln!(
                    "[up] zram tier already correct at {existing} (backing {bound}) — no action (idempotent)"
                );
                return Ok(existing);
            }
            Some(bound) => {
                return Err(CascadeError::Precondition(format!(
                    "a managed zram tier is already live at {existing} with backing {bound}, \
                     and this run asks for {requested}; allocating again would leave two devices \
                     for one tier. Run `ramshared down` first to reconfigure."
                )));
            }
            None => {
                return Err(CascadeError::Precondition(format!(
                    "a managed zram tier is already live at {existing} but its backing binding \
                     cannot be read; refusing to allocate a second device for one tier. \
                     Run `ramshared down` first."
                )));
            }
        }
    }
    // Compressor preference order. Advancing to the next name is licensed only
    // by `ZramAllocationFailure::AlgorithmUnsupported`.
    let mut refused_compressors: Vec<&str> = Vec::new();
    'compressor: for algo in ZRAM_ALGOS {
        let mut attempt = 1u32;
        let (zdev, before) = loop {
            let before = detect_live_managed_devices_before_effect().map_err(|error| {
                CascadeError::Precondition(format!(
                    "cannot enumerate managed devices before zram allocation: {error}"
                ))
            })?;
            // Allocate the device **uninitialized**. `--size` is what
            // initializes a zram device, and the kernel takes `backing_dev`
            // and `comp_algorithm` only before that: a size set at allocation
            // makes the kernel refuse the backing write with `Can't setup
            // backing device for initialized device`. `zramctl --find --size`
            // was exactly that reversed order; the size and the compressor
            // are applied below, in the kernel's order (DT-2).
            match runner.native_zram_allocate() {
                Ok(output) => {
                    let Some(zdev) = parse_zram_device(&output) else {
                        if reconcile_malformed_zram_allocation(runner, paths, &before, &output)? {
                            eprintln!(
                                "[up] reset the exact zram allocation reconciled from malformed allocator output"
                            );
                            return Err(CascadeError::Precondition(
                                "zram allocator protocol was malformed; the exact newly allocated device was reset, and setup stopped before another allocation"
                                    .into(),
                            ));
                        }
                        return Err(zram_allocator_unavailable(&format!(
                            "zramctl returned an unexpected device ({output:?}) and no new zram device was allocated"
                        )));
                    };
                    // DT-2: the backing device is bound before the size is
                    // set, and a failure here refuses the whole tier rather
                    // than leaving one that can lose its pages.
                    if let Some(backing) = backing_dev
                        && let Err(primary) = bind_zram_backing_device(runner, &zdev, backing)
                    {
                        return Err(abandon_allocated_zram_device(
                            runner, &zdev, &before, primary,
                        ));
                    }
                    // The compressor is the kernel's answer and it is taken
                    // before the size. Only the EINVAL spelling is a
                    // capability answer; any other refusal is a device-state
                    // failure and does not license the next name.
                    if let Err(primary) = set_zram_algorithm(runner, &zdev, algo) {
                        match classify_zram_allocation_failure(&primary) {
                            ZramAllocationFailure::AlgorithmUnsupported => {
                                let primary =
                                    abandon_allocated_zram_device(runner, &zdev, &before, primary);
                                if matches!(primary, CascadeError::UnsafeContainment(_)) {
                                    return Err(primary);
                                }
                                eprintln!(
                                    "[up] kernel does not offer zram compressor {algo}; \
                                     trying the next one: {primary}"
                                );
                                refused_compressors.push(algo);
                                continue 'compressor;
                            }
                            _ => {
                                return Err(abandon_allocated_zram_device(
                                    runner, &zdev, &before, primary,
                                ));
                            }
                        }
                    }
                    runner.native_zram_configure_recomp(&zdev, algo);
                    // The size initializes the device, set after backing and compressor.
                    if let Err(primary) =
                        set_zram_size(runner, &zdev, backing_dev.unwrap_or(""), mb)
                    {
                        return Err(abandon_allocated_zram_device(
                            runner, &zdev, &before, primary,
                        ));
                    }
                    break (zdev, before);
                }
                Err(error @ CascadeError::UnsafeContainment(_)) => return Err(error),
                Err(error) => {
                    let primary = error.to_string();
                    match classify_zram_allocation_failure(&error) {
                        ZramAllocationFailure::DeviceTransient
                            if attempt < ZRAM_ALLOCATION_ATTEMPTS =>
                        {
                            eprintln!(
                                "[up] zram allocator unavailable for {algo} \
                                 (attempt {attempt} of {ZRAM_ALLOCATION_ATTEMPTS}); \
                                 retrying the same compressor: {primary}"
                            );
                            attempt += 1;
                            sleep(ZRAM_ALLOCATION_RETRY_DELAY);
                        }
                        ZramAllocationFailure::DeviceTransient => {
                            return Err(zram_allocator_unavailable(&format!(
                                "zram device was still unavailable after \
                                 {ZRAM_ALLOCATION_ATTEMPTS} attempts with compressor {algo} \
                                 ({primary}); the compressor was not changed to work around \
                                 a device-state failure"
                            )));
                        }
                        ZramAllocationFailure::AlgorithmUnsupported => {
                            eprintln!(
                                "[up] kernel does not offer zram compressor {algo}; \
                                 trying the next one: {primary}"
                            );
                            refused_compressors.push(algo);
                            continue 'compressor;
                        }
                        ZramAllocationFailure::Fatal => {
                            return Err(zram_allocator_unavailable(&primary));
                        }
                    }
                }
            }
        };
        let priority = prio.to_string();
        let after = detect_live_managed_devices().map_err(|error| {
            CascadeError::UnsafeContainment(format!(
                "zram allocator returned {zdev} but post-allocation state is unreadable ({error}); no formatting or activation was attempted"
            ))
        })?;
        let Some(identity) = exact_single_device_delta(&before, &after, ManagedDeviceKind::Zram)
        else {
            return Err(CascadeError::UnsafeContainment(format!(
                "zram allocator returned {zdev} but exact allocation cardinality could not be proven; no formatting or activation was attempted"
            )));
        };
        if identity.path != canonicalize_swap_path(&zdev) {
            return Err(CascadeError::UnsafeContainment(format!(
                "zram allocator output {zdev} differs from the exact newly allocated device {}; no formatting or activation was attempted",
                identity.path
            )));
        }
        if let Err(error) = write_bound_device_record(&paths.zram_dev_file, &identity) {
            return Err(CascadeError::UnsafeContainment(format!(
                "zram identity could not be recorded before formatting ({error}); {zdev} was preserved without mkswap, swapon, or reset"
            )));
        }
        let paired = writeback_binding.is_some();
        if let Some(binding) = writeback_binding {
            super::writeback_startup::seal_zram(paths, binding, &identity)?;
        }
        // A failed activation can still have changed the kernel swap table.
        let mut may_be_active = false;
        let armed = (|| {
            let name = identity.path.rsplit('/').next().unwrap_or_default();
            let verified = verify_zram_active_algorithm(&paths.sys_block, name, algo)?;
            let pinned = bind_device_for_effect(&identity)?;
            pinned.revalidate_named_identity()?;
            may_be_active = true;
            runner.native_mkswap_and_swapon(
                Path::new(pinned.path()),
                mb.saturating_mul(1024 * 1024),
                &priority,
            )?;
            pinned.revalidate_named_identity().map_err(|error| {
                CascadeError::UnsafeContainment(format!(
                    "zram identity changed after swapon ({error}); ownership evidence preserved"
                ))
            })?;
            Ok(verified)
        })();
        let verified_algo = match armed {
            Ok(verified) => verified,
            Err(error) if paired || matches!(error, CascadeError::UnsafeContainment(_)) => {
                return Err(error);
            }
            Err(error) => {
                return Err(error_after_zram_rollback(
                    runner,
                    paths,
                    &zdev,
                    may_be_active,
                    error,
                ));
            }
        };
        eprintln!("[up] zram {zdev} algo={verified_algo} prio={prio}");
        return Ok(zdev);
    }
    Err(zram_allocator_unavailable(&format!(
        "the kernel offered none of the compressors in preference order ({refused_compressors:?})"
    )))
}

#[cfg(test)]
pub(crate) fn setup_zram(mb: u64, prio: i32) -> Result<String, CascadeError> {
    setup_zram_with(
        &SystemCommandRunner,
        &RuntimePaths::system(),
        mb,
        prio,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allocator's success line is only a device when it names exactly
    /// one zram node. Anything else is a protocol the caller cannot trust.
    #[test]
    fn zram_output_requires_exact_block_identity() {
        assert_eq!(parse_zram_device("/dev/zram0\n"), Some("/dev/zram0".into()));
        assert_eq!(
            parse_zram_device(" /dev/zram12 "),
            Some("/dev/zram12".into())
        );
        assert_eq!(parse_zram_device("/dev/zram"), None);
        assert_eq!(parse_zram_device("/dev/nbd0"), None);
        assert_eq!(parse_zram_device("/dev/zram0extra"), None);
        assert_eq!(parse_zram_device(""), None);
    }

    /// The kernel's `comp_algorithm` line brackets the active name. Two
    /// brackets are a malformed line and refuse — a junk name would reach
    /// the setup verification and the published sample.
    #[test]
    fn parse_active_zram_algorithm_reads_the_bracketed_name_and_refuses_junk() {
        assert_eq!(
            parse_active_zram_algorithm("[lzo-rle] lzo lz4"),
            Some("lzo-rle")
        );
        assert_eq!(parse_active_zram_algorithm("lzo [lz4]"), Some("lz4"));
        assert_eq!(parse_active_zram_algorithm("lzo lz4"), None);
        assert_eq!(parse_active_zram_algorithm("[lzo] [lz4]"), None);
        assert_eq!(parse_active_zram_algorithm("[]"), None);
    }

    /// The failure classifier advances the compressor walk only on a
    /// capability answer.
    #[test]
    fn zram_allocation_failure_classification_names_only_capability_answers() {
        // The classifier only reads a shell failure's message: that is the
        // spelling the spawn helper guarantees with LC_ALL=C. Any other
        // error kind is Fatal by construction, not by wording.
        let shell = |msg: &str| CascadeError::Shell {
            cmd: "zramctl".into(),
            msg: msg.into(),
        };
        assert_eq!(
            classify_zram_allocation_failure(&shell("Device or resource busy")),
            ZramAllocationFailure::DeviceTransient
        );
        assert_eq!(
            classify_zram_allocation_failure(&shell("failed to set algorithm: Invalid argument")),
            ZramAllocationFailure::AlgorithmUnsupported
        );
        assert_eq!(
            classify_zram_allocation_failure(&shell("something else entirely")),
            ZramAllocationFailure::Fatal
        );
        assert_eq!(
            classify_zram_allocation_failure(&CascadeError::Precondition("busy".into())),
            ZramAllocationFailure::Fatal,
            "a non-shell error carries no wording to classify"
        );
    }

    /// The classifier's second clause: a set-algorithm failure that is not
    /// the `EINVAL` spelling is a fatal refusal, not a capability answer.
    #[test]
    fn zram_algorithm_is_unknown_reads_only_the_einval_spelling() {
        assert!(zram_algorithm_is_unknown(
            "zramctl: failed to set algorithm: Invalid argument"
        ));
        assert!(zram_algorithm_is_unknown("failed to set algorithm: EINVAL"));
        // A set-algorithm failure with a different errno is not a capability
        // answer and must not advance the walk.
        assert!(!zram_algorithm_is_unknown(
            "zramctl: failed to set algorithm: Permission denied"
        ));
        assert!(!zram_algorithm_is_unknown("no free zram device found"));
    }
}
