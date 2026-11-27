// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/pooled_cache/server/hyperv.rs
//! Windows `AF_HYPERV` listener, WSL2 host-to-guest connector, and SCM spawn adapter.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(windows)]
use std::sync::mpsc;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use ramshared_ipc::cache_wire::{CACHE_PAGE_BYTES, validate_cache_data_port};
#[cfg(windows)]
use ramshared_ipc::vsock::VsockStreamTrait;

#[cfg(windows)]
use super::config::DEFAULT_POOLED_STATUS_PATH;
use super::config::{DEFAULT_POOLED_CONFIG_PATH, PooledServerConfig};
#[cfg(windows)]
use super::{
    SchedulerHandle, SchedulerLoopConfig, build_live_supervisor, create_scheduler_channel,
    run_scheduler_loop, serve_session_stream,
};
#[cfg(windows)]
use crate::vm_id_resolve::parse_guid;

#[cfg(windows)]
const MAX_CONCURRENT_SESSIONS: usize = 8;

/// Run the native Windows `AF_HYPERV` pooled cache server with real CUDA VRAM slab until `stop`.
#[cfg(windows)]
pub fn run_hyperv_server(cfg: &PooledServerConfig, stop: Arc<AtomicBool>) -> Result<(), String> {
    use ramshared_ipc::vsock::{VsockError, listen_hyperv};

    cfg.validate()?;
    let guid = validate_cache_data_port(cfg.data_port, cfg.control_port)
        .map_err(|e| format!("invalid data port: {e}"))?;
    let listener = listen_hyperv(guid).map_err(|e| format!("listen_hyperv failed: {e}"))?;
    let (handle, rx) = create_scheduler_channel();
    let cfg_worker = cfg.clone();
    let stop_worker = Arc::clone(&stop);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<String, String>>(1);

    let worker_thread = std::thread::Builder::new()
        .name("ramshared-pooled-gpu".into())
        .spawn(move || {
            let res = super::super::cuda_backend::with_cuda_slot_worker(
                cfg_worker.cuda_ordinal,
                cfg_worker.slot_count,
                ramshared_vram::ReserveFloorPolicy::sealed(),
                |backend| {
                    let adapter_id = backend.adapter_id().to_string();
                    match build_live_supervisor(&cfg_worker, &adapter_id, backend) {
                        Ok(sup) => {
                            let _ = ready_tx.send(Ok(adapter_id.clone()));
                            let status_path = cfg_worker
                                .status_file
                                .as_deref()
                                .unwrap_or_else(|| Path::new(DEFAULT_POOLED_STATUS_PATH));
                            run_scheduler_loop(
                                sup,
                                SchedulerLoopConfig {
                                    adapter_id: &adapter_id,
                                    slot_capacity: cfg_worker.slot_count,
                                    wsl_auto_vm: cfg_worker.wsl_uses_auto_vm_id(),
                                    status_path: Some(status_path),
                                },
                                rx,
                                |cuda_backend, now| {
                                    let mem = crate::host_memory::sample()?;
                                    cuda_backend.observe_adapter(
                                        mem.total_bytes,
                                        mem.available_bytes,
                                        now,
                                    )
                                },
                                &stop_worker,
                            );
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                        }
                    }
                },
            );
            if let Err(err) = res {
                let _ = ready_tx.send(Err(err));
            }
        })
        .map_err(|e| format!("spawn pooled gpu thread failed: {e}"))?;

    let adapter_id = ready_rx
        .recv()
        .map_err(|_| "pooled gpu thread exited before ready".to_string())??;
    eprintln!(
        "[ramshared-pooled-cache] listening port={} adapter={} slots={} ({} MiB)",
        cfg.data_port,
        adapter_id,
        cfg.slot_count,
        ((cfg.slot_count as u64) * (CACHE_PAGE_BYTES as u64)) >> 20
    );

    let active_handlers = Arc::new(AtomicUsize::new(0));
    let mut joins = Vec::new();
    let wsl_connector = spawn_wsl_connector(cfg, guid, handle.clone(), Arc::clone(&stop));
    while !stop.load(Ordering::Acquire) {
        match listener.accept(Duration::from_millis(250)) {
            Ok(mut stream) => {
                if active_handlers.load(Ordering::Acquire) >= MAX_CONCURRENT_SESSIONS {
                    let _ = stream.shutdown_both();
                    continue;
                }
                let h = handle.clone();
                let s = Arc::clone(&stop);
                let active = Arc::clone(&active_handlers);
                let port = cfg.data_port;
                active.fetch_add(1, Ordering::AcqRel);
                if let Ok(t) = std::thread::Builder::new()
                    .name("ramshared-pooled-peer".into())
                    .spawn(move || {
                        if let Err(err) = serve_session_stream(&mut stream, &h, port, &s, None) {
                            eprintln!("[ramshared-pooled-cache] peer ended: {err}");
                        }
                        let _ = stream.shutdown_both();
                        active.fetch_sub(1, Ordering::AcqRel);
                    })
                {
                    joins.push(t);
                } else {
                    active_handlers.fetch_sub(1, Ordering::AcqRel);
                }
                joins.retain(|j| !j.is_finished());
            }
            Err(VsockError::AcceptTimeout) => continue,
            Err(err) => {
                eprintln!("[ramshared-pooled-cache] accept error: {err}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    handle.shutdown();
    if let Some(t) = wsl_connector {
        let _ = t.join();
    }
    for j in joins {
        let _ = j.join();
    }
    let _ = worker_thread.join();
    Ok(())
}

#[cfg(windows)]
fn spawn_wsl_connector(
    cfg: &PooledServerConfig,
    guid: [u8; 16],
    handle: SchedulerHandle,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    let wsl_cfg = cfg.wsl.clone()?;
    let sec_guid = cfg
        .secondary
        .as_ref()
        .and_then(|s| parse_guid(&s.vm_id).ok());
    let port = cfg.data_port;
    let explicit_wsl_vm = if wsl_cfg.vm_id.trim().eq_ignore_ascii_case("auto") {
        None
    } else {
        parse_guid(&wsl_cfg.vm_id).ok()
    };
    if explicit_wsl_vm.is_none()
        && let Ok(exe) = std::env::current_exe()
    {
        let _ = crate::vm_identity::sampler::start(exe);
    }
    std::thread::Builder::new()
        .name("ramshared-pooled-wsl".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if handle
                    .snapshot()
                    .is_ok_and(|s| s.wsl_active_epoch.is_some())
                {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                let candidates: Vec<[u8; 16]> = match explicit_wsl_vm {
                    Some(id) => vec![id],
                    None => crate::vm_id_resolve::resolve_connect_candidates()
                        .map(|list| {
                            list.into_iter()
                                .map(|c| c.runtime_id)
                                .filter(|id| Some(*id) != sec_guid)
                                .collect()
                        })
                        .unwrap_or_default(),
                };
                let mut connected = false;
                for vm_id in candidates {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Ok(mut stream) = ramshared_ipc::vsock::connect_hyperv(
                        vm_id,
                        guid,
                        Duration::from_millis(750),
                    ) {
                        connected = true;
                        if let Err(err) =
                            serve_session_stream(&mut stream, &handle, port, &stop, None)
                        {
                            eprintln!("[ramshared-pooled-cache] wsl peer ended: {err}");
                        }
                        let _ = stream.shutdown_both();
                        break;
                    }
                }
                if !connected {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        })
        .ok()
}

#[cfg(not(windows))]
pub fn run_hyperv_server(_cfg: &PooledServerConfig, _stop: Arc<AtomicBool>) -> Result<(), String> {
    Err("AF_HYPERV pooled cache server requires Windows".into())
}

/// Spawn the pooled cache server alongside `authority-service` when its config file exists.
pub fn spawn_if_configured(stop: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
    let path = Path::new(DEFAULT_POOLED_CONFIG_PATH);
    if !path.is_file() {
        return None;
    }
    let cfg = match PooledServerConfig::load(path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("[ramshared-pooled-cache] config refused, staying disabled: {err}");
            return None;
        }
    };
    std::thread::Builder::new()
        .name("ramshared-pooled-server".into())
        .spawn(move || {
            if let Err(err) = run_hyperv_server(&cfg, stop) {
                eprintln!("[ramshared-pooled-cache] server exited with error: {err}");
            }
        })
        .ok()
}
