// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/up_config.rs
//! `ramshared up` argument parsing, transport admission rules, and cascade readiness/precondition checks.
//!
//! Extracted from `cascade/mod.rs` to separate `up` configuration validation from device scanning and telemetry aggregation.

use super::*;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Transport {
    /// Prefer ublk when safe; WSL2 resolves to NBD, which product admission refuses.
    Auto,
    Nbd,
    Ublk,
}

#[derive(Debug)]
pub(super) struct UpArgs {
    pub(super) vram_mb: u64,
    pub(super) zram_mb: u64,
    /// Explicit operator acknowledgment for NBD-backed swap on WSL2 after
    /// the verified test case qualification. Without it the refusal stands; there is
    /// no silent re-enable.
    pub(super) accept_wsl2_nbd: bool,
    /// The lower-tier block device a zram tier is backed by. Bound **before**
    /// the tier is sized (SPEC `tier-ownership-orphan-page-elimination`
    /// DT-2): a zram tier with no backing device holds the only copy of its
    /// pages and can lose them.
    pub(super) zram_backing_dev: Option<String>,
    /// The writeback topology (SPEC zram-writeback-vram-tier DT-1): the
    /// VRAM tier becomes zram's `backing_dev` instead of a swap device, so
    /// the compressed spill lands on the tier and the overflow reaches the
    /// fallback. Off by default; the swap posture stays the product's
    /// default until the pump's evidence qualifies it.
    pub(super) zram_writeback_tier: bool,
    pub(super) daemon: String,
    pub(super) force: bool,
    pub(super) connections: u32,
    pub(super) transport: Transport,
    pub(super) swap_dev: String,
    pub(super) origin_path: String,
    pub(super) origin_partuuid: String,
    pub(super) origin_ptuuid: String,
    pub(super) origin_partition_dev_t: String,
    pub(super) origin_parent_dev_t: String,
    pub(super) expected_swap_uuid: String,
    pub(super) host_manifest_sha256: String,
    pub(super) configuration_sha256: String,
    pub(super) cache_cap_mib: u64,
    pub(super) disk_baseline_kib: u64,
}

/// True when running under Microsoft WSL2 (shared kernel VM).
pub(super) fn is_wsl2() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.contains("microsoft") || s.contains("WSL"))
        .unwrap_or(false)
        || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
        || std::env::var_os("WSL_INTEROP").is_some()
}

/// Pick the transport this host prefers for `auto`.
///
/// Preference only. Named transports pass through unchanged — refusing an
/// unusable transport is `check_transport`'s job, and giving this function a
/// second copy of that policy would give the product two owners for one rule.
/// `auto` is the only value this function chooses for the peer: NBD on WSL2,
/// where the later admission check refuses the unqualified swap path; otherwise
/// ublk when the control node exists and NBD when it does not.
pub(super) fn resolve_transport(t: Transport) -> Transport {
    match t {
        Transport::Nbd | Transport::Ublk => t,
        Transport::Auto => {
            if is_wsl2() {
                eprintln!(
                    "[up] transport=auto -> nbd (WSL2 admission will refuse the unqualified swap path)"
                );
                return Transport::Nbd;
            }
            if Path::new("/dev/ublk-control").exists() {
                eprintln!("[up] transport=auto -> ublk (/dev/ublk-control present, non-WSL2 host)");
                Transport::Ublk
            } else {
                eprintln!("[up] transport=auto -> nbd (no /dev/ublk-control)");
                Transport::Nbd
            }
        }
    }
}

/// Why the product refuses `ublk` as an `up` transport. One spelling, used by
/// the gate and its tests, prevents the reason from drifting.
pub(super) fn ublk_refusal(running_on_wsl2: bool) -> &'static str {
    if running_on_wsl2 {
        "transport ublk refused on WSL2 (freeze risk 2026-06-09; NBD swap is currently gated). \
         There is no safety override. The kernel may provide ublk_drv; the cascade product \
         does not use it."
    } else {
        "transport ublk in `ramshared up` is not implemented yet (future SPEC). \
         Use --transport nbd or auto. A manual ublk daemon is lab-only."
    }
}

/// WSL2 NBD-backed swap is admitted only under an explicit operator
/// acknowledgment after the isolated qualification.
///
/// The qualification exists: peer liveness, a bounded teardown
/// (a measured-dead peer refuses `down` before any kernel swapoff; a hung
/// peer is bounded by the 120 s deadline with the backend preserved),
/// reconnect and restart. The refusal stands without the acknowledgment
/// because a peer failure can still SIGBUS processes holding pages on the
/// tier, and there is no silent re-enable.
pub(super) fn nbd_refusal() -> &'static str {
    "transport NBD on WSL2 requires --accept-wsl2-nbd: isolated testing qualified peer liveness, bounded teardown, reconnect and restart, but a peer failure can still SIGBUS processes holding pages on the tier — keep the configured WSL fallback swap active. Pass --accept-wsl2-nbd to arm it."
}

pub(super) fn nbd_admission_notice() -> &'static str {
    "[up] WSL2 NBD accepted by operator acknowledgment: WSL2 NBD qualification covers peer liveness, bounded teardown, reconnect and restart; a peer failure can still SIGBUS processes holding pages on the tier, so the fallback swap stays active below it."
}

pub(super) fn check_transport_for(
    transport: Transport,
    running_on_wsl2: bool,
    accept_wsl2_nbd: bool,
) -> Result<(), CascadeError> {
    match transport {
        Transport::Auto => Err(CascadeError::Precondition(
            "transport auto must be resolved before admission".into(),
        )),
        Transport::Ublk => Err(CascadeError::Precondition(
            ublk_refusal(running_on_wsl2).into(),
        )),
        Transport::Nbd if running_on_wsl2 && !accept_wsl2_nbd => {
            Err(CascadeError::Precondition(nbd_refusal().into()))
        }
        Transport::Nbd if running_on_wsl2 => {
            eprintln!("{}", nbd_admission_notice());
            Ok(())
        }
        Transport::Nbd => Ok(()),
    }
}

pub(super) fn check_transport(
    transport: Transport,
    accept_wsl2_nbd: bool,
) -> Result<(), CascadeError> {
    check_transport_for(transport, is_wsl2(), accept_wsl2_nbd)
}

/// The product's ublk design is a single ring, so a request for more than one
/// connection is incoherent on every host. Pure decision: no host policy, no
/// I/O. Applied to the transport that will actually be honoured, because
/// `auto` can resolve to ublk on a non-WSL2 host and the constraint follows
/// the resolved transport, not the token the peer typed.
pub(super) fn ensure_single_ring_ublk(
    transport: Transport,
    connections: u32,
) -> Result<(), CascadeError> {
    if transport == Transport::Ublk && connections != 1 {
        return Err(CascadeError::Arg(
            "--connections > 1 is invalid with --transport ublk (single ring)".into(),
        ));
    }
    Ok(())
}

/// Env reader for attended-`up` sizing.
///
/// Production is `std::env`. The test build routes `TEST_ENV_MB` through the
/// same [`boot::Env`] trait the boot path uses, so one DT-3 chain serves both
/// and the test seam cannot drift from the operator-facing resolver.
pub(super) struct UpEnv;

impl boot::Env for UpEnv {
    fn var(&self, key: &str) -> Option<String> {
        #[cfg(test)]
        if let Some((ref k, n)) = TEST_ENV_MB.with(|c| c.borrow().clone())
            && k == key
        {
            return Some(n.to_string());
        }
        std::env::var(key).ok()
    }
}

/// Attended-`up` zram default, resolved through the DT-3 chain.
///
/// `/etc/ramshared/cascade.conf` → `RAMSHARED_ZRAM_MIB` → 1024 MiB. This is the
/// same authority `boot` resolves, so a bare `up` no longer starts half the
/// configured cushion on a host whose policy says otherwise. Explicit `--zram`
/// still wins. `vram_mb` is deliberately *not* taken from here: it stays bound
/// to the sealed origin capacity, which is the size the NBD actually exposes.
pub(super) fn default_zram_mb() -> Result<u64, CascadeError> {
    #[cfg(test)]
    let text = match TEST_CASCADE_CONF.with(|cell| cell.borrow().clone()) {
        Some(Ok(text)) => text,
        Some(Err(message)) => return Err(CascadeError::Io(message)),
        // Test mode never reads the host policy file.
        None => String::new(),
    };
    #[cfg(not(test))]
    let text = match fs::read_to_string(CASCADE_CONF_FILE) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(CascadeError::Io(format!(
                "read {CASCADE_CONF_FILE}: {error}"
            )));
        }
    };
    let resolved = boot::resolve_boot_config_text(&text, &UpEnv)
        .map_err(|error| CascadeError::Arg(error.to_string()))?;
    Ok(resolved.config.zram_mib)
}

/// True when a healthy Day-1 cascade is already mounted (idempotent `up`).
/// SPEC ITEM-5 — pure over `/proc/swaps` + run files + optional pid/socket.
pub fn cascade_already_healthy(entries: &[SwapEntry]) -> bool {
    if !ghost_vram_swaps(entries).is_empty() {
        return false;
    }
    let has_vram_swap = entries.iter().any(|e| {
        !e.is_ghost()
            && (is_nbd_device_path(&e.filename) || is_ublk_device_path(&e.filename))
            && e.is_managed_or_orphan_vram_tier()
    });
    if !has_vram_swap {
        return false;
    }
    // Test seam: injected `/proc/swaps` must not couple to live /run records (orphan/half tests).
    #[cfg(test)]
    if TEST_SWAPS.with(|c| c.borrow().is_some()) {
        return false;
    }
    let has_record = Path::new(SWAP_DEV_FILE).exists() || Path::new(PID_FILE).exists();
    if !has_record {
        return false;
    }
    // Daemon must still be serving, or we have a half-state (caller must down).
    let pid_alive = fs::read_to_string(PID_FILE)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists());
    let sock_ok = Path::new(SOCK).exists();
    pid_alive || sock_ok
}

/// Half-state: records or nbd without a complete healthy cascade → refuse second `up`.
pub(super) fn refuse_half_cascade(entries: &[SwapEntry]) -> Result<(), CascadeError> {
    if cascade_already_healthy(entries) {
        return Ok(());
    }
    // Same test seam as `has_live_records` above: an injected `/proc/swaps`
    // must not couple to live `/run` record paths. Without this, a unit test
    // on a host whose daemon is running sees the operator's real half-state
    // and cannot reach the ordering it is meant to prove. The injected
    // entries still decide `has_vram` — only the hardcoded host paths are
    // ignored.
    #[cfg(test)]
    let has_record = if TEST_SWAPS.with(|c| c.borrow().is_some()) {
        false
    } else {
        Path::new(SWAP_DEV_FILE).exists()
            || Path::new(ZRAM_DEV_FILE).exists()
            || Path::new(PID_FILE).exists()
    };
    #[cfg(not(test))]
    let has_record = Path::new(SWAP_DEV_FILE).exists()
        || Path::new(ZRAM_DEV_FILE).exists()
        || Path::new(PID_FILE).exists();
    let has_vram = entries.iter().any(|e| {
        !e.is_ghost() && (is_nbd_device_path(&e.filename) || is_ublk_device_path(&e.filename))
    });
    if has_record || has_vram {
        return Err(CascadeError::Precondition(
            "half cascade (state under the runtime directory, or a nbd/ublk \
             device with no healthy daemon). Run `sudo ramshared down` and \
             retry `up`. Do not stack a second `up` on top of this one."
                .into(),
        ));
    }
    Ok(())
}

pub(super) fn parse_up_args_from(args: &[String], daemon: String) -> Result<UpArgs, CascadeError> {
    let sealed = read_sealed_origin_config()?;
    let sealed_capacity_mib = sealed.logical_capacity_mib;
    let mut a = UpArgs {
        zram_backing_dev: None,
        zram_writeback_tier: false,
        accept_wsl2_nbd: false,
        vram_mb: sealed.logical_capacity_mib,
        zram_mb: default_zram_mb()?,
        daemon,
        force: false,
        connections: 1,
        // Default auto: on WSL2 resolves to NBD and is refused by admission; ublk is non-WSL2 only.
        transport: Transport::Auto,
        swap_dev: NBD.to_string(),
        origin_path: sealed.origin_path,
        origin_partuuid: sealed.partuuid,
        origin_ptuuid: sealed.ptuuid,
        origin_partition_dev_t: sealed.partition_dev_t,
        origin_parent_dev_t: sealed.parent_dev_t,
        expected_swap_uuid: sealed.expected_swap_uuid,
        host_manifest_sha256: sealed.host_manifest_sha256,
        configuration_sha256: sealed.configuration_sha256,
        cache_cap_mib: sealed.physical_cache_cap_mib,
        disk_baseline_kib: 0,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--vram" => {
                i += 1;
                a.vram_mb = args
                    .get(i)
                    .ok_or_else(|| CascadeError::Arg("--vram requires MiB".into()))?
                    .parse()
                    .map_err(|_| CascadeError::Arg("invalid vram value".into()))?;
            }
            "--zram" => {
                i += 1;
                a.zram_mb = args
                    .get(i)
                    .ok_or_else(|| CascadeError::Arg("--zram requires MiB".into()))?
                    .parse()
                    .map_err(|_| CascadeError::Arg("invalid zram value".into()))?;
            }
            // SPEC tier-ownership-orphan-page-elimination DT-2: the backing
            // device is bound before the tier is sized. A tier with no
            // backing device holds the only copy of its pages.
            "--zram-backing-dev" => {
                i += 1;
                let path = args.get(i).ok_or_else(|| {
                    CascadeError::Arg("--zram-backing-dev requires a path".into())
                })?;
                if !path.starts_with('/') {
                    return Err(CascadeError::Arg(
                        "--zram-backing-dev must be an absolute block-device path".into(),
                    ));
                }
                a.zram_backing_dev = Some(path.clone());
            }
            // SPEC zram-writeback-vram-tier DT-2: the mode is a flag and
            // reversible. The default stays the swap posture.
            "--zram-writeback-tier" => a.zram_writeback_tier = true,
            "--daemon" => {
                i += 1;
                a.daemon = args
                    .get(i)
                    .ok_or_else(|| CascadeError::Arg("--daemon requires a path".into()))?
                    .clone();
            }
            "--connections" => {
                i += 1;
                a.connections = args
                    .get(i)
                    .ok_or_else(|| CascadeError::Arg("--connections requires N".into()))?
                    .parse()
                    .map_err(|_| CascadeError::Arg("invalid connections value".into()))?;
                if a.connections == 0 {
                    return Err(CascadeError::Arg("--connections must be >= 1".into()));
                }
            }
            "--transport" => {
                i += 1;
                a.transport = match args
                    .get(i)
                    .ok_or_else(|| CascadeError::Arg("--transport requires a value".into()))?
                    .as_str()
                {
                    "auto" => Transport::Auto,
                    "nbd" => Transport::Nbd,
                    "ublk" => Transport::Ublk,
                    other => {
                        return Err(CascadeError::Arg(format!(
                            "invalid --transport: {other} (use auto|nbd|ublk)"
                        )));
                    }
                };
            }
            "--swap-dev" => {
                i += 1;
                a.swap_dev = args
                    .get(i)
                    .ok_or_else(|| CascadeError::Arg("--swap-dev requires a path".into()))?
                    .clone();
            }
            "--origin" | "--origin-manifest" => {
                return Err(CascadeError::Arg(
                    "origin identity is sealed by the host gate and cannot be overridden".into(),
                ));
            }
            "--nbd" => {
                i += 1;
                a.swap_dev = args
                    .get(i)
                    .ok_or_else(|| CascadeError::Arg("--nbd requires a path".into()))?
                    .clone();
                a.transport = Transport::Nbd;
            }
            "--force-no-safety-net" => a.force = true,
            "--accept-wsl2-nbd" => a.accept_wsl2_nbd = true,
            other => return Err(CascadeError::Arg(format!("unknown argument: {other}"))),
        }
        i += 1;
    }
    // Resolve Auto after parse so env/flag still work, then validate the
    // transport that will actually be honoured. One owner for the single-ring
    // constraint: checking it twice inline is how a rule ends up with two
    // spellings and one of them drifting.
    a.transport = resolve_transport(a.transport);
    ensure_single_ring_ublk(a.transport, a.connections)?;
    if a.cache_cap_mib < DEFAULT_PHYSICAL_CACHE_CAP_MIB || a.cache_cap_mib > a.vram_mb {
        return Err(CascadeError::Precondition(
            "physical cache cap must be between 1024 MiB and logical capacity".into(),
        ));
    }
    // The daemon already refuses a `--size` that leaves the sealed origin
    // capacity (`validate_origin_manifest_identity`). Refusing here instead
    // keeps `up`/`boot` from mutating devices first and only then watching the
    // daemon die — the anti-hang contract is about half-states, and a late
    // refusal leaves one.
    if a.vram_mb != sealed_capacity_mib {
        return Err(CascadeError::Precondition(format!(
            "--vram {} MiB differs from the sealed origin capacity {} MiB; \
             the daemon refuses a logical capacity that leaves the seal, \
             so this is refused before any device is touched",
            a.vram_mb, sealed_capacity_mib
        )));
    }
    if !canonical_origin_uuid(&a.expected_swap_uuid) {
        return Err(CascadeError::Precondition(
            "sealed origin swap UUID is missing or invalid".into(),
        ));
    }
    Ok(a)
}

/// Test seam for the advertised-surface contract in [`crate::usage`].
///
/// `UpArgs` is private on purpose; the usage catalog only needs the parse
/// verdict. Not present outside `cfg(test)`, so this is not a production
/// visibility change.
#[cfg(test)]
pub(crate) fn parse_up_args_verdict_for_test(args: &[&str]) -> Result<(), CascadeError> {
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    parse_up_args_from(&owned, "ramsharedd".to_string()).map(|_| ())
}
