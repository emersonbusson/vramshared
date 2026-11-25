// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/boot.rs
//! Native cascade bootstrap (`ramshared boot`).
//!
//! Owns config loading (RF-4), the gate sequence of PRD §3 steps 1–4, and the
//! refusal surface (RF-2, NFR-6). `boot` is gate-then-act: every gate completes
//! before the first mutation, there is no retry loop (NFR-5), and the function
//! never writes to a product binary path (RF-8, DT-7).
//!

use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use super::posture::{self, Posture, PostureError};
use crate::customer::transition::{CustomerTransitionGuard, OperationOwner};

/// Environment seam so sizing resolution is pure and testable (DT-3).
pub trait Env {
    fn var(&self, key: &str) -> Option<String>;
}

/// Production environment reader.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Built-in conservative sizing (RF-4, DT-3).
pub const DEFAULT_VRAM_MIB: u64 = 1024;
/// Built-in conservative sizing (RF-4, DT-3).
pub const DEFAULT_ZRAM_MIB: u64 = 1024;
/// Built-in shared-hardware cushion (DT-3).
///
/// Bound to the sealed GPU free-floor authority — `MIN_VRAM_HEADROOM_MIB` is
/// the operator-facing name for the same configured reserve
/// (`ramshared_vram::SEALED_RESERVE_MIN_MIB`) that `ReserveFloorPolicy` and
/// `preflight.sh` enforce. A built-in of 256 MiB is the retired silent default
/// that admitted a shared host with almost no GPU headroom; it must not return.
pub const DEFAULT_MIN_VRAM_HEADROOM_MIB: u64 = ramshared_vram::SEALED_RESERVE_MIN_MIB;

/// Config keys accepted in `/etc/ramshared/cascade.conf` (PRD §7).
pub const CONF_KEY_VRAM_MIB: &str = "VRAM_MIB";
/// Config keys accepted in `/etc/ramshared/cascade.conf` (PRD §7).
pub const CONF_KEY_ZRAM_MIB: &str = "ZRAM_MIB";
/// Config keys accepted in `/etc/ramshared/cascade.conf` (PRD §7).
pub const CONF_KEY_MIN_VRAM_HEADROOM_MIB: &str = "MIN_VRAM_HEADROOM_MIB";

/// Approved activation verb (DT-4).
pub const APPROVAL_VERB: &str = "activate";

/// Durable WSL2 SIGBUS-risk acknowledgment verb (DT-13).
pub const WSL2_ACCEPT_VERB: &str = "accept-wsl2-nbd";

/// File name prefix of the durable WSL2 acknowledgment token.
pub const WSL2_ACCEPT_FILE_PREFIX: &str = "wsl2-nbd-ack";

/// Resolved bootstrap sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootConfig {
    pub vram_mib: u64,
    pub zram_mib: u64,
    pub min_vram_headroom_mib: u64,
}

/// Where each sizing value came from (observability, DT-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// `/etc/ramshared/cascade.conf`.
    Etc,
    /// `RAMSHARED_*` / `MIN_VRAM_HEADROOM_MIB` environment.
    Env,
    /// Built-in conservative default.
    Default,
}

impl ConfigSource {
    /// Stable spelling for logs, journal lines, and `status` JSON.
    #[allow(dead_code)] // consumed by the ITEM-5 display surface
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Etc => "etc",
            Self::Env => "env",
            Self::Default => "default",
        }
    }
}

/// Resolved sizing plus provenance for `status --json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedBootConfig {
    pub config: BootConfig,
    pub vram_source: ConfigSource,
    pub zram_source: ConfigSource,
    pub headroom_source: ConfigSource,
}

impl Default for BootConfig {
    fn default() -> Self {
        Self {
            vram_mib: DEFAULT_VRAM_MIB,
            zram_mib: DEFAULT_ZRAM_MIB,
            min_vram_headroom_mib: DEFAULT_MIN_VRAM_HEADROOM_MIB,
        }
    }
}

/// Partial sizing parsed from `/etc/ramshared/cascade.conf`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CascadeConf {
    vram_mib: Option<u64>,
    zram_mib: Option<u64>,
    min_vram_headroom_mib: Option<u64>,
}

/// Version-scoped activation approval (DT-4, RF-9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedApproval {
    pub verb: String,
    pub release: String,
    pub vram_mib: Option<u64>,
    pub zram_mib: Option<u64>,
    /// The posture the attended enrollment bound to this release (DT-1,
    /// DT-2). There is no default: an absent binding is `POSTURE_ABSENT`.
    pub posture: Posture,
}

impl ScopedApproval {
    /// Version **equality** with the running release. A different release is
    /// stale and must not authorize this one (RF-9).
    pub fn matches_release(&self, release: &str) -> bool {
        self.release == release
    }
}

/// Deterministic bootstrap refusals. Never retried (NFR-5, invariant check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// `/etc/ramshared/cascade.conf` is present but not usable.
    ConfigInvalid(String),
    /// No approval token for the running release.
    ApprovalMissing,
    /// Approval names another release (RF-9).
    ApprovalStaleVersion { found: String, running: String },
    /// Token is not root-owned, or is group/world-writable (security checklist).
    ApprovalUntrusted(&'static str),
    /// The durable WSL2 acknowledgment is malformed, untrusted, or names a
    /// different release. Absence is not an error: it leaves the explicit
    /// `--accept-wsl2-nbd` gate in force on WSL2.
    Wsl2Acceptance(&'static str),
    /// The token carries no posture, an unknown one, or one whose
    /// prerequisites the resolved configuration does not meet (DT-1..DT-3).
    Posture(PostureError),
    /// Sealed identity / binary-match gate refused (DT-2).
    Identity(ramshared_tier::nbd_readiness::RefusalCode),
    /// Origin, Guardian, or host lease is missing or stale.
    HostPrerequisite(&'static str),
    /// Ghost swap, half cascade, or unbound managed device is present.
    DirtyState(&'static str),
    /// `boot` was invoked with deploy/upgrade arguments (DT-7).
    DeployRefused,
    /// Customer-mutation custody refused (ITEM-6 / DT-4).
    ///
    /// The reason is the stable customer reason string so `status` can surface
    /// it without parsing prose. Deterministic; never retried (NFR-5).
    Custody {
        reason: &'static str,
        detail: String,
    },
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigInvalid(detail) => write!(f, "cascade.conf invalid: {detail}"),
            Self::ApprovalMissing => {
                write!(f, "scoped approval missing for this release")
            }
            Self::ApprovalStaleVersion { found, running } => write!(
                f,
                "scoped approval names release {found}, running release is {running}"
            ),
            Self::ApprovalUntrusted(reason) => write!(f, "approval token untrusted: {reason}"),
            Self::Wsl2Acceptance(reason) => write!(f, "WSL2 NBD acceptance untrusted: {reason}"),
            Self::Posture(error) => write!(f, "{error}"),
            Self::Identity(code) => write!(f, "identity gate refused: {}", code.as_str()),
            Self::HostPrerequisite(reason) => write!(f, "host prerequisite: {reason}"),
            Self::DirtyState(reason) => write!(f, "dirty cascade state: {reason}"),
            Self::DeployRefused => {
                write!(f, "boot has no deploy API; install or upgrade explicitly")
            }
            Self::Custody { reason, detail } => {
                write!(f, "mutation custody refused ({reason}): {detail}")
            }
        }
    }
}

impl std::error::Error for BootError {}

/// Paths the bootstrap consults. Injected so tests never touch live host state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPaths {
    pub config: PathBuf,
    pub approval_dir: PathBuf,
    pub lease: PathBuf,
    pub product_root: PathBuf,
}

impl BootPaths {
    /// Production layout (PRD §7, DT-4).
    pub fn production() -> Self {
        Self {
            config: PathBuf::from("/etc/ramshared/cascade.conf"),
            approval_dir: PathBuf::from("/var/lib/ramshared/approvals"),
            lease: PathBuf::from("/run/ramshared/host-resume-lease.json"),
            product_root: PathBuf::from("/opt/ramshared"),
        }
    }
}

/// Parse `KEY=VALUE` lines from `/etc/ramshared/cascade.conf`.
///
/// The file is data, not shell: comments and blanks are skipped, unknown keys
/// are ignored (the sealed example carries NBD binding keys owned by
/// `nbd-product-preflight.sh`), and a malformed known key is a hard error.
fn parse_cascade_conf(text: &str) -> Result<CascadeConf, BootError> {
    let mut conf = CascadeConf::default();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(BootError::ConfigInvalid(format!(
                "line {}: expected KEY=VALUE",
                index + 1
            )));
        };
        let key = key.trim();
        let value = value.trim();
        let parsed = match key {
            CONF_KEY_VRAM_MIB => &mut conf.vram_mib,
            CONF_KEY_ZRAM_MIB => &mut conf.zram_mib,
            CONF_KEY_MIN_VRAM_HEADROOM_MIB => &mut conf.min_vram_headroom_mib,
            _ => continue,
        };
        let n: u64 = value.parse().map_err(|_| {
            BootError::ConfigInvalid(format!(
                "line {}: {key} must be a non-negative integer",
                index + 1
            ))
        })?;
        if n == 0 {
            return Err(BootError::ConfigInvalid(format!(
                "line {}: {key} must be greater than zero",
                index + 1
            )));
        }
        *parsed = Some(n);
    }
    Ok(conf)
}

fn env_u64(env: &dyn Env, key: &str) -> Option<u64> {
    env.var(key)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .and_then(|s| match s.parse::<u64>() {
            Ok(n) if n > 0 => Some(n),
            _ => None,
        })
}

/// Resolve sizing: `/etc/ramshared/cascade.conf` → env → built-in defaults (DT-3).
///
/// The sealed `cascade.conf.example` is never read here; it keeps only the
/// machine binding keys that `nbd-product-preflight.sh` owns.
pub fn load_boot_config_from(path: &Path, env: &dyn Env) -> Result<BootConfig, BootError> {
    Ok(resolve_boot_config_from(path, env)?.config)
}

/// Resolve sizing with provenance for observability.
pub fn resolve_boot_config_from(
    path: &Path,
    env: &dyn Env,
) -> Result<ResolvedBootConfig, BootError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(BootError::ConfigInvalid(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };
    resolve_boot_config_text(&text, env)
}

/// Resolve sizing from an already-read `cascade.conf` body (DT-3).
///
/// Split out of `resolve_boot_config_from` so the attended `up` path and the
/// tests share one chain: `/etc/ramshared/cascade.conf` → env → built-in
/// defaults. An empty body is the same as a missing file.
pub fn resolve_boot_config_text(
    text: &str,
    env: &dyn Env,
) -> Result<ResolvedBootConfig, BootError> {
    let conf = parse_cascade_conf(text)?;

    let env_vram = env_u64(env, "RAMSHARED_VRAM_MIB");
    let env_zram = env_u64(env, "RAMSHARED_ZRAM_MIB");
    let env_headroom = env_u64(env, CONF_KEY_MIN_VRAM_HEADROOM_MIB);

    let (vram_mib, vram_source) = match (conf.vram_mib, env_vram) {
        (Some(n), _) => (n, ConfigSource::Etc),
        (None, Some(n)) => (n, ConfigSource::Env),
        (None, None) => (DEFAULT_VRAM_MIB, ConfigSource::Default),
    };
    let (zram_mib, zram_source) = match (conf.zram_mib, env_zram) {
        (Some(n), _) => (n, ConfigSource::Etc),
        (None, Some(n)) => (n, ConfigSource::Env),
        (None, None) => (DEFAULT_ZRAM_MIB, ConfigSource::Default),
    };
    let (min_vram_headroom_mib, headroom_source) = match (conf.min_vram_headroom_mib, env_headroom)
    {
        (Some(n), _) => (n, ConfigSource::Etc),
        (None, Some(n)) => (n, ConfigSource::Env),
        (None, None) => (DEFAULT_MIN_VRAM_HEADROOM_MIB, ConfigSource::Default),
    };
    // DT-8 raise-only, same rule `preflight.sh` and `ReserveFloorPolicy` apply
    // to this name: the operator may be more conservative than the seal, never
    // less. A value below the sealed authority is a refusal, not a clamp.
    if min_vram_headroom_mib < DEFAULT_MIN_VRAM_HEADROOM_MIB {
        return Err(BootError::ConfigInvalid(format!(
            "MIN_VRAM_HEADROOM_MIB={min_vram_headroom_mib} is below the sealed authority \
             {DEFAULT_MIN_VRAM_HEADROOM_MIB} MiB (raise-only)"
        )));
    }

    Ok(ResolvedBootConfig {
        config: BootConfig {
            vram_mib,
            zram_mib,
            min_vram_headroom_mib,
        },
        vram_source,
        zram_source,
        headroom_source,
    })
}

/// Parse the version-scoped approval wire format (DT-2):
/// `activate:<release>[:vram=<n>][:zram=<n>]:posture=<writeback|swap>`.
///
/// The posture is mandatory: absence never chooses a posture, and an unknown
/// spelling is refused with its stable code rather than corrected.
pub fn parse_scoped_approval(raw: &str) -> Result<ScopedApproval, BootError> {
    let raw = raw.trim();
    let mut parts = raw.split(':');
    let verb = parts.next().unwrap_or_default();
    if verb != APPROVAL_VERB {
        return Err(BootError::ApprovalUntrusted(
            "approval verb must be 'activate'",
        ));
    }
    let release = parts.next().unwrap_or_default();
    if release.is_empty() {
        return Err(BootError::ApprovalUntrusted("approval release is empty"));
    }
    let mut approval = ScopedApproval {
        verb: verb.to_string(),
        release: release.to_string(),
        vram_mib: None,
        zram_mib: None,
        posture: Posture::Writeback,
    };
    let mut posture = None;
    for extra in parts {
        let (key, value) = extra.split_once('=').ok_or(BootError::ApprovalUntrusted(
            "approval binding must be key=value",
        ))?;
        match key.trim() {
            "posture" => {
                if posture.is_some() {
                    return Err(BootError::ApprovalUntrusted(
                        "approval carries more than one posture binding",
                    ));
                }
                posture = Some(posture::parse_binding(value).map_err(BootError::Posture)?);
            }
            "vram" | "zram" => {
                let n: u64 = value.trim().parse().map_err(|_| {
                    BootError::ApprovalUntrusted("approval size binding must be a positive integer")
                })?;
                if n == 0 {
                    return Err(BootError::ApprovalUntrusted(
                        "approval size binding must be a positive integer",
                    ));
                }
                if key.trim() == "vram" {
                    approval.vram_mib = Some(n);
                } else {
                    approval.zram_mib = Some(n);
                }
            }
            _ => {
                return Err(BootError::ApprovalUntrusted(
                    "approval carries an unknown binding",
                ));
            }
        }
    }
    approval.posture = posture.ok_or(BootError::Posture(PostureError::Absent))?;
    Ok(approval)
}

/// Token trust rule, pure over `stat(2)` bits so both directions are testable.
fn approval_token_trust(uid: u32, gid: u32, mode: u32) -> Result<(), &'static str> {
    let _ = gid;
    if uid != 0 {
        return Err("approval token is not owned by root");
    }
    // Reject group/other write. A world-writable token is a privilege bypass:
    // any user could mint activate:<running>.
    if mode & 0o022 != 0 {
        return Err("approval token is group- or world-writable");
    }
    Ok(())
}

/// A token that is group/world-writable, or not owned by root, is refused as
/// `ApprovalUntrusted` and never treated as `Present`.
///
/// Operates on the `Metadata` already collected by the caller (no TOCTOU).
fn approval_token_is_root_owned(_path: &Path, meta: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        approval_token_trust(meta.uid(), meta.gid(), meta.mode()).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// Verify the host-resume lease is present, well-formed, and bound to this
/// boot.
///
/// `boot` **verifies** the lease; it never mints one (minting is
/// `ramshared-host-gate.service` on a normal boot, or `recover --resume`
/// after an incident). The validity rule is the one in
/// `workload::verify_host_resume_lease_at`: the lease is boot-ID-bound, and
/// that binding is its lifetime — there is no wall-clock deadline to compare
/// against, and `host_gate::lease_expired` (which answers for the in-memory
/// `host_gate::LeaseToken`, a broker token) does not apply to this file.
/// The caller injects `boot_id` so the boundary is testable without rebooting.
fn verify_host_lease(path: &Path, boot_id: &str) -> Result<(), BootError> {
    crate::workload::verify_host_resume_lease_at(path, boot_id)
        .map(|_| ())
        .map_err(BootError::HostPrerequisite)
}

/// DT-7: `boot` has no deploy API. Reject any invocation that carries
/// install/upgrade/replace style arguments before a single gate runs.
pub fn parse_boot_invocation(args: &[String]) -> Result<(), BootError> {
    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--install"
                | "--upgrade"
                | "--deploy"
                | "--replace"
                | "--release"
                | "--force"
                | "--uninstall"
        ) || arg.starts_with("--release=")
    }) {
        return Err(BootError::DeployRefused);
    }
    Ok(())
}

/// Locate the approval token for `release` and prove it trusted (RF-9).
pub(super) fn read_scoped_approval(
    approval_dir: &Path,
    running_release: &str,
) -> Result<ScopedApproval, BootError> {
    let token_path = approval_dir.join(format!("{APPROVAL_VERB}-{running_release}.token"));
    let meta = fs::metadata(&token_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BootError::ApprovalMissing
        } else {
            BootError::ApprovalUntrusted("approval token is unreadable")
        }
    })?;
    if !approval_token_is_root_owned(&token_path, &meta) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let reason = approval_token_trust(meta.uid(), meta.gid(), meta.mode())
                .err()
                .unwrap_or("approval token failed trust check");
            return Err(BootError::ApprovalUntrusted(reason));
        }
        #[cfg(not(unix))]
        {
            return Err(BootError::ApprovalUntrusted(
                "approval token trust requires unix",
            ));
        }
    }
    let raw = fs::read_to_string(&token_path)
        .map_err(|_| BootError::ApprovalUntrusted("approval token is unreadable"))?;
    let approval = parse_scoped_approval(&raw)?;
    if approval.verb != APPROVAL_VERB {
        return Err(BootError::ApprovalUntrusted(
            "approval verb must be 'activate'",
        ));
    }
    if !approval.matches_release(running_release) {
        return Err(BootError::ApprovalStaleVersion {
            found: approval.release.clone(),
            running: running_release.to_string(),
        });
    }
    Ok(approval)
}

/// Parse the durable WSL2 acknowledgment wire format:
/// `accept-wsl2-nbd:<release>`.
///
/// The acknowledgment is the install-time consent that replaces the per-boot
/// `--accept-wsl2-nbd` flag (DT-13). It is deliberately a separate token from
/// the activation approval: the operator consents to the SIGBUS risk once per
/// release, not to a posture.
pub(crate) fn parse_wsl2_acceptance(raw: &str, running_release: &str) -> Result<(), BootError> {
    let raw = raw.trim();
    let Some(release) = raw.strip_prefix(&format!("{WSL2_ACCEPT_VERB}:")) else {
        return Err(BootError::Wsl2Acceptance(
            "acknowledgment verb must be 'accept-wsl2-nbd'",
        ));
    };
    if release.is_empty() || release.contains(':') {
        return Err(BootError::Wsl2Acceptance(
            "acknowledgment release is empty or ambiguous",
        ));
    }
    if release != running_release {
        return Err(BootError::Wsl2Acceptance(
            "acknowledgment names a different release",
        ));
    }
    Ok(())
}

/// Read the durable WSL2 acknowledgment for `release`.
///
/// Absent means the explicit per-boot gate stays in force. A present but
/// untrusted or mismatched document is a refusal, never a silent ignore: an
/// acknowledgment that cannot be attributed is not consent.
fn read_wsl2_acceptance(approval_dir: &Path, running_release: &str) -> Result<bool, BootError> {
    let path = approval_dir.join(format!("{WSL2_ACCEPT_FILE_PREFIX}-{running_release}.token"));
    let meta = match fs::metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => {
            return Err(BootError::Wsl2Acceptance(
                "acknowledgment token is unreadable",
            ));
        }
    };
    if !approval_token_is_root_owned(&path, &meta) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let reason = approval_token_trust(meta.uid(), meta.gid(), meta.mode())
                .err()
                .unwrap_or("acknowledgment token failed trust check");
            return Err(BootError::Wsl2Acceptance(reason));
        }
        #[cfg(not(unix))]
        {
            return Err(BootError::Wsl2Acceptance(
                "acknowledgment trust requires unix",
            ));
        }
    }
    let raw = fs::read_to_string(&path)
        .map_err(|_| BootError::Wsl2Acceptance("acknowledgment token is unreadable"))?;
    parse_wsl2_acceptance(&raw, running_release)?;
    Ok(true)
}

/// Host operations `boot` cannot unit-test against the live machine.
///
/// Same seam pattern as `ramshared-winsvc::control_plane::CommandRunner`: the
/// production implementation touches the real host; tests inject a fake so the
/// gate order and the no-mutation-on-refusal property are executable.
pub trait BootHost {
    /// Gate 1 (identity, RF-2/RF-8): `evaluate_product` over a fresh
    /// observation of the sealed release, BINARY_MATCH, and lifecycle.
    fn identity_gate(&self, config: &BootConfig, running_release: &str) -> Result<(), BootError>;
    /// Gate 2 (approval, RF-9, DT-4): token trust + version equality.
    fn resolve_approval(
        &self,
        approval_dir: &Path,
        running_release: &str,
    ) -> Result<ScopedApproval, BootError>;
    /// Gate 4 (dirty state, NFR-1): ghost / half cascade refusal.
    fn dirty_gate(&self) -> Result<(), BootError>;
    /// RF-5: `Some(true)` when a healthy cascade is already mounted
    /// (idempotent no-op). `None` when the swap table was unreadable — a
    /// probe that never took, never a measured `false`. Only `Some(true)`
    /// short-circuits into the no-op; `None` and `Some(false)` both fall
    /// through to the full gate path.
    fn already_healthy(&self) -> Option<bool>;
    /// The single mutation: the existing idempotent `up` (RF-5).
    ///
    /// `guard` is the customer-mutation custody the caller already holds
    /// (ITEM-6). This method **borrows** it; it never acquires the non-reentrant
    /// lock again.
    fn activate(&self, guard: &CustomerTransitionGuard, args: &[String]) -> Result<(), String>;
}

mod probes;
pub(crate) use probes::observe_lower_tier_capacity;
pub use probes::{SystemBootHost, release_version};
#[cfg(test)]
use probes::{
    conf_value, hex_digest, observe_cli_binary_match, observe_daemon_binary_match,
    observe_legacy_ublk, observe_lifecycle, observe_relay_gate, observe_release_gate,
    sealed_digest, sealed_release_identity, sha256_path, statvfs_free_bytes,
};

/// Gate 0 (DT-7, RF-8): `boot` has no deploy API. Checked on the caller's argv
/// before any gate reads the filesystem.
fn gate_no_deploy_api(args: &[String]) -> Result<(), BootError> {
    parse_boot_invocation(args)
}

/// Gate order (PRD §3 steps 1–4): identity → approval (incl. token trust and
/// posture prerequisites) → host prerequisites (incl. lease freshness) → dirty
/// state. No mutation, no retry loop (NFR-5).
///
/// The decision carries both the resolved sizing and the approval that passed,
/// because the posture bound to that approval decides the activation arguments
/// and must not be re-read (or re-defaulted) after the gates.
#[derive(Debug)]
struct BootDecision {
    config: BootConfig,
    approval: ScopedApproval,
}

fn evaluate_boot_gates(
    host: &dyn BootHost,
    paths: &BootPaths,
    env: &dyn Env,
    running_release: &str,
    args: &[String],
    boot_id: &str,
) -> Result<BootDecision, BootError> {
    gate_no_deploy_api(args)?;
    // Sizing is an input to the identity/capacity gate (DT-3), not a gate: it
    // is resolved before `evaluate_product` can validate lower-tier capacity,
    // and a refusal below never reports a config it would not have used.
    let config = load_boot_config_from(&paths.config, env)?;
    host.identity_gate(&config, running_release)?;
    // Approval before host prerequisites: a missing token is the operator's
    // first actionable refusal, not a lease error (RF-9). The posture comes
    // from that same token (DT-1): it is validated against the sizing and the
    // tier before anything is prepared, and an absent/unknown binding refuses.
    let approval = host.resolve_approval(&paths.approval_dir, running_release)?;
    posture::validate(approval.posture, config.zram_mib, Some(super::NBD))
        .map_err(BootError::Posture)?;
    verify_host_lease(&paths.lease, boot_id)?;
    host.dirty_gate()?;
    Ok(BootDecision { config, approval })
}

/// The activation argv `boot` hands to the lifecycle owner.
///
/// The posture comes from the version-scoped approval; the optional WSL2
/// acknowledgment comes from the installer's durable token (DT-13). Keeping
/// this pure lets both the boot path and its tests see one ordering.
fn activation_argv(config: &BootConfig, posture: Posture, accept_wsl2: bool) -> Vec<String> {
    let mut args = vec![
        "--vram".to_string(),
        config.vram_mib.to_string(),
        "--zram".to_string(),
        config.zram_mib.to_string(),
    ];
    args.extend(
        posture::activation_args(posture)
            .iter()
            .map(|arg| (*arg).to_string()),
    );
    if accept_wsl2 {
        args.push("--accept-wsl2-nbd".to_string());
    }
    args
}

/// Native bootstrap entrypoint (RF-7). Gate-then-act, no retry loop.
///
/// Custody is taken **first** (ITEM-6), before the already-healthy short
/// circuit: a durable `PackagePending` record must refuse `boot` even when the
/// cascade is up, because the operator is being told a bootstrap is being
/// attempted and that is not true while a package transaction is in flight.
pub fn boot() -> Result<BootConfig, BootError> {
    let guard = super::mutation_guard::enter_boot()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    boot_with(
        &SystemBootHost::default(),
        &BootPaths::production(),
        &ProcessEnv,
        &release_version(),
        &args,
        &guard,
    )
}

/// Testable orchestrator over injected host, paths, environment, argv and
/// customer-mutation custody. Returns the resolved sizing so the caller can
/// surface what was activated.
///
/// `guard` is the custody [`boot`] acquired. This function never acquires the
/// non-reentrant lock again and never mutates without it: it validates that the
/// caller brought a [`OperationOwner::Boot`] guard and hands it to the single
/// mutation.
pub fn boot_with(
    host: &dyn BootHost,
    paths: &BootPaths,
    env: &dyn Env,
    running_release: &str,
    args: &[String],
    guard: &CustomerTransitionGuard,
) -> Result<BootConfig, BootError> {
    if guard.owner() != OperationOwner::Boot {
        return Err(BootError::Custody {
            reason: "unsupported_surface",
            detail: format!("boot requires boot custody, got {}", guard.owner().as_str()),
        });
    }
    if host.already_healthy() == Some(true) {
        // RF-5 / a second `boot` on a healthy cascade is a no-op.
        // Sizing is reported best-effort; a missing config must not turn a
        // proven-healthy cascade into a failure. `None` is not a proven-healthy
        // cascade — the probe never took — so it falls through to the gates.
        return Ok(load_boot_config_from(&paths.config, env).unwrap_or_default());
    }
    // The lease is bound to this boot, so `boot_with` resolves the boot id the
    // same way production does. Tests that need a different binding call
    // `evaluate_boot_gates` directly.
    let boot_id = crate::workload::current_boot_id()
        .map_err(|_| BootError::HostPrerequisite("current boot id is unreadable"))?;
    let decision = evaluate_boot_gates(host, paths, env, running_release, args, &boot_id)?;
    // DT-13: the installer's durable WSL2 acknowledgment is the install-time
    // consent for the SIGBUS risk. It is bound to this release; a stale or
    // untrusted document refuses. Absence leaves the per-boot gate in force.
    let accept_wsl2 = read_wsl2_acceptance(&paths.approval_dir, running_release)?;
    // DT-3: sizes come from the resolved config, never from the sealed example.
    // DT-1/DT-2: the posture the token bound to this release decides the
    // activation shape; it is never inferred from the absence of a flag.
    let up_args = activation_argv(&decision.config, decision.approval.posture, accept_wsl2);
    host.activate(guard, &up_args).map_err(|error| {
        // The dynamic detail goes to the journal; the typed refusal stays
        // a stable static reason so `status` can surface it (NFR-6).
        eprintln!("[boot] activation failed after all gates passed: {error}");
        BootError::DirtyState("activation failed after all gates passed")
    })?;
    Ok(decision.config)
}

#[cfg(test)]
mod tests;
