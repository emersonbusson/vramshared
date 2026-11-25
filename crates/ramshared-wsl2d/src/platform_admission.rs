// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/platform_admission.rs
//! Platform admission before PID/device effects; shared observations retain unknown state.
use super::{AppArgs, BackendKind, ORIGIN_MANIFEST_PATH, Transport};
use ramshared_block::platform::{PlatformKind, probe_platform};

/// A validated daemon action selected before PID, driver or device effects.
pub(super) enum DaemonAction {
    Broker(AppArgs),
    Nbd(AppArgs),
    Ublk(AppArgs),
}

pub(super) fn select_daemon_action(
    args: AppArgs,
) -> Result<DaemonAction, Box<dyn std::error::Error>> {
    select_daemon_action_for(args, observe_wsl2())
}

pub(super) fn select_daemon_action_for(
    args: AppArgs,
    running_on_wsl2: Option<bool>,
) -> Result<DaemonAction, Box<dyn std::error::Error>> {
    if args.native_origin.is_some() && running_on_wsl2 != Some(false) {
        return Err("native origin serving requires a measured native Linux platform".into());
    }
    if args.slices > 0 && (args.origin.is_some() || args.native_origin.is_some()) {
        return Err("--origin-manifest is valid only for the single NBD product path".into());
    }
    if args.slices > 0 && args.arbiter_addr.is_none() {
        return Err("--slices requires --arbiter-listen IP:PORT (broker control point)".into());
    }
    if args.slices > 0 {
        if args.backend != BackendKind::Ram {
            return Err(
                "GPU-backed --slices is unsupported because its driver calls are synchronous; use the single NBD --origin-manifest path for isolated, revocable GPU caching".into(),
            );
        }
        return Ok(DaemonAction::Broker(args));
    }
    if args.arbiter_addr.is_some() || args.listen_nbd_addr.is_some() {
        return Err("--arbiter-listen/--listen-nbd require --slices N (N > 0)".into());
    }
    // Fail closed: `None` — a probe that never ran — takes the same hard
    // refusal as `Some(true)`. Only a measured absence (`Some(false)`) admits
    // the path, so silence cannot skip a refusal that exists after a confirmed
    // NBD timeout, swap read errors, and SIGBUS. A measured WSL2 host admits
    // it under the operator's explicit acknowledgment (`--accept-wsl2-nbd`,
    // verified test case qualification); the acknowledgment is never inferred.
    if running_on_wsl2 != Some(false) && args.transport == Transport::Nbd && args.origin.is_some() {
        if running_on_wsl2 == Some(true) && args.accept_wsl2_nbd {
            eprintln!("{}", wsl2_nbd_admission_notice());
        } else {
            if running_on_wsl2.is_none() {
                return Err("product NBD requires measured platform evidence; kernel or WSL runtime identity is unknown. Verify platform observations before retrying. --accept-wsl2-nbd cannot authorize an unknown platform; keep the configured WSL fallback swap active.".into());
            }
            return Err(wsl2_nbd_swap_refusal().into());
        }
    }
    match (args.transport, args.backend) {
        (Transport::Nbd, BackendKind::Ram) => Err(
            "--backend ram has no single NBD path; use --slices (broker) or ublk".into(),
        ),
        (Transport::Ublk, BackendKind::Vulkan) => Err(
            "ublk with --backend vulkan is not supported (DT-11); use --backend vram, or Vulkan via --slices / --transport nbd"
                .into(),
        ),
        (Transport::Nbd, _) if args.origin.is_some() || args.native_origin.is_some() => Ok(DaemonAction::Nbd(args)),
        (Transport::Nbd, _) => {
            Err(format!(
                "product NBD requires --origin-manifest {ORIGIN_MANIFEST_PATH}"
            )
            .into())
        }
        (Transport::Ublk, _) if args.origin.is_some() => {
            Err("--origin-manifest is valid only with --transport nbd".into())
        }
        (Transport::Ublk, _) => {
            ublk_platform_guard(running_on_wsl2)?;
            Ok(DaemonAction::Ublk(args))
        }
    }
}

fn wsl2_nbd_swap_refusal() -> &'static str {
    "transport NBD on WSL2 requires --accept-wsl2-nbd: isolated testing qualified peer liveness, bounded teardown, reconnect and restart, but a peer failure can still SIGBUS processes holding pages on the tier — keep the configured WSL fallback swap active. Pass --accept-wsl2-nbd to arm it."
}

fn wsl2_nbd_admission_notice() -> &'static str {
    "[ramsharedd] WSL2 NBD accepted by operator acknowledgment: WSL2 NBD qualification covers peer liveness, bounded teardown, reconnect and restart; a peer failure can still SIGBUS processes holding pages on the tier, so the fallback swap stays active below it."
}

fn observed_wsl(kind: Option<PlatformKind>) -> Option<bool> {
    match kind {
        Some(PlatformKind::NativeLinux) => Some(false),
        Some(PlatformKind::Wsl1 | PlatformKind::Wsl2) => Some(true),
        None => None,
    }
}

fn observe_wsl2() -> Option<bool> {
    observed_wsl(probe_platform().platform)
}

pub(super) fn ublk_platform_guard(wsl: Option<bool>) -> Result<(), String> {
    match wsl {
        Some(false) => Ok(()),
        Some(true) => Err("refused: standalone ublk on WSL can freeze the system after teardown failure; validate in an isolated native Linux VM".into()),
        None => Err("refused: standalone ublk requires a measured native Linux platform; platform evidence is unknown".into()),
    }
}

pub(super) fn guard_not_wsl2() -> Result<(), Box<dyn std::error::Error>> {
    ublk_platform_guard(observe_wsl2()).map_err(Into::into)
}

#[cfg(test)]
mod tests;
