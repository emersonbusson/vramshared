# ramshared-dxg
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: crates/ramshared-dxg/README.md
-->

Direct `/dev/dxg` WDDM VidMm video-memory budget and adapter telemetry interface for WSL2.

## Scope & Responsibility

`ramshared-dxg` allows user-space daemons running in WSL2 to query process-scoped
memory budgets from the Windows DirectX graphics kernel (`dxgkrnl`):
- **Adapter Enumeration:** Enumerates guest DXG adapter handles via `ENUM_ADAPTERS2_IOCTL`; this is not proof of complete physical topology.
- **VidMm Budget Query:** Queries the calling process's local-segment budget,
  usage and reservations on the selected linked-adapter node through
  `QUERY_VIDEO_MEMORY_INFO_IOCTL`. This is not device-wide free-memory telemetry.
- **Reserve consumption:** Exports observations; the caller applies the shared
  reserve policy before allocating. This interface does not allocate VRAM or
  qualify swap activation.

## Workspace Dependencies

- Low-level native interface using `libc` and the shared budget/identity contract
  from `ramshared-vram`.

## Safety Invariants

- **Isolated Unsafe Ioctls:** Mirrors Microsoft WSL2 `d3dkmthk.h` UAPI definitions; all raw ioctl calls are isolated with safe wrapper interfaces.
- **Unavailable support:** A missing `/dev/dxg` permits the caller's declared
  native-absence policy. Permission, malformed identity and operational errors
  remain errors; they do not permit that fallback.

## Testing

```bash
cargo test -p ramshared-dxg
```

## WSL2 adapter identity

CUDA/Vulkan Windows adapter LUIDs and WSL DXG channel LUIDs use separate
namespaces. The [Microsoft DXG implementation](https://github.com/microsoft/WSL2-Linux-Kernel/blob/linux-msft-wsl-6.6.y/drivers/hv/dxgkrnl/ioctl.c)
returns the guest `adapter->luid` during enumeration and keeps
`host_adapter_luid` separately. Equal numeric values across those scopes do not
establish physical identity. A single guest adapter does not prove a complete
physical topology or correspondence with a filtered allocator inventory.

`BudgetSnapshot` records its LUID namespace. This native provider always emits
`WslGuest`; conversion to the shared budget preserves that guest identity
without exporting it as a Windows physical LUID. Sample time starts before the
native query, so a slow ioctl cannot renew the sample's age.

Daemon budget composition requires an immutable binding to the complete
allocator identity, WDDM identity and matching Windows-host namespace. Every
later sample revalidates the binding. Native DXG absence may permit allocator-
only startup; available but unproven correspondence refuses cache admission.
The former `SoleAdapter` fallback has been removed.

An authenticated host UUID-to-LUID budget producer is still required for
split-namespace WSL cache admission. Typed snapshots and synthetic host-scope
tests do not qualify that producer or physical multi-adapter operation. The
legacy direct sparse-NBD budget path has a separate admission audit recorded in
[Documentation/architecture/three-tier-cascade.rst](../../Documentation/architecture/three-tier-cascade.rst).

Native scope also retains process start ticks, PID-namespace device/inode and
kernel boot identity, together with an owned creator `pidfd`. Cloned scopes keep
that kernel process handle alive as evidence; readiness marks an exited creator
invalid even if numeric observations coincide. The handle does not keep the
process running. Zero start ticks are valid observed data. Fixed-size reads
verify procfs/nsfs provenance and a complete supported stat record; unavailable
or changed evidence refuses query and explicit adapter-close ioctls. Ordinary
descriptor cleanup remains owned. These local checks do not authenticate a
remote host producer. The privileged namespace regression reproduces a numeric
PID-1 alias across two real namespaces; its execution and coverage belong to
[Documentation/security/trust-model.rst](../../Documentation/security/trust-model.rst).

Native scope requires `pidfd_open` support (Linux 5.3 or newer). Budget snapshots
and scopes are cloned ownership records, not `Copy` values. Their data equality
does not grant authority: budget composition and selected-provider reopening
also validate the captured and observed creators' liveness. No ioctl or public
budget-telemetry JSON layout changes.
