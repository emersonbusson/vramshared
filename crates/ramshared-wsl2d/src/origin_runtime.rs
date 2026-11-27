// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/origin_runtime.rs
//! Origin backend startup, keeping native and WSL cache authority distinct.
use std::sync::Arc;

use ramshared_block::{AuthoritativeOriginBackend, BestEffortCache, DisabledCache, FileOrigin};
use ramshared_wsl2d::host_gate::SealedOrigin;

use super::{
    BLOCK_SIZE, ControlPlaneCacheAdmission, IsolatedWorkerRuntime, NbdRuntimeStarter, OriginCache,
    WorkerRespawnBudget, WorkerSet, WorkerSpawnParams, compose_cache_admission,
};

pub(super) enum OpenedOrigin {
    Wsl(FileOrigin, SealedOrigin),
    Native(super::native_origin::AdmittedOrigin),
}
impl OpenedOrigin {
    pub(super) fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }
}

pub(super) struct OriginRuntime {
    pub(super) backend: Arc<AuthoritativeOriginBackend<FileOrigin, OriginCache>>,
    pub(super) admission: Option<ControlPlaneCacheAdmission>,
    pub(super) workers: WorkerSet,
    pub(super) native_session: Option<Arc<super::native_cache::NativeCacheSession>>,
    pub(super) pooled_session: Option<Arc<ramshared_wsl2d::pooled_cache::GuestCacheSession>>,
}

impl Drop for OriginRuntime {
    fn drop(&mut self) {
        if let Some(session) = &self.native_session {
            session.revoke();
        }
        // Fields remain owned while Drop runs, including the origin backend.
        self.workers.wait_for_shutdown();
    }
}

pub(super) fn open_configured(
    wsl: Option<&str>,
    native: Option<&super::native_origin::NativeOriginRequest>,
    size: u64,
) -> Result<Option<(OpenedOrigin, u64)>, Box<dyn std::error::Error>> {
    open_selected(
        wsl,
        native,
        size,
        super::open_validated_origin,
        super::native_origin::open,
    )
}

// Keep source selection and transfer of descriptor ownership independent of
// platform observations. Production always supplies the actual admission owners.
fn open_selected(
    wsl: Option<&str>,
    native: Option<&super::native_origin::NativeOriginRequest>,
    size: u64,
    open_wsl: impl FnOnce(&str, u64) -> Result<(FileOrigin, SealedOrigin), Box<dyn std::error::Error>>,
    open_native: impl FnOnce(
        &super::native_origin::NativeOriginRequest,
    ) -> Result<super::native_origin::AdmittedOrigin, String>,
) -> Result<Option<(OpenedOrigin, u64)>, Box<dyn std::error::Error>> {
    match (wsl, native) {
        (Some(_), Some(_)) => Err("native and WSL origins are mutually exclusive".into()),
        (None, Some(request)) => {
            let admitted = open_native(request)?;
            let logical = admitted.logical_bytes;
            Ok(Some((OpenedOrigin::Native(admitted), logical)))
        }
        (Some(path), None) => {
            let (origin, sealed) = open_wsl(path, size)?;
            eprintln!(
                "[ramsharedd] mode=origin-cache logical={} MiB partuuid={}",
                size >> 20,
                sealed.partuuid
            );
            Ok(Some((OpenedOrigin::Wsl(origin, sealed), size)))
        }
        (None, None) => Ok(None),
    }
}

#[cfg(not(test))]
const INITIAL_HOST_CATALOG_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

fn initial_catalog_wait_for(
    port: Option<&std::ffi::OsStr>,
    key: Option<&std::ffi::OsStr>,
) -> std::time::Duration {
    #[cfg(test)]
    {
        let _ = (port, key);
        std::time::Duration::ZERO
    }
    #[cfg(not(test))]
    {
        let configured = port.is_some_and(|v| !v.is_empty()) && key.is_some_and(|v| !v.is_empty());
        if configured {
            INITIAL_HOST_CATALOG_WAIT
        } else {
            std::time::Duration::ZERO
        }
    }
}

pub(super) fn build<S: NbdRuntimeStarter + ?Sized>(
    opened: OpenedOrigin,
    size: u64,
    reserve: u64,
    starter: &mut S,
) -> Result<OriginRuntime, Box<dyn std::error::Error>> {
    let vsock = crate::control_requests::VsockLeaseInputs::from_env();
    let wait = initial_catalog_wait_for(
        vsock.port_raw.as_deref().map(std::ffi::OsStr::new),
        vsock.key_file.as_deref().map(std::ffi::OsStr::new),
    );
    build_with_catalog_wait(opened, size, reserve, starter, wait)
}

pub(super) fn build_with_catalog_wait<S: NbdRuntimeStarter + ?Sized>(
    opened: OpenedOrigin,
    size: u64,
    reserve: u64,
    starter: &mut S,
    initial_catalog_wait: std::time::Duration,
) -> Result<OriginRuntime, Box<dyn std::error::Error>> {
    build_with_profile(
        opened,
        size,
        reserve,
        starter,
        initial_catalog_wait,
        active_profile_path(),
    )
}

fn build_with_profile<S: NbdRuntimeStarter + ?Sized>(
    opened: OpenedOrigin,
    size: u64,
    reserve: u64,
    starter: &mut S,
    initial_catalog_wait: std::time::Duration,
    profile_path: &std::path::Path,
) -> Result<OriginRuntime, Box<dyn std::error::Error>> {
    let (origin, sealed) = match opened {
        OpenedOrigin::Native(origin) => return build_native(origin, size, starter, profile_path),
        OpenedOrigin::Wsl(origin, sealed) => (origin, sealed),
    };
    let origin_binding = sealed.partuuid.clone();
    let admission = compose_cache_admission(sealed);
    #[cfg(test)]
    starter.seed_test_catalog(admission.host_catalog());
    // Wait briefly (within `ORIGIN_DAEMON_READINESS_TIMEOUT`) for an already-listening
    // Windows vsock authority to deliver the first host GPU catalog before probing WDDM.
    let _ = admission.wait_for_initial_catalog(initial_catalog_wait);
    // Validate geometry before spawning, then keep this owner through every
    // fallible cache-attachment step and through the caller's startup rollback.
    let backend = AuthoritativeOriginBackend::new_with_admission(
        origin,
        OriginCache::Disabled(DisabledCache),
        size,
        BLOCK_SIZE,
        admission.clone(),
    )
    .map_err(|error| error.0)?;
    let (backend, pooled_session) =
        maybe_attach_pooled_cache(profile_path, "origin-cache", &origin_binding, size, backend)?;
    if let Some(pooled_session) = pooled_session {
        return Ok(OriginRuntime {
            backend: Arc::new(backend),
            admission: Some(admission),
            workers: WorkerSet::default(),
            native_session: None,
            pooled_session: Some(pooled_session),
        });
    }
    let mut runtime = OriginRuntime {
        backend: Arc::new(backend),
        admission: Some(admission),
        workers: WorkerSet::default(),
        native_session: None,
        pooled_session: None,
    };
    let plan = starter.origin_worker_plan(size, reserve)?;
    attach_workers(&mut runtime, starter, plan, size, reserve)?;
    Ok(runtime)
}

type OriginBackend = AuthoritativeOriginBackend<FileOrigin, OriginCache>;
type PooledAttachResult = Result<
    (
        OriginBackend,
        Option<Arc<ramshared_wsl2d::pooled_cache::GuestCacheSession>>,
    ),
    Box<dyn std::error::Error>,
>;

fn maybe_attach_pooled_cache(
    profile_path: &std::path::Path,
    mode_label: &str,
    origin_binding: &str,
    size: u64,
    mut backend: OriginBackend,
) -> PooledAttachResult {
    if origin_binding.is_empty() {
        return Ok((backend, None));
    }
    let swaps = read_active_swap_paths();
    let swap_refs: Vec<&str> = swaps.iter().map(String::as_str).collect();
    let (pooled_session, pooled_read) =
        ramshared_wsl2d::pooled_cache::connector::connect_boot_pooled_session(
            profile_path,
            origin_binding,
            &swap_refs,
            size,
        )?;
    if pooled_session.spawns_independent_worker() {
        return Ok((backend, None));
    }
    let status = pooled_session.status(None);
    if let Some(state) = pooled_read {
        backend = backend.with_pooled_read(state);
    }
    eprintln!(
        "[ramsharedd] mode={mode_label} cache={} epoch={} reason={}",
        status.effective_mode.as_str(),
        status.epoch,
        status.reason.as_deref().unwrap_or("none")
    );
    Ok((backend, Some(Arc::new(pooled_session))))
}

fn active_profile_path() -> &'static std::path::Path {
    #[cfg(test)]
    {
        std::path::Path::new("/nonexistent/ramshared-test-cache-mode.json")
    }
    #[cfg(not(test))]
    {
        std::path::Path::new(ramshared_wsl2d::pooled_cache::profile::DEFAULT_CACHE_PROFILE_PATH)
    }
}

fn read_active_swap_paths() -> Vec<String> {
    std::fs::read_to_string("/proc/swaps")
        .ok()
        .map(|text| {
            text.lines()
                .skip(1)
                .filter_map(|line| line.split_whitespace().next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn attach_workers<S: NbdRuntimeStarter + ?Sized>(
    runtime: &mut OriginRuntime,
    starter: &mut S,
    plan: super::origin_plan::WorkerPlan,
    size: u64,
    reserve: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let (chunk, requests) = plan.prepare(size, reserve)?.into_parts();
    runtime.workers = WorkerSet::with_capacity(requests.len())?;
    let cache = prepare_cache_group(&requests, chunk)?;
    runtime
        .backend
        .try_control_cache(|current| *current = cache)?;
    for (index, mut params) in requests.into_iter().enumerate() {
        params.host_catalog = runtime
            .admission
            .as_ref()
            .map(|admission| admission.host_catalog().clone());
        let (cache, supervisor, unavailable_reason) = initial_worker(starter, &mut params, size);
        runtime.workers.append_initial(IsolatedWorkerRuntime {
            cache_member: index,
            startup: None,
            supervisor,
            params,
            budget: WorkerRespawnBudget::new(),
            unavailable_reason,
            lease_lost_release: false,
            permanent_fallback_logged: false,
        });
        // The guard already owns this child if cache attachment fails.
        runtime.backend.try_control_cache(|current| {
            if let OriginCache::Group(group) = current {
                group
                    .control_member(index)
                    .ok_or("initial worker slot missing")?
                    .replace(cache);
                Ok(())
            } else {
                Err("initial worker group missing")
            }
        })??;
    }
    Ok(())
}

fn prepare_cache_group(
    requests: &[WorkerSpawnParams],
    chunk: u64,
) -> Result<OriginCache, Box<dyn std::error::Error>> {
    // Allocate keys, routing and client slots before any worker can exist.
    let mut members = Vec::new();
    members.try_reserve_exact(requests.len())?;
    for params in requests {
        members.push(ramshared_block::cache_group::CacheMember::new(
            super::origin_plan::member_key(params),
            params.target_bytes,
            OriginCache::Disabled(DisabledCache),
        ));
    }
    Ok(OriginCache::Group(Box::new(
        ramshared_block::cache_group::CacheGroup::new(
            members,
            chunk,
            ramshared_block::DEFAULT_READ_TIMEOUT,
            ramshared_block::ipc_cache_client::DISABLE_TIMEOUT,
        )?,
    )))
}

fn build_native<S: NbdRuntimeStarter + ?Sized>(
    origin: super::native_origin::AdmittedOrigin,
    size: u64,
    starter: &mut S,
    profile_path: &std::path::Path,
) -> Result<OriginRuntime, Box<dyn std::error::Error>> {
    let origin_binding = match &origin.manifest.target {
        ramshared_config::resource_profile::ResourceTarget::LinuxFileOrigin {
            filesystem_uuid,
            ..
        } => filesystem_uuid.clone(),
        _ => String::new(),
    };
    let session = origin.cache_session;
    let backend = match &session {
        Some(session) => AuthoritativeOriginBackend::new_with_admission(
            origin.file,
            OriginCache::Disabled(DisabledCache),
            size,
            BLOCK_SIZE,
            super::native_cache::NativeCacheAdmission(session.clone()),
        ),
        None => AuthoritativeOriginBackend::new(
            origin.file,
            OriginCache::Disabled(DisabledCache),
            size,
            BLOCK_SIZE,
        ),
    }
    .map_err(|error| error.0)?;
    let (backend, pooled_session) = maybe_attach_pooled_cache(
        profile_path,
        "native-origin",
        &origin_binding,
        size,
        backend,
    )?;
    if let Some(pooled_session) = pooled_session {
        return Ok(OriginRuntime {
            backend: Arc::new(backend),
            admission: None,
            workers: WorkerSet::default(),
            native_session: session,
            pooled_session: Some(pooled_session),
        });
    }
    let mut runtime = OriginRuntime {
        backend: Arc::new(backend),
        admission: None,
        workers: WorkerSet::default(),
        native_session: session.clone(),
        pooled_session: None,
    };
    if let Some(session) = session {
        if session.policy.is_some() {
            let result = session.worker_plan().map_err(Into::into).and_then(|plan| {
                attach_workers(
                    &mut runtime,
                    starter,
                    plan,
                    size,
                    ramshared_vram::ReserveFloorPolicy::sealed().min_floor_bytes,
                )
            });
            if let Err(error) = result {
                session.revoke();
                eprintln!(
                    "[ramsharedd] native cache configuration refused; origin retained: {error}"
                );
            }
        } else {
            eprintln!(
                "[ramsharedd] native cache unavailable; origin retained: {}",
                session
                    .unavailable_reason
                    .as_deref()
                    .unwrap_or("no approved cache configuration")
            );
        }
    }
    Ok(runtime)
}

fn initial_worker<S: NbdRuntimeStarter + ?Sized>(
    starter: &mut S,
    params: &mut WorkerSpawnParams,
    size: u64,
) -> (
    OriginCache,
    Option<super::IsolatedWorkerSupervisor>,
    Option<String>,
) {
    match starter.spawn_isolated_gpu_worker(params) {
        Ok((client, supervisor)) => {
            if let Err(reason) = validate_reply(&client, params) {
                return (
                    OriginCache::Disabled(DisabledCache),
                    Some(supervisor),
                    Some(reason),
                );
            }
            if params.expected_adapter.is_none() {
                params.expected_adapter = client.bound_adapter_identity().cloned();
            }
            eprintln!(
                "[ramsharedd] mode=authoritative-origin logical={} MiB cache={} worker_pid={} isolation=process-isolated",
                size >> 20,
                client.state().as_str(),
                supervisor.pid()
            );
            (OriginCache::Ipc(Box::new(client)), Some(supervisor), None)
        }
        Err(error) => {
            eprintln!(
                "[ramsharedd] mode=authoritative-origin logical={} MiB cache=UNAVAILABLE isolation=bounded-worker-fallback error={error}",
                size >> 20
            );
            let supervisor = error
                .retained_child
                .map(|child| super::IsolatedWorkerSupervisor {
                    child: Some(super::WorkerChildHandle::Process(*child)),
                });
            (
                OriginCache::Disabled(DisabledCache),
                supervisor,
                Some(format!(
                    "isolated GPU cache worker startup failed: {}",
                    error.message
                )),
            )
        }
    }
}

fn validate_reply(
    client: &super::IpcCacheClient,
    params: &WorkerSpawnParams,
) -> Result<(), String> {
    if client.target_bytes() > params.target_bytes {
        return Err("initial worker exceeds its requested target ceiling".into());
    }
    if let Some(expected) = &params.expected_adapter
        && client.state() == super::OriginCacheState::Active
    {
        if client.bound_adapter_identity() != Some(expected) {
            return Err("initial worker does not match its planned adapter".into());
        }
        super::gpu_worker_request::validate_startup_binding(client)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
