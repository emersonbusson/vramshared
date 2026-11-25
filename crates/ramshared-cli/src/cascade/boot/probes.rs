// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/boot/probes.rs
//! Live-host probes and identity observers for native cascade bootstrap.

use std::fs;
use std::path::{Path, PathBuf};

use crate::customer::transition::CustomerTransitionGuard;

use super::{BootConfig, BootError, BootHost, BootPaths, ScopedApproval, read_scoped_approval};

/// Production host: the live WSL2 machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemBootHost {
    pub product_root: PathBuf,
}

impl SystemBootHost {
    pub fn new(product_root: PathBuf) -> Self {
        Self { product_root }
    }
}

impl Default for SystemBootHost {
    fn default() -> Self {
        Self::new(BootPaths::production().product_root)
    }
}

impl BootHost for SystemBootHost {
    fn identity_gate(&self, config: &BootConfig, running_release: &str) -> Result<(), BootError> {
        let input = self.observe_product_input(config, running_release)?;
        let decision = ramshared_tier::nbd_readiness::evaluate_product(&input);
        match decision.reason {
            ramshared_tier::nbd_readiness::ReadinessReason::NotEvaluated
            | ramshared_tier::nbd_readiness::ReadinessReason::ProductOff
            | ramshared_tier::nbd_readiness::ReadinessReason::AllGatesPass => Ok(()),
            ramshared_tier::nbd_readiness::ReadinessReason::Refusal(code) => {
                Err(BootError::Identity(code))
            }
        }
    }

    fn resolve_approval(
        &self,
        approval_dir: &Path,
        running_release: &str,
    ) -> Result<ScopedApproval, BootError> {
        read_scoped_approval(approval_dir, running_release)
    }

    fn dirty_gate(&self) -> Result<(), BootError> {
        let entries = crate::cascade::read_swaps()
            .map_err(|_| BootError::DirtyState("cannot read /proc/swaps to prove a clean state"))?;
        crate::cascade::refuse_half_cascade(&entries).map_err(|_| {
            BootError::DirtyState("ghost or half cascade is present; run `sudo ramshared down`")
        })
    }

    fn already_healthy(&self) -> Option<bool> {
        crate::cascade::read_swaps()
            .ok()
            .map(|entries| crate::cascade::cascade_already_healthy(&entries))
    }

    fn activate(&self, guard: &CustomerTransitionGuard, args: &[String]) -> Result<(), String> {
        crate::cascade::cascade_io::up_with_args_held(guard, args)
            .map_err(|error| error.to_string())
    }
}

impl SystemBootHost {
    /// One fresh observation for [`ramshared_tier::nbd_readiness::evaluate_product`].
    ///
    /// DT-2: the policy lives in `nbd_readiness`; this only gathers facts.
    /// Anything that cannot be measured safely is `Gate::Unknown` /
    /// `CapacitySample::Unknown`, which `evaluate_product` refuses — never a
    /// silent success (invariant checksecurity checklist).
    pub(super) fn observe_product_input(
        &self,
        config: &BootConfig,
        running_release: &str,
    ) -> Result<ramshared_tier::nbd_readiness::ProductInput, BootError> {
        use ramshared_tier::nbd_readiness::{
            Approval, Gate, Operation, ProductInput, ProductTransport,
        };

        let current = self.product_root.join("current");
        let (nbd_swap_active, daemon_running) = observe_lifecycle();
        let cli_match = observe_cli_binary_match(&current);
        let release_gate = observe_release_gate(&current, running_release);
        let relay_gate = observe_relay_gate(&current);
        let capacity = observe_lower_tier_capacity(&current);
        let daemon_match = observe_daemon_binary_match(&current, daemon_running);

        // The CLI's own identity is a boot property (RF-8) checked outside the
        // daemon-scoped `binary_match` field: `evaluate_product` models the
        // daemon, `boot` also has to prove the running CLI is the sealed one.
        if let Gate::Fail = cli_match {
            return Err(BootError::Identity(
                ramshared_tier::nbd_readiness::RefusalCode::BinaryMatchFailed,
            ));
        }
        if let Gate::Unknown = cli_match {
            return Err(BootError::Identity(
                ramshared_tier::nbd_readiness::RefusalCode::BinaryMatchUnknown,
            ));
        }

        Ok(ProductInput {
            // The transport is the swap table's own answer, not a second
            // probe: an unreadable table leaves both `None`.
            transport: nbd_swap_active.map(|active| {
                if active {
                    ProductTransport::Nbd
                } else {
                    ProductTransport::None
                }
            }),
            nbd_swap_active,
            daemon_running,
            legacy_ublk_product_active: observe_legacy_ublk(),
            release_gate,
            relay_gate,
            binary_match: daemon_match,
            capacity,
            vram_bytes: config
                .vram_mib
                .saturating_mul(ramshared_tier::nbd_readiness::MIB_BYTES),
            // Read-only classification: approval is a separate later gate (RF-9)
            // and `boot` must not ask `evaluate_product` to authorize the
            // mutation it has not yet reached.
            operation: Operation::ReadOnly,
            approval: Approval::NotRequired,
            reboot_requested: false,
        })
    }
}

/// Managed NBD swap present + managed daemon alive. Both must agree; a
/// disagreement is `NbdLifecycleIncomplete` inside `evaluate_product`.
///
/// The swap half is a three-state witness: `Some(true)` measured a managed
/// NBD swap present, `Some(false)` measured its absence, `None` never took
/// the probe because `/proc/swaps` was unreadable. `None` is not `false`.
pub(super) fn observe_lifecycle() -> (Option<bool>, bool) {
    let nbd_swap_active = crate::cascade::read_swaps().ok().map(|entries| {
        entries
            .iter()
            .any(|entry| !entry.is_ghost() && crate::cascade::is_nbd_device_path(&entry.filename))
    });
    let daemon_running = std::fs::read_to_string(crate::cascade::PID_FILE)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists())
        || Path::new(crate::cascade::SOCK).exists();
    (nbd_swap_active, daemon_running)
}

/// A live legacy ublk product device is a hard refusal in `evaluate_product`.
/// Observed from /proc/swaps: any ublk device path means the legacy transport
/// is still holding the product slot.
///
/// `Some(true)` measured one present, `Some(false)` measured its absence,
/// `None` never took the probe. The `None` arm is the fail-open the old
/// `.unwrap_or(false)` had: an unreadable table used to *skip* the hard
/// refusal and let a live legacy ublk product through.
pub(super) fn observe_legacy_ublk() -> Option<bool> {
    crate::cascade::read_swaps().ok().map(|entries| {
        entries
            .iter()
            .any(|entry| crate::cascade::is_ublk_device_path(&entry.filename))
    })
}

/// Read `SHA256SUMS` under the selected release and return the hex digest for
/// `member`, or `None` when the manifest is absent or does not name it.
pub(super) fn sealed_digest(current: &Path, member: &str) -> Option<String> {
    use crate::release_identity::{SHA256SUMS_FILE, read_regular_text};
    // Same regular-file rule as the manifest re-verification: a symlinked
    // `SHA256SUMS` is not a sealed manifest, and following one here while
    // `sha256_file` refuses it would be the DT-12 dual-predicate again.
    let manifest = read_regular_text(&current.join(SHA256SUMS_FILE))?;
    for line in manifest.lines() {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?;
        if name == member || name.strip_prefix("./") == Some(member) {
            return Some(digest.to_ascii_lowercase());
        }
    }
    None
}

pub(super) fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn observe_cli_binary_match(current: &Path) -> ramshared_tier::nbd_readiness::Gate {
    use ramshared_tier::nbd_readiness::Gate;
    let Some(expected) = sealed_digest(current, "bin/ramshared") else {
        return Gate::Unknown;
    };
    let Ok(exe) = std::env::current_exe() else {
        return Gate::Unknown;
    };
    let Ok(digest) = sha256_path(&exe) else {
        return Gate::Unknown;
    };
    if digest == expected {
        Gate::Pass
    } else {
        Gate::Fail
    }
}

pub(super) fn observe_daemon_binary_match(
    current: &Path,
    daemon_running: bool,
) -> ramshared_tier::nbd_readiness::Gate {
    use ramshared_tier::nbd_readiness::Gate;
    if !daemon_running {
        // `evaluate_product` accepts `NotApplicable` only while no daemon is up.
        return Gate::NotApplicable;
    }
    let Some(expected) = sealed_digest(current, "bin/ramsharedd") else {
        return Gate::Unknown;
    };
    let Ok(pid_text) = fs::read_to_string(crate::cascade::PID_FILE) else {
        return Gate::Unknown;
    };
    let Ok(pid) = pid_text.trim().parse::<i32>() else {
        return Gate::Unknown;
    };
    let exe = PathBuf::from(format!("/proc/{pid}/exe"));
    let Ok(digest) = sha256_path(&exe) else {
        return Gate::Unknown;
    };
    if digest == expected {
        Gate::Pass
    } else {
        Gate::Fail
    }
}

/// Sealed release ownership for [`SystemBootHost::identity_gate`].
///
/// `running_release` is the **bundle identity** of the running executable
/// ([`release_version`]), compared with the `RELEASE_VERSION` the `current`
/// selector names. Equal means the binary I am executing is the one this
/// selected release ships: a binary from release A against a `current`
/// pointing at release B is `Fail`. That is the selector half of
/// `nbd_readiness::ProductInput::release_gate`; the manifest half is the
/// `SHA256SUMS` seal. Binary-level equality is the separate `BINARY_MATCH`
/// gate (RF-2/RF-8).
pub(super) fn observe_release_gate(
    current: &Path,
    running_release: &str,
) -> ramshared_tier::nbd_readiness::Gate {
    use ramshared_tier::nbd_readiness::Gate;
    // The same reader [`sealed_release_identity`] uses. A `RELEASE_VERSION` the
    // sealed reader refuses must not be laundered into a Pass by a more
    // permissive second reader (DT-10): that is how a malformed file matched
    // the crate-semver fallback and gated an unverified identity.
    let Some(version) = crate::release_identity::read_release_version(current) else {
        return Gate::Unknown;
    };
    if version != running_release {
        return Gate::Fail;
    }
    // Presence means a regular file, the same predicate `sealed_digest` and
    // `sha256_file` apply. `fs::metadata` follows symlinks, so a symlinked
    // seal would have counted as present here and been refused everywhere the
    // manifest is actually verified (DT-12).
    if !crate::release_identity::is_regular_file(
        &current.join(crate::release_identity::SHA256SUMS_FILE),
    ) {
        return Gate::Unknown;
    }
    Gate::Pass
}

pub(super) fn observe_relay_gate(current: &Path) -> ramshared_tier::nbd_readiness::Gate {
    crate::cascade::relay_health::observe(current)
}

/// One fresh lower-tier sample from the sealed release's bound sink.
///
/// `pub(crate)` so [`crate::customer::setup_command`] plans against the same
/// observation `boot` gates on. One observer, two consumers: a second copy
/// would let the plan and the gate disagree about what the sink can absorb.
pub(crate) fn observe_lower_tier_capacity(
    current: &Path,
) -> ramshared_tier::nbd_readiness::CapacitySample {
    use ramshared_tier::nbd_readiness::{CapacitySample, LowerTierSink};
    // The lower-tier sink binding (`NBD_LOWER_SINK*`) is owned by
    // `nbd-product-preflight.sh` (DT-3) and lives in the sealed release's
    // `cascade.conf.example`, which is where the installer writes it and where
    // DT-3 validates it. It is never in `/etc/ramshared/cascade.conf`: that
    // file is host sizing policy, and `parse_cascade_conf` ignores exactly
    // these keys for that reason. `boot` does not invent a sink — it consumes
    // the bound one and measures its free capacity. Without an exact binding
    // the sample is `Unknown` and `evaluate_product` refuses with
    // `LOWER_TIER_SINK_UNKNOWN` (never a silent pass).
    let binding = current.join("scripts/safety/cascade.conf.example");
    let Ok(raw) = fs::read_to_string(&binding) else {
        return CapacitySample::Unknown;
    };
    if conf_value(&raw, "NBD_LOWER_SINK_BINDING").as_deref() != Some("bound") {
        return CapacitySample::Unknown;
    }
    if conf_value(&raw, "NBD_LOWER_SINK_TYPE").as_deref() != Some("directory") {
        return CapacitySample::Unknown;
    }
    let Some(sink) = conf_value(&raw, "NBD_LOWER_SINK") else {
        return CapacitySample::Unknown;
    };
    // DT-3 owns the filesystem block size of the bound sink; the capacity
    // formula's alignment must be that same number, not a second constant.
    let Some(alignment) = conf_value(&raw, "NBD_LOWER_SINK_FS_BLOCK_BYTES")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|bytes| *bytes > 0)
    else {
        return CapacitySample::Unknown;
    };
    let Ok(meta) = fs::metadata(&sink) else {
        return CapacitySample::Unknown;
    };
    if !meta.is_dir() {
        return CapacitySample::Unknown;
    }
    let Ok(stats) = statvfs_free_bytes(&sink) else {
        return CapacitySample::Unknown;
    };
    CapacitySample::Observed {
        sink: LowerTierSink::Known(sink),
        free_absorbable_bytes: stats,
        alignment_bytes: alignment,
    }
}

pub(super) fn conf_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line.split_once('=')?;
        if name.trim() == key {
            let value = value.trim().to_string();
            if value.is_empty() {
                return None;
            }
            return Some(value);
        }
    }
    None
}

/// Free bytes on the filesystem holding `path`, via `statvfs(2)` (safe rustix).
pub(super) fn statvfs_free_bytes(path: &str) -> Result<u64, &'static str> {
    let stat = rustix::fs::statvfs(path).map_err(|_| "statvfs failed on the lower-tier sink")?;
    let frsize = if stat.f_frsize != 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    Ok(stat.f_bavail.saturating_mul(frsize))
}

pub(super) fn sha256_path(path: &Path) -> Result<String, &'static str> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path).map_err(|_| "cannot read sealed binary")?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex_digest(&hasher.finalize()))
}

/// Running **sealed release identity** — the `RELEASE_VERSION` of the bundle
/// the running executable ships in.
///
/// That string is `git describe` (`v0.14.1-386-gdffa5ea2-dirty`), never the
/// workspace crate semver (`0.15.0`). Three consumers must see one identity:
/// `scripts/package/build-linux-bundle.sh` writes `git describe --always
/// --dirty --tags` into `RELEASE_VERSION`, `install-cascade-boot.sh` names
/// `/opt/ramshared/releases/<that>` and mints `activate:<that>`, and
/// `scripts/safety/cascade-up.sh` mints the same token from the release
/// directory name (DT-4).
///
/// This used to return `CARGO_PKG_VERSION`, so it was compared against the
/// bundle identity as if the two namespaces were one. Every real sealed
/// install then refused `RELEASE_GATE_FAILED` (identity gate) and
/// `APPROVAL_STALE_VERSION` (approval gate) — the two gates RF-9 and RF-2
/// exist to protect.
///
/// Outside a sealed bundle (tests, `target/`) this falls back to the crate
/// version. No `git describe` identity equals a bare crate semver, so a
/// direct-install binary still cannot boot a sealed release.
pub fn release_version() -> String {
    sealed_release_identity(std::env::current_exe().ok().as_deref()).unwrap_or_else(|| {
        option_env!("CARGO_PKG_VERSION")
            .unwrap_or("0.0.0")
            .to_string()
    })
}

/// `RELEASE_VERSION` above the running executable, which is the release root
/// for both `bin/ramshared` and `bin/ramsharedd`. `None` when the executable
/// is not sealed into a release bundle.
pub(super) fn sealed_release_identity(executable: Option<&Path>) -> Option<String> {
    let release_root = executable?.parent()?.parent()?;
    crate::release_identity::read_release_version(release_root)
}
