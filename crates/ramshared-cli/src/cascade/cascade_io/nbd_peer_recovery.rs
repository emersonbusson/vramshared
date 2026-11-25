// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/nbd_peer_recovery.rs
//! Bounded replacement of a dead NBD peer while the kernel tier stays active.
//!
//! Separated from `nbd_netlink.rs` (Generic Netlink wire framing) so protocol
//! mechanics and custody-bound lifecycle recovery stay independent and concise.

use super::nbd_netlink::{
    DEFAULT_DEAD_CONN_TIMEOUT_SECS, KernelNbdNetlink, NbdNetlinkTransport, NbdPeerAttachOptions,
    query_device_status_with, replace_peer_socket_with,
};
#[allow(unused_imports)]
use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cascade) struct PeerRecoveryParams {
    pub(in crate::cascade) daemon_path: String,
    pub(in crate::cascade) vram_mb: u64,
    pub(in crate::cascade) cache_cap_mib: u64,
    pub(in crate::cascade) origin_path: String,
    pub(in crate::cascade) accept_wsl2_nbd: bool,
    pub(in crate::cascade) spill_pump_tier: Option<String>,
}

pub(in crate::cascade) fn read_nbd_block_size_bytes(
    sys_block: &Path,
    nbd_path: &str,
) -> Result<u64, CascadeError> {
    let name = Path::new(nbd_path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CascadeError::Precondition(format!("invalid NBD path: {nbd_path}")))?;
    let sectors_text = fs::read_to_string(sys_block.join(name).join("size")).map_err(|error| {
        CascadeError::Precondition(format!("read sysfs size for {nbd_path}: {error}"))
    })?;
    let sectors: u64 = sectors_text.trim().parse().map_err(|_| {
        CascadeError::Precondition(format!("invalid sysfs size for {nbd_path}: {sectors_text}"))
    })?;
    sectors
        .checked_mul(512)
        .filter(|bytes| *bytes >= 4096)
        .ok_or_else(|| CascadeError::Precondition(format!("zero NBD block size for {nbd_path}")))
}

pub(super) fn zram_backing_for_nbd(
    sys_block: &Path,
    binding: &LifecycleBinding,
    nbd_path: &str,
) -> Option<String> {
    let zram = binding
        .devices
        .iter()
        .find(|device| device.kind == ManagedDeviceKind::Zram)?;
    let name = Path::new(&zram.path).file_name()?.to_str()?;
    let backing = fs::read_to_string(sys_block.join(name).join("backing_dev")).ok()?;
    (backing.trim() == nbd_path).then(|| zram.path.clone())
}

fn validate_dead_peer_recovery(
    paths: &RuntimePaths,
    binding: &LifecycleBinding,
    swaps: &[SwapEntry],
    live_devices: &[BoundDeviceIdentity],
) -> Result<BoundDeviceIdentity, CascadeError> {
    authorize_bound_environment_for(binding, paths, swaps, live_devices, PeerState::ReplaceDead)?;
    let nbd = binding
        .devices
        .iter()
        .find(|device| device.kind == ManagedDeviceKind::Nbd)
        .cloned()
        .ok_or_else(|| CascadeError::Precondition("binding omitted NBD device".into()))?;
    if !swaps.iter().any(|entry| entry.canonical_path() == nbd.path)
        && zram_backing_for_nbd(&paths.sys_block, binding, &nbd.path).is_none()
    {
        return Err(CascadeError::Precondition(format!(
            "origin tier {} is already released from swap; run `ramshared down --recover-released-peer`",
            nbd.path
        )));
    }
    if paths.socket.exists() && socket_identity(&paths.socket)? != binding.export_socket {
        return Err(CascadeError::Precondition(
            "existing export socket identity differs from the sealed lifecycle binding".into(),
        ));
    }
    Ok(nbd)
}

fn update_binding_for_replacement_peer(
    paths: &RuntimePaths,
    binding: &mut LifecycleBinding,
    new_pid: u32,
) -> Result<(), CascadeError> {
    let new_instance = daemon_instance_id_from_pid(new_pid).ok_or_else(|| {
        CascadeError::Precondition("replacement daemon PID/start identity unavailable".into())
    })?;
    let new_start_ticks = new_instance
        .rsplit_once('-')
        .and_then(|(_, ticks)| ticks.parse::<u64>().ok())
        .filter(|ticks| *ticks > 0)
        .ok_or_else(|| {
            CascadeError::Precondition("replacement daemon start ticks invalid".into())
        })?;
    binding.daemon_pid = new_pid;
    binding.daemon_start_ticks = new_start_ticks;
    binding.daemon_instance_id = new_instance;
    binding.daemon_invocation_id = daemon_invocation_id(new_pid)?;
    binding.export_socket = socket_identity(&paths.socket)?;
    write_lifecycle_binding(paths, binding)
}

pub(in crate::cascade::cascade_io) fn recover_dead_nbd_peer_with<T, F>(
    paths: &RuntimePaths,
    params: &PeerRecoveryParams,
    swaps: &[SwapEntry],
    live_devices: &[BoundDeviceIdentity],
    transport: &mut T,
    spawn: F,
) -> Result<u32, CascadeError>
where
    T: NbdNetlinkTransport,
    F: FnOnce(
        &RuntimePaths,
        &PeerRecoveryParams,
        &str,
    ) -> Result<std::process::Child, CascadeError>,
{
    let mut binding = read_lifecycle_binding(paths)?;
    let nbd = validate_dead_peer_recovery(paths, &binding, swaps, live_devices)?;
    let expected_size_bytes = read_nbd_block_size_bytes(&paths.sys_block, &nbd.path)?;
    let status = query_device_status_with(transport, 2, &nbd.path)?;
    if status.connected {
        eprintln!(
            "[recover --nbd-peer] kernel still reports {} connected; proceeding after ESRCH peer proof",
            nbd.path
        );
    }
    let mut child = spawn(paths, params, &nbd.path)?;
    let replace_result = replace_peer_socket_with(
        transport,
        3,
        &nbd.path,
        &NbdPeerAttachOptions {
            socket_path: &paths.socket,
            export_name: PRODUCTION_EXPORT_NAME,
            expected_size_bytes: Some(expected_size_bytes),
            dead_conn_timeout_secs: Some(DEFAULT_DEAD_CONN_TIMEOUT_SECS),
            backend_identifier: None,
        },
    );
    if let Err(error) = replace_result {
        setup::stop_daemon_bounded(&mut child);
        return Err(error);
    }
    let new_pid = child.id();
    if let Err(error) = update_binding_for_replacement_peer(paths, &mut binding, new_pid) {
        setup::stop_daemon_bounded(&mut child);
        return Err(error);
    }
    eprintln!(
        "[recover --nbd-peer] attached replacement ramsharedd pid={new_pid} onto {} ({} B)",
        nbd.path, expected_size_bytes
    );
    Ok(new_pid)
}

pub(in crate::cascade::cascade_io) fn recover_dead_nbd_peer_at(
    paths: &RuntimePaths,
    params: &PeerRecoveryParams,
) -> Result<(), CascadeError> {
    let swaps = read_swaps()?;
    let live_devices = detect_live_managed_devices()?;
    let mut transport = KernelNbdNetlink::open()?;
    let _pid = recover_dead_nbd_peer_with(
        paths,
        params,
        &swaps,
        &live_devices,
        &mut transport,
        |paths, params, nbd_path| {
            setup::spawn_daemon_with_deadline(
                &params.daemon_path,
                params.vram_mb,
                params.cache_cap_mib,
                nbd_path,
                &params.origin_path,
                paths,
                ORIGIN_DAEMON_READINESS_TIMEOUT,
                params.accept_wsl2_nbd,
                params.spill_pump_tier.as_deref(),
            )
        },
    )?;
    Ok(())
}

pub(super) fn recover_nbd_peer_from(
    paths: &RuntimePaths,
    load_up: impl FnOnce() -> Result<UpArgs, CascadeError>,
) -> Result<(), CascadeError> {
    let binding = read_lifecycle_binding(paths)?;
    let nbd = binding
        .devices
        .iter()
        .find(|device| device.kind == ManagedDeviceKind::Nbd)
        .ok_or_else(|| CascadeError::Precondition("binding omitted NBD device".into()))?;
    let up = load_up()?;
    let spill_pump_tier = zram_backing_for_nbd(&paths.sys_block, &binding, &nbd.path);
    recover_dead_nbd_peer_at(
        paths,
        &PeerRecoveryParams {
            daemon_path: up.daemon,
            vram_mb: up.vram_mb,
            cache_cap_mib: up.cache_cap_mib,
            origin_path: up.origin_path,
            accept_wsl2_nbd: up.accept_wsl2_nbd,
            spill_pump_tier,
        },
    )
}

fn recover_up_args(wsl2: bool) -> Vec<String> {
    if wsl2 {
        vec![
            "--transport".into(),
            "nbd".into(),
            "--accept-wsl2-nbd".into(),
        ]
    } else {
        vec!["--transport".into(), "nbd".into()]
    }
}

pub fn recover_nbd_peer() -> Result<(), CascadeError> {
    let _guard =
        super::super::mutation_guard::enter(crate::customer::transition::OperationOwner::Up)?;
    let paths = RuntimePaths::system();
    recover_nbd_peer_from(&paths, || {
        let args = recover_up_args(up_config::is_wsl2());
        crate::cascade::up_config::parse_up_args_from(&args, default_daemon())
    })
}

#[cfg(test)]
#[path = "nbd_peer_recovery/tests.rs"]
mod tests;
