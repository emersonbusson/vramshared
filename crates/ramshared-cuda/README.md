# ramshared-cuda
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: crates/ramshared-cuda/README.md
-->

Dynamic CUDA Driver API runtime loader and page-locked DMA allocator for Tier 2 VRAM.

## Scope & Responsibility

`ramshared-cuda` manages physical GPU memory across Linux (WSL2) and Windows environments:
- **Dynamic Runtime Loading:** Loads `libcuda.so.1` (Linux) or `nvcuda.dll` (Windows) at runtime without static linking against the CUDA Toolkit.
- **Device-Wide Occupancy (NVML):** Reads VRAM used/free/total through `nvmlDeviceGetMemoryInfo` from `libnvidia-ml.so.1` (Linux) or `nvml.dll` (Windows), also loaded at runtime from the same NVIDIA driver package.
- **VRAM Allocation & Zeroing:** Allocates device memory chunks (`cuMemAlloc`), clears pages (`cuMemsetD8`), and copies blocks with synchronous DMA.
- **Provider Implementation:** Implements `VramProvider` and `VramMemory` traits defined in `ramshared-vram`.
- **Probing Utility:** Provides diagnostic memory integrity probes (`pattern_for_offset`).

## Workspace Dependencies

- [`ramshared-vram`](../ramshared-vram/README.md) — Backend-agnostic VRAM traits and error definitions.

## Runtime Dependencies

Both are resolved at runtime from the installed NVIDIA driver; there is no
build-time CUDA Toolkit or NVML SDK dependency.

| Library | Linux candidates | Windows | Used for |
| --- | --- | --- | --- |
| CUDA Driver | Protected system paths for `libcuda.so.1` or `libcuda.so` | System32 `nvcuda.dll` | allocation, copy, context |
| NVML | Protected system paths for `libnvidia-ml.so.1` or `libnvidia-ml.so` | System32 `nvml.dll` | device-wide budget occupancy |

Unix discovery prefers `/usr/lib/wsl/lib`, then native architecture and distro
library directories. Administrator-installed SONAME aliases resolve through
protected root-owned directories; the loader receives only the canonical regular
file. It independently rejects symlinks, nonroot ownership and group/other write
permission throughout the final path. There is no basename search fallback.
With effective uid 0, ambient `LD_LIBRARY_PATH`, `LD_PRELOAD` and `LD_AUDIT`
also refuse. This cannot undo code preloaded before process startup or attest
arbitrary transitive dependencies. Driver calls still need worker isolation.

The optional nvCOMP path constructor uses the same final-path authority; it has
no public owner-check bypass. Path validation does not implement or qualify a
native nvCOMP backend. See [Documentation/security/trust-model.rst](../../Documentation/security/trust-model.rst).

### Why the budget must come from NVML

`cuMemGetInfo` is **not** a device-wide reading on WSL2 GPU-PV. The paravirtual
shim accounts the calling process's channel, so its free/total figures do not
move when another process allocates device memory. A VRAM budget built on that
number cannot yield to a GPU application, which is the opposite of the intended
containment behaviour.

`nvmlDeviceGetMemoryInfo` reports the whole adapter and does observe every
process. [`Cuda::device_budget_snapshot` and `Context::budget_snapshot`](src/vram_impl.rs)
therefore source occupancy from NVML. `Context::mem_info` still exposes the raw `cuMemGetInfo`
figures and is documented as allocator-local; it is not used for the budget.

`Cuda::load` **fails closed** when NVML is missing. A silent fallback to
`cuMemGetInfo` would reintroduce the defect, so a host without NVML gets no
VRAM budget and therefore no VRAM cache, which is the safe direction.

### Adapter identity and discovery

CUDA and NVML indices are separate namespaces. Budget and utilization queries
resolve the full CUDA UUID through NVML and verify the returned handle's UUID
before reading counters. Missing or mismatched identity produces no usable
budget; there is no index-based fallback. Partition-aware identity requires
`cuDeviceGetUuid_v2`; an unresolved partition cannot inherit a parent GPU's
budget.

`Cuda::device_budget_snapshot` reads a device budget without creating a CUDA
context. Its timestamp starts before the native query. Native discovery still
belongs in an isolated worker with bounded lifetime and retained child custody.
Each adapter keeps its own detected capacity, live headroom and safety reserve.
The 32 GB desktop and 96 GB workstation references are test cases, not capacity
ceilings or physical qualification claims.

## Safety Invariants

- **FFI ownership:** CUDA driver calls belong to `driver.rs` and `ffi.rs`; NVML calls and UUID handle validation belong to `nvml.rs` and `nvml/adapter.rs`.
- **RAII Lifecycle:** All device allocations automatically release CUDA memory contexts upon drop.

## Testing

```bash
cargo test -p ramshared-cuda
```

On Windows, CUDA/NVML DLL basenames resolve to an absolute path obtained from
`GetSystemDirectoryW`; dependencies use `LOAD_LIBRARY_SEARCH_SYSTEM32`.
Application, working-directory and PATH lookup are excluded. Caller-supplied
paths and malformed names refuse. Native tests exercise a copied DLL in an
isolated working directory, an already-loaded conflicting basename, legitimate
system symbols and installed driver metadata. This does not attest driver
signatures, configure a GPU or qualify the Windows storage product path.
