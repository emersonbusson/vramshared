# ramshared-vulkan
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: crates/ramshared-vulkan/README.md
-->

Vulkan-based `VramProvider` implementation that can expose compatible GPU adapters as cache candidates. AMD/Intel physical cache qualification remains open.

## Scope & Responsibility

`ramshared-vulkan` implements the `ramshared-vram` interface using the Vulkan API:
- **Adapter eligibility:** Vulkan devices, including AMD Radeon and Intel Arc, can be candidates when their driver provides the required transfer operations, stable identity, and usable memory-budget data. Per-adapter physical qualification is still required.
- **Discovery:** `VulkanInventory` owns one instance and queries exact physical-device ordinals without creating logical devices, transfer resources or device memory. Only the selected adapter is opened, after its complete captured identity is checked again. Instance enumeration and observation can still block inside a driver and therefore stay in the isolated worker.
- **Staging Buffer Management:** Utilizes pre-allocated 1 MiB host-visible staging buffers with transfer queue synchronization (`vkCmdCopyBuffer`) to eliminate allocations on the hot I/O path.
- **Adapter-bound budget:** Uses `VK_EXT_memory_budget` and a physical-device UUID or valid Windows adapter LUID when available. The extension estimates the current process's budget and heap usage. Its values do not establish host/guest physical correspondence. Observation age starts before the native query. If the extension is unavailable, the provider-local estimate cannot authorize automatic allocation. A driver sample without stable adapter identity remains informational and is also rejected by automatic admission.

## Workspace Dependencies

- [`ramshared-vram`](../ramshared-vram/README.md) — Core VRAM traits and error types.

## Safety Invariants

- **Documented FFI Safety:** All raw `ash` Vulkan calls are isolated with explicit `// SAFETY:` proofs.
- **Fail-closed fallback:** Without external memory-budget data, reported heap size is informational only and cannot authorize cache admission.

## Testing

```bash
cargo test -p ramshared-vulkan
```

Native function-table fixtures verify independent adapter budgets, query age,
creation counts and instance cleanup. Four ignored loader/ICD tests can run with
a software ICD; they exercise native enumeration and transfer I/O without
qualifying physical GPU cache operation.
