// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_worker_entry.rs
//! Isolated GPU worker child process entrypoint and adapter revalidation loop.

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use ramshared_block::{GpuWorkerConfig, run_gpu_worker_loop};
use ramshared_cuda::Cuda;
use ramshared_dxg::{BudgetRequest, DxgBudgetProvider, DxgError};
use ramshared_vram::{GpuAdapterIdentity, VramProvider};
use ramshared_vulkan::{VulkanInventory, VulkanProvider};
use ramshared_wsl2d::gpu_budget::{
    DxgOpenTarget, GpuAdapterCandidate, GpuBackendKind, safe_cache_target, select_gpu_candidate,
    worker_config_for_candidate,
};
use ramshared_wsl2d::host_catalog::HostGpuCatalogProvider;

use crate::gpu_worker_request::{
    WorkerRequest, bind_parent_lifetime, claim_inherited_catalog, duplicate_inherited_socket,
    parse_isolated_gpu_worker_args,
};
use crate::gpu_worker_runtime::{
    UnavailableVramProvider, run_gpu_worker_with_selected_wddm_and_host_and_codec,
};
use crate::gpu_worker_selection::{candidate_from_budget_with_host, vulkan_candidate_with_host};
use crate::native_cache;

/// Host adapters for [`run_isolated_gpu_worker_entry_with`].
pub(crate) trait GpuWorkerEntryHost {
    fn duplicate_socket(&mut self, fd: i32) -> std::io::Result<UnixStream>;
    fn bind_parent(&mut self, socket: &UnixStream) -> std::io::Result<()>;
    fn admit(&mut self, request: &WorkerRequest) -> Result<(), String>;
    fn claim_catalog(
        &mut self,
        _catalog_fd: Option<i32>,
        _worker_socket: &UnixStream,
    ) -> std::io::Result<Option<Arc<dyn HostGpuCatalogProvider>>> {
        Ok(None)
    }
    fn probe_hardware(&self) -> bool {
        true
    }
    fn open_dxg(&self, request: BudgetRequest) -> Result<DxgBudgetProvider, DxgError>;
    fn open_dxg_target(&self, target: DxgOpenTarget<'_>) -> Result<DxgBudgetProvider, DxgError> {
        match target {
            DxgOpenTarget::Luid(req) => self.open_dxg(req),
            DxgOpenTarget::Interface { interface_id, .. } => {
                Err(DxgError::InterfaceNotFound(interface_id.to_string()))
            }
        }
    }
}

pub(crate) struct ProductionGpuWorkerEntryHost;

impl GpuWorkerEntryHost for ProductionGpuWorkerEntryHost {
    fn duplicate_socket(&mut self, fd: i32) -> std::io::Result<UnixStream> {
        duplicate_inherited_socket(fd)
    }

    fn bind_parent(&mut self, socket: &UnixStream) -> std::io::Result<()> {
        bind_parent_lifetime(socket)
    }

    fn admit(&mut self, request: &WorkerRequest) -> Result<(), String> {
        native_cache::worker::admit(request).map(|_| ())
    }

    fn claim_catalog(
        &mut self,
        catalog_fd: Option<i32>,
        worker_socket: &UnixStream,
    ) -> std::io::Result<Option<Arc<dyn HostGpuCatalogProvider>>> {
        catalog_fd
            .map(|fd| claim_inherited_catalog(fd, worker_socket))
            .transpose()
    }

    fn open_dxg(&self, request: BudgetRequest) -> Result<DxgBudgetProvider, DxgError> {
        DxgBudgetProvider::open_request(request)
    }

    fn open_dxg_target(&self, target: DxgOpenTarget<'_>) -> Result<DxgBudgetProvider, DxgError> {
        match target {
            DxgOpenTarget::Luid(request) => DxgBudgetProvider::open_request(request),
            DxgOpenTarget::Interface {
                interface_id,
                physical_adapter_index,
            } => DxgBudgetProvider::open_interface(interface_id, physical_adapter_index),
        }
    }
}

fn record_candidate(
    candidates: &mut Vec<GpuAdapterCandidate>,
    backend_label: &str,
    ordinal: u32,
    result: Result<Option<GpuAdapterCandidate>, String>,
) {
    match result {
        Ok(Some(candidate)) => candidates.push(candidate),
        Ok(None) => {}
        Err(error) => {
            eprintln!("[ramsharedd] {backend_label} adapter {ordinal} budget rejected: {error}");
        }
    }
}

fn run_cuda_worker_for_candidate(
    cuda: Option<&Cuda>,
    socket: UnixStream,
    selected: &GpuAdapterCandidate,
    worker_config: GpuWorkerConfig,
    host_catalog: Option<Arc<dyn HostGpuCatalogProvider>>,
    host: &dyn GpuWorkerEntryHost,
) -> Result<(), String> {
    let cuda = cuda.ok_or_else(|| "CUDA unavailable after adapter selection".to_string())?;
    let device = cuda
        .device(selected.ordinal as i32)
        .map_err(|error| error.to_string())?;
    let provider = cuda
        .create_context(&device)
        .map_err(|error| error.to_string())?;
    // Re-clamp downward after CUDA context creation so context VRAM overhead
    // never leaves the device slab underwater on the first heartbeat.
    let worker_config = match provider.budget_snapshot().ok().and_then(|snapshot| {
        safe_cache_target(
            &snapshot,
            worker_config.target_bytes,
            worker_config.reserve_floor_bytes,
            std::time::Instant::now(),
        )
    }) {
        Some(post_ctx_target) if post_ctx_target < worker_config.target_bytes => {
            worker_config_for_candidate(worker_config, post_ctx_target)
        }
        _ => worker_config,
    };
    run_gpu_worker_with_selected_wddm_and_host_and_codec(
        socket,
        provider,
        selected,
        worker_config,
        host_catalog,
        |target| host.open_dxg_target(target),
        |ctx, avail, frames| {
            ramshared_cuda::CudaPtxLz4Codec::new_context_codec(ctx, avail, frames)
                .map(|codec| {
                    Box::new(codec) as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec + '_>
                })
                .map_err(|error| format!("{error:?}"))
        },
    )
}

fn run_selected_worker_with(
    candidates: &[GpuAdapterCandidate],
    expected_adapter: Option<&GpuAdapterIdentity>,
    config: GpuWorkerConfig,
    run_cuda: impl FnOnce(&GpuAdapterCandidate, GpuWorkerConfig) -> Result<(), String>,
    run_vulkan: impl FnOnce(&GpuAdapterCandidate, GpuWorkerConfig) -> Result<(), String>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(selected) = select_gpu_candidate(candidates, expected_adapter).cloned() else {
        return Ok(false);
    };
    let worker_config = worker_config_for_candidate(config, selected.safe_target_bytes);
    eprintln!(
        "[ramsharedd] gpu_adapter_selected backend={:?} ordinal={} key={} safe_target_bytes={}",
        selected.backend, selected.ordinal, selected.identity.key, selected.safe_target_bytes
    );
    let result = match selected.backend {
        GpuBackendKind::Cuda => run_cuda(&selected, worker_config),
        GpuBackendKind::Vulkan => run_vulkan(&selected, worker_config),
    };
    if let Err(error) = result {
        eprintln!("[ramsharedd] selected GPU adapter failed revalidation: {error}");
        return Err(error.into());
    }
    Ok(true)
}

pub(crate) fn run_isolated_gpu_worker_entry(
    args: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut host = ProductionGpuWorkerEntryHost;
    run_isolated_gpu_worker_entry_with(args, &mut host)
}

pub(crate) fn run_isolated_gpu_worker_entry_with(
    args: &[String],
    host: &mut dyn GpuWorkerEntryHost,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = parse_isolated_gpu_worker_args(args)?;
    let config = request.config;
    let socket = host.duplicate_socket(request.fd)?;
    host.bind_parent(&socket)?;
    host.admit(&request)?;
    let host_catalog = host.claim_catalog(request.catalog_fd, &socket)?;

    let mut cuda = None;
    let mut candidates = Vec::new();
    if host.probe_hardware() {
        // Load CUDA (and its fail-closed NVML occupancy source) once. A second
        // `Cuda::load` after selection would repeat dlopen + cuInit + nvmlInit on
        // the handshake critical path and push worker startup past the parent's
        // deadline under load.
        cuda = request
            .allows_backend("cuda")
            .then(|| Cuda::load().ok())
            .flatten();
        if let Some(ref cuda) = cuda
            && let Ok(count) = cuda.device_count()
        {
            for ordinal in 0..count.max(0) as u32 {
                let Ok(device) = cuda.device(ordinal as i32) else {
                    continue;
                };
                if !request.accepts_adapter(device.adapter_identity().as_ref()) {
                    continue;
                }
                let Ok(budget) = cuda.device_budget_snapshot(&device) else {
                    continue;
                };
                record_candidate(
                    &mut candidates,
                    "CUDA",
                    ordinal,
                    candidate_from_budget_with_host(
                        budget,
                        GpuBackendKind::Cuda,
                        ordinal,
                        config,
                        host_catalog.clone(),
                        |target| host.open_dxg_target(target),
                    ),
                );
            }
        }

        if request.allows_backend("vulkan")
            && let Ok(inventory) = VulkanInventory::open()
        {
            for ordinal in 0..inventory.device_count() {
                let Ok(observation) = inventory.observe(ordinal) else {
                    continue;
                };
                record_candidate(
                    &mut candidates,
                    "Vulkan",
                    ordinal,
                    vulkan_candidate_with_host(
                        observation,
                        request.expected_adapter.as_ref(),
                        config,
                        host_catalog.clone(),
                        |target| host.open_dxg_target(target),
                    ),
                );
            }
        }
    }

    if run_selected_worker_with(
        &candidates,
        request.expected_adapter.as_ref(),
        config,
        |selected, worker_config| {
            run_cuda_worker_for_candidate(
                cuda.as_ref(),
                socket.try_clone().map_err(|error| error.to_string())?,
                selected,
                worker_config,
                host_catalog.clone(),
                host,
            )
        },
        |selected, worker_config| {
            let provider =
                VulkanProvider::open_exact_for_adapter(selected.ordinal, &selected.identity)
                    .map_err(|error| error.to_string())?;
            run_gpu_worker_with_selected_wddm_and_host_and_codec(
                socket.try_clone().map_err(|error| error.to_string())?,
                provider,
                selected,
                worker_config,
                host_catalog.clone(),
                |target| host.open_dxg_target(target),
                |vulkan, avail, frames| {
                    ramshared_vulkan::compute_lz4_codec::VulkanComputeLz4Codec::new_provider_codec(
                        vulkan, avail, frames,
                    )
                    .map(|codec| {
                        Box::new(codec)
                            as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec + '_>
                    })
                    .map_err(|error| format!("{error:?}"))
                },
            )
        },
    )? {
        return Ok(());
    }

    eprintln!("[ramsharedd] no GPU adapter passed fresh safe-budget selection; cache unavailable");
    let _ = run_gpu_worker_loop(socket, UnavailableVramProvider, config);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    struct DefaultMethodHost;

    impl GpuWorkerEntryHost for DefaultMethodHost {
        fn duplicate_socket(&mut self, _fd: i32) -> std::io::Result<UnixStream> {
            Err(std::io::Error::other("unused in unit test"))
        }

        fn bind_parent(&mut self, _socket: &UnixStream) -> std::io::Result<()> {
            Ok(())
        }

        fn admit(&mut self, _request: &WorkerRequest) -> Result<(), String> {
            Ok(())
        }

        fn open_dxg(&self, _request: BudgetRequest) -> Result<DxgBudgetProvider, DxgError> {
            Err(DxgError::Unavailable("fake dxg unavailable".into()))
        }
    }

    fn sample_candidate(backend: GpuBackendKind, safe_target_bytes: u64) -> GpuAdapterCandidate {
        GpuAdapterCandidate {
            backend,
            ordinal: 0,
            identity: GpuAdapterIdentity {
                backend: match backend {
                    GpuBackendKind::Cuda => "cuda".into(),
                    GpuBackendKind::Vulkan => "vulkan".into(),
                },
                key: "adapter-0".into(),
                luid: Some("1:2".into()),
                node_mask: Some(1),
            },
            safe_target_bytes,
            wddm_binding: None,
        }
    }

    #[test]
    fn default_host_methods_and_candidate_selection_cover_error_and_dispatch_paths() {
        let mut host = DefaultMethodHost;
        let (sock_a, _sock_b) = UnixStream::pair().expect("unix socket pair");
        assert!(host.claim_catalog(Some(9), &sock_a).unwrap().is_none());
        assert!(host.probe_hardware());
        assert!(matches!(
            host.open_dxg_target(DxgOpenTarget::Luid(BudgetRequest {
                adapter: Some(ramshared_dxg::AdapterLuid { high: 0, low: 1 }),
                physical_adapter_index: Some(0),
            })),
            Err(DxgError::Unavailable(_))
        ));
        assert!(matches!(
            host.open_dxg_target(DxgOpenTarget::Interface {
                interface_id: "pci-0000:01:00.0",
                physical_adapter_index: 0,
            }),
            Err(DxgError::InterfaceNotFound(id)) if id == "pci-0000:01:00.0"
        ));

        let cuda_candidate = sample_candidate(GpuBackendKind::Cuda, 64 * 1024 * 1024);
        let vulkan_candidate = sample_candidate(GpuBackendKind::Vulkan, 32 * 1024 * 1024);
        let mut candidates = Vec::new();
        record_candidate(&mut candidates, "CUDA", 0, Ok(Some(cuda_candidate.clone())));
        record_candidate(&mut candidates, "CUDA", 1, Ok(None));
        record_candidate(&mut candidates, "Vulkan", 2, Err("stale sample".into()));
        assert_eq!(candidates.len(), 1);

        assert!(
            !run_selected_worker_with(
                &[],
                None,
                GpuWorkerConfig::default(),
                |_, _| panic!("must not launch cuda"),
                |_, _| panic!("must not launch vulkan"),
            )
            .expect("empty candidates fall back")
        );

        let missing_cuda = run_cuda_worker_for_candidate(
            None,
            sock_a.try_clone().expect("clone unix stream"),
            &cuda_candidate,
            GpuWorkerConfig::default(),
            None,
            &host,
        )
        .expect_err("missing CUDA runtime after selection must fail closed");
        assert!(missing_cuda.contains("CUDA unavailable"));

        let requested_config = GpuWorkerConfig {
            target_bytes: 128 * 1024 * 1024,
            ..GpuWorkerConfig::default()
        };
        assert!(
            run_selected_worker_with(
                &candidates,
                None,
                requested_config,
                |selected, cfg| {
                    assert_eq!(selected.backend, GpuBackendKind::Cuda);
                    assert_eq!(cfg.target_bytes, 64 * 1024 * 1024);
                    Ok(())
                },
                |_, _| panic!("must select cuda"),
            )
            .expect("cuda candidate runs")
        );

        assert!(
            run_selected_worker_with(
                std::slice::from_ref(&vulkan_candidate),
                None,
                requested_config,
                |_, _| panic!("must select vulkan"),
                |selected, cfg| {
                    assert_eq!(selected.backend, GpuBackendKind::Vulkan);
                    assert_eq!(cfg.target_bytes, 32 * 1024 * 1024);
                    Ok(())
                },
            )
            .expect("vulkan candidate runs")
        );

        let err = run_selected_worker_with(
            &[vulkan_candidate],
            None,
            GpuWorkerConfig::default(),
            |_, _| panic!("must select vulkan"),
            |_, _| Err("revalidation mismatch".into()),
        )
        .expect_err("revalidation error propagates");
        assert!(err.to_string().contains("revalidation mismatch"));
    }
}
