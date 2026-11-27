// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/pooled_cache/server.rs
//! Live Windows `AF_HYPERV` RSCP v1 pooled cache server and dedicated GPU scheduler loop.
//! SPEC: the protocol specification §DT-3, §DT-4, §DT-7, §DT-8, §DT-10.

pub mod config;
pub mod hyperv;

pub use config::{
    DEFAULT_POOLED_CONFIG_PATH, DEFAULT_POOLED_STATUS_PATH, MAX_POOLED_CONFIG_BYTES,
    MAX_POOLED_SLOTS, PooledServerConfig, PooledStatusSnapshot, RoleEndpointConfig,
    RoleStatusCounters, parse_key_bytes, write_status_atomic,
};
pub use hyperv::{run_hyperv_server, spawn_if_configured};

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ramshared_ipc::cache_wire::{
    CACHE_PAGE_BYTES, CacheClientHello, CacheFrameBuffer, CacheFrameHeader, CacheGuestRole,
    CacheHostChallenge, CacheOpcode, CacheScope, CacheWireError, read_client_finish,
    read_client_hello, read_into, write_frame_with, write_host_challenge,
};
use ramshared_ipc::vsock::{VsockPeerIdentity, VsockStreamTrait, linux_service_guid};
use ramshared_ipc::{FrameError, random_nonce};

use super::allocation::{
    AdapterObservation, GuestCustodyState, GuestPlacementIntent, PoolAdmission, RolePlacement,
};
use super::worker::{GpuWorkerBackend, OwnedGpuWorker};
use super::{PoolSupervisor, ScheduledReply, SupervisorError};

const OBSERVE_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const SCHEDULER_IDLE_TICK: Duration = Duration::from_millis(50);
const STATUS_WRITE_INTERVAL: Duration = Duration::from_millis(500);
const HANDSHAKE_IO_TIMEOUT: Duration = Duration::from_secs(5);
const DATA_IDLE_READ_TIMEOUT: Duration = Duration::from_millis(500);
const DATA_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const CMD_QUEUE_CAP: usize = 32;

struct PendingAuth {
    role: CacheGuestRole,
    peer: VsockPeerIdentity,
    guest_nonce: [u8; 32],
    host_nonce: [u8; 32],
}

enum SchedulerCmd {
    AuthBegin {
        hello: CacheClientHello,
        peer: VsockPeerIdentity,
        reply: SyncSender<Result<(CacheHostChallenge, PendingAuth), String>>,
    },
    AuthFinish {
        pending: PendingAuth,
        guest_proof: [u8; 32],
        finish_proof: [u8; 32],
        reply: SyncSender<Result<CacheScope, String>>,
    },
    Lookup {
        role: CacheGuestRole,
        header: CacheFrameHeader,
        reply: SyncSender<Result<ScheduledReply, SupervisorError>>,
    },
    Offer {
        role: CacheGuestRole,
        header: CacheFrameHeader,
        page: Box<[u8; CACHE_PAGE_BYTES]>,
        crc: u32,
        reply: SyncSender<Result<ScheduledReply, SupervisorError>>,
    },
    Retire {
        role: CacheGuestRole,
    },
    Snapshot {
        reply: SyncSender<PooledStatusSnapshot>,
    },
    Shutdown,
}

/// Thread-safe handle to the single owning GPU scheduler thread.
#[derive(Clone)]
pub struct SchedulerHandle {
    tx: SyncSender<SchedulerCmd>,
}

impl SchedulerHandle {
    pub fn snapshot(&self) -> Result<PooledStatusSnapshot, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .send(SchedulerCmd::Snapshot { reply: tx })
            .map_err(|_| "pooled scheduler stopped".to_string())?;
        rx.recv()
            .map_err(|_| "pooled scheduler snapshot closed".to_string())
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(SchedulerCmd::Shutdown);
    }
}

/// Opaque receiver end owned by the single GPU scheduler thread.
pub struct SchedulerReceiver(Receiver<SchedulerCmd>);

/// Static parameters for [`run_scheduler_loop`].
pub struct SchedulerLoopConfig<'a> {
    pub adapter_id: &'a str,
    pub slot_capacity: usize,
    pub wsl_auto_vm: bool,
    pub status_path: Option<&'a Path>,
}

pub fn create_scheduler_channel() -> (SchedulerHandle, SchedulerReceiver) {
    let (tx, rx) = mpsc::sync_channel(CMD_QUEUE_CAP);
    (SchedulerHandle { tx }, SchedulerReceiver(rx))
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Drive the owning [`PoolSupervisor`] on the current (CUDA-context-owning) thread.
pub fn run_scheduler_loop<B: GpuWorkerBackend>(
    mut sup: PoolSupervisor<B>,
    cfg: SchedulerLoopConfig<'_>,
    rx: SchedulerReceiver,
    mut observe: impl FnMut(&B, Instant) -> Option<AdapterObservation>,
    stop: &AtomicBool,
) {
    let rx = rx.0;
    let SchedulerLoopConfig {
        adapter_id,
        slot_capacity,
        wsl_auto_vm,
        status_path,
    } = cfg;
    let mut pending: HashMap<
        (CacheGuestRole, u64),
        SyncSender<Result<ScheduledReply, SupervisorError>>,
    > = HashMap::with_capacity(8);
    let mut last_obs: Option<AdapterObservation> = None;
    let mut last_obs_at = Instant::now()
        .checked_sub(OBSERVE_REFRESH_INTERVAL * 2)
        .unwrap_or_else(Instant::now);
    let mut last_status_at = Instant::now()
        .checked_sub(STATUS_WRITE_INTERVAL * 2)
        .unwrap_or_else(Instant::now);

    let mut refresh_obs = |sup: &PoolSupervisor<B>,
                           now: Instant,
                           last_obs: &mut Option<AdapterObservation>,
                           last_obs_at: &mut Instant| {
        if (last_obs.is_none()
            || now.saturating_duration_since(*last_obs_at) >= OBSERVE_REFRESH_INTERVAL)
            && let Some(fresh) = observe(sup.worker().backend(), now)
        {
            *last_obs = Some(fresh);
            *last_obs_at = now;
        }
    };

    let make_snapshot =
        |sup: &PoolSupervisor<B>, obs: &Option<AdapterObservation>| PooledStatusSnapshot {
            schema_version: 1,
            written_at_unix_ms: unix_ms_now(),
            adapter_id: adapter_id.to_string(),
            slot_capacity,
            slab_bytes: (slot_capacity as u64).saturating_mul(CACHE_PAGE_BYTES as u64),
            placement_permit_held: sup.admission().placement_permit_held(),
            worker_quarantined: sup.worker().is_quarantined(),
            wsl_active_epoch: sup.active_epoch(CacheGuestRole::Wsl2),
            secondary_active_epoch: sup.active_epoch(CacheGuestRole::SecondaryGuest),
            wsl_charged_bytes: sup.admission().charged_for(CacheGuestRole::Wsl2),
            secondary_charged_bytes: sup.admission().charged_for(CacheGuestRole::SecondaryGuest),
            in_flight_bytes: sup.admission().in_flight_bytes(),
            quarantined_bytes: sup.admission().quarantined_bytes(),
            host_physical_ram_bytes: obs.as_ref().map_or(0, |o| o.host_physical_ram_bytes),
            host_available_ram_bytes: obs.as_ref().map_or(0, |o| o.host_available_ram_bytes),
            cuda_free_bytes: obs.as_ref().map_or(0, |o| o.cuda_free_bytes),
            wsl: sup.counters(CacheGuestRole::Wsl2).into(),
            secondary: sup.counters(CacheGuestRole::SecondaryGuest).into(),
        };

    refresh_obs(&sup, Instant::now(), &mut last_obs, &mut last_obs_at);
    if let Some(path) = status_path {
        let _ = write_status_atomic(path, &make_snapshot(&sup, &last_obs));
        last_status_at = Instant::now();
    }

    let mut running = true;
    while running && !stop.load(Ordering::Acquire) {
        let first = match rx.recv_timeout(SCHEDULER_IDLE_TICK) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let now = Instant::now();
        refresh_obs(&sup, now, &mut last_obs, &mut last_obs_at);

        let mut handle_cmd = |cmd: SchedulerCmd| -> bool {
            match cmd {
                SchedulerCmd::AuthBegin { hello, peer, reply } => {
                    let res = begin_auth(&mut sup, &hello, peer, wsl_auto_vm);
                    let _ = reply.send(res);
                }
                SchedulerCmd::AuthFinish {
                    pending: pa,
                    guest_proof,
                    finish_proof,
                    reply,
                } => {
                    let res = sup
                        .authenticate_session(
                            pa.role,
                            Some(pa.peer),
                            pa.guest_nonce,
                            pa.host_nonce,
                            &guest_proof,
                            &finish_proof,
                        )
                        .map_err(|e| format!("{e:?}"));
                    let _ = reply.send(res);
                }
                SchedulerCmd::Lookup {
                    role,
                    header,
                    reply,
                } => match sup.enqueue_lookup(role, header, now) {
                    Ok(()) => {
                        pending.insert((role, header.correlation), reply);
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err));
                    }
                },
                SchedulerCmd::Offer {
                    role,
                    header,
                    page,
                    crc,
                    reply,
                } => match sup.enqueue_offer(role, header, page, crc, now) {
                    Ok(()) => {
                        pending.insert((role, header.correlation), reply);
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err));
                    }
                },
                SchedulerCmd::Retire { role } => {
                    sup.retire_session(role, now);
                    pending.retain(|&(r, _), _| r != role);
                }
                SchedulerCmd::Snapshot { reply } => {
                    let _ = reply.send(make_snapshot(&sup, &last_obs));
                }
                SchedulerCmd::Shutdown => {
                    return false;
                }
            }
            true
        };

        if let Some(cmd) = first {
            running = handle_cmd(cmd);
            while running {
                match rx.try_recv() {
                    Ok(extra) => running = handle_cmd(extra),
                    Err(_) => break,
                }
            }
        }

        if let Some(obs) = last_obs.as_ref() {
            while let Some(reply) = sup.step(obs, Instant::now()) {
                if let Some(waiter) = pending.remove(&(reply.role, reply.header.correlation)) {
                    let _ = waiter.send(Ok(reply));
                }
            }
        }
        let _ = sup.poll_quarantine_reap(Instant::now());
        if let Some(path) = status_path
            && Instant::now().saturating_duration_since(last_status_at) >= STATUS_WRITE_INTERVAL
        {
            let _ = write_status_atomic(path, &make_snapshot(&sup, &last_obs));
            last_status_at = Instant::now();
        }
    }

    let now = Instant::now();
    sup.retire_session(CacheGuestRole::Wsl2, now);
    sup.retire_session(CacheGuestRole::SecondaryGuest, now);
    if let Some(path) = status_path {
        let _ = write_status_atomic(path, &make_snapshot(&sup, &last_obs));
    }
}

fn begin_auth<B: GpuWorkerBackend>(
    sup: &mut PoolSupervisor<B>,
    hello: &CacheClientHello,
    peer: VsockPeerIdentity,
    wsl_auto_vm: bool,
) -> Result<(CacheHostChallenge, PendingAuth), String> {
    if hello.role == CacheGuestRole::Wsl2 && wsl_auto_vm {
        sup.bind_dynamic_wsl_runtime_id(peer.vm_id)
            .map_err(|e| format!("dynamic wsl bind refused: {e:?}"))?;
    }
    let cfg = sup
        .enrolled(hello.role)
        .cloned()
        .ok_or_else(|| "role is not enrolled".to_string())?;
    if sup.active_epoch(hello.role).is_some() {
        return Err("duplicate active session for role".into());
    }
    let expected_guid = linux_service_guid(cfg.data_port);
    if peer.vm_id != cfg.expected_runtime_id || peer.service_id != expected_guid {
        return Err("native peer VM/service route mismatch".into());
    }
    if hello.extent_bytes != cfg.extent_bytes {
        return Err("guest extent_bytes differs from enrolled extent".into());
    }
    if hello.origin_fingerprint != cfg.expected_origin_fingerprint {
        return Err("guest origin_fingerprint differs from enrolled origin".into());
    }
    let host_nonce = random_nonce().map_err(|e| e.to_string())?;
    let epoch = sup.next_epoch(hello.role);
    let scope = CacheScope {
        role: hello.role,
        runtime_id: peer.vm_id,
        service_guid: peer.service_id,
        origin_fingerprint: cfg.expected_origin_fingerprint,
        epoch,
        extent_bytes: cfg.extent_bytes,
        guest_nonce: hello.guest_nonce,
        host_nonce,
    };
    let host_proof = scope.host_proof(&cfg.key).map_err(|e| e.to_string())?;
    let challenge = CacheHostChallenge {
        accepted: true,
        epoch,
        runtime_id: peer.vm_id,
        host_nonce,
        host_proof,
    };
    Ok((
        challenge,
        PendingAuth {
            role: hello.role,
            peer,
            guest_nonce: hello.guest_nonce,
            host_nonce,
        },
    ))
}

struct SessionRetireGuard<'a> {
    handle: &'a SchedulerHandle,
    role: Option<CacheGuestRole>,
}

impl Drop for SessionRetireGuard<'_> {
    fn drop(&mut self) {
        if let Some(role) = self.role.take() {
            let _ = self.handle.tx.send(SchedulerCmd::Retire { role });
        }
    }
}

/// Authenticate and serve one guest stream over RSCP v1 until disconnect, refusal, or `stop`.
///
/// `override_peer` is `None` in production (always queries kernel `stream.native_peer()`)
/// and may be supplied only in deterministic non-Hyper-V stream tests.
pub fn serve_session_stream<S: VsockStreamTrait>(
    stream: &mut S,
    handle: &SchedulerHandle,
    data_port: u32,
    stop: &AtomicBool,
    override_peer: Option<VsockPeerIdentity>,
) -> Result<(), String> {
    let _ = stream.set_read_timeout(Some(HANDSHAKE_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HANDSHAKE_IO_TIMEOUT));
    let observed = match override_peer {
        Some(p) => p,
        None => {
            let native = stream
                .native_peer()
                .map_err(|e| format!("native_peer query failed: {e}"))?
                .ok_or_else(|| "missing native AF_HYPERV peer identity".to_string())?;
            // Accepted AF_HYPERV sockets carry the guest's ephemeral port in peer.service_id;
            // bind service_id to our kernel-bound listening service GUID for `data_port`.
            VsockPeerIdentity {
                vm_id: native.vm_id,
                service_id: linux_service_guid(data_port),
            }
        }
    };
    let hello = read_client_hello(stream).map_err(|e| format!("client hello failed: {e}"))?;
    let (begin_tx, begin_rx) = mpsc::sync_channel(1);
    handle
        .tx
        .send(SchedulerCmd::AuthBegin {
            hello,
            peer: observed,
            reply: begin_tx,
        })
        .map_err(|_| "scheduler unavailable".to_string())?;
    let (challenge, pending) = match begin_rx
        .recv()
        .map_err(|_| "scheduler auth closed".to_string())?
    {
        Ok(ok) => ok,
        Err(reason) => {
            let _ = write_host_challenge(stream, &CacheHostChallenge::rejected());
            return Err(reason);
        }
    };
    write_host_challenge(stream, &challenge).map_err(|e| format!("write challenge failed: {e}"))?;
    let finish = read_client_finish(stream).map_err(|e| format!("read finish failed: {e}"))?;
    let role = pending.role;
    let (fin_tx, fin_rx) = mpsc::sync_channel(1);
    handle
        .tx
        .send(SchedulerCmd::AuthFinish {
            pending,
            guest_proof: finish.guest_proof,
            finish_proof: finish.finish_proof,
            reply: fin_tx,
        })
        .map_err(|_| "scheduler unavailable".to_string())?;
    let _scope = fin_rx
        .recv()
        .map_err(|_| "scheduler finish closed".to_string())??;

    let _guard = SessionRetireGuard {
        handle,
        role: Some(role),
    };
    let _ = stream.set_read_timeout(Some(DATA_IDLE_READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(DATA_WRITE_TIMEOUT));

    let mut rbuf = CacheFrameBuffer::new();
    let mut wbuf = CacheFrameBuffer::new();
    while !stop.load(Ordering::Acquire) {
        let frame = match read_into(stream, &mut rbuf) {
            Ok(f) => f,
            Err(CacheWireError::Frame(FrameError::StreamIo(msg)))
                if msg.contains("timed out")
                    || msg.contains("TimedOut")
                    || msg.contains("WouldBlock")
                    || msg.contains("10060") =>
            {
                continue;
            }
            Err(CacheWireError::Frame(FrameError::TruncatedFrame { got: 0, .. })) => break,
            Err(err) => return Err(format!("data lane read failed: {err}")),
        };
        let (rep_tx, rep_rx) = mpsc::sync_channel(1);
        let req_header = frame.header;
        let cmd = match (req_header.opcode, frame.page, frame.crc) {
            (CacheOpcode::Lookup, None, None) => SchedulerCmd::Lookup {
                role,
                header: req_header,
                reply: rep_tx,
            },
            (CacheOpcode::Offer, Some(page), Some(crc)) => SchedulerCmd::Offer {
                role,
                header: req_header,
                page: Box::new(*page),
                crc,
                reply: rep_tx,
            },
            _ => return Err("unexpected opcode or payload on data lane".into()),
        };
        handle
            .tx
            .send(cmd)
            .map_err(|_| "scheduler stopped".to_string())?;
        let reply = match rep_rx
            .recv()
            .map_err(|_| "scheduler reply closed".to_string())?
        {
            Ok(rep) => rep,
            Err(SupervisorError::QueueFull) => ScheduledReply {
                role,
                header: CacheFrameHeader {
                    opcode: CacheOpcode::Busy,
                    payload_len: 0,
                    ..req_header
                },
                page: None,
            },
            Err(err) => return Err(format!("supervisor refused frame: {err:?}")),
        };
        write_frame_with(stream, &mut wbuf, reply.header, reply.page.as_deref())
            .map_err(|e| format!("write reply failed: {e}"))?;
    }
    Ok(())
}

/// Build the initial authorized [`PoolSupervisor`] for a live pooled cache deployment.
pub fn build_live_supervisor<B: GpuWorkerBackend>(
    cfg: &PooledServerConfig,
    adapter_id: &str,
    backend: B,
) -> Result<PoolSupervisor<B>, String> {
    let roles = cfg.build_enrolled_roles()?;
    let mut admission =
        PoolAdmission::new(ramshared_vram::ReserveFloorPolicy::sealed(), adapter_id);
    if let Some(floor) = cfg.host_ram_reserve_bytes {
        admission = admission.with_host_ram_floor_bytes(floor);
    }
    let mut sup = PoolSupervisor::new(admission, OwnedGpuWorker::new(backend, cfg.slot_count));
    for r in roles {
        sup.enroll(r).map_err(|e| format!("enroll failed: {e:?}"))?;
    }
    let consented = RolePlacement {
        intent: GuestPlacementIntent::PooledSealed,
        custody: GuestCustodyState::RunningConsented { boot_gen: 1 },
    };
    let sec_placement = if cfg.secondary.is_some() {
        RolePlacement {
            intent: GuestPlacementIntent::PooledSealed,
            custody: GuestCustodyState::ControlledOff {
                sealed_boot_path: true,
            },
        }
    } else {
        consented
    };
    sup.update_placement(consented, sec_placement, false, Instant::now())
        .map_err(|e| format!("authorize placement failed: {e:?}"))?;
    Ok(sup)
}
