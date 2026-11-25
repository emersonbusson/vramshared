# ramshared-wsl2d
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: crates/ramshared-wsl2d/README.md
-->

Dual-tier block device daemon (`ublk` server + NBD engine), autotiering state machine, and residency canary monitor.

## Scope & Responsibility

`ramshared-wsl2d` produces the primary Linux daemon (`ramsharedd`) managing physical block interfaces in WSL2 and native Linux:
- **`ublk` Block Server:** Implements the Linux user-space block driver (`ublk`) interface for zero-copy I/O.
- **NBD Fallback Server:** Maintains an NBD fixed-newstyle server for environments without custom kernel `ublk` support.
- **Canary Residency Probes:** Continuously executes background canary read probes across allocated VRAM chunks to verify data integrity and detect eviction latency spikes.
- **Telemetry Reconciliation:** Gathers cross-tier I/O counters and reconciles swap telemetry with the supervisor.

## Workspace Dependencies

- [`ramshared-block`](../ramshared-block/README.md) — Authoritative SSD origin and cache.
- [`ramshared-config`](../ramshared-config/README.md) — Shared resource profile and native prepared-origin identity schema.
- [`ramshared-cuda`](../ramshared-cuda/README.md) — VRAM allocation and DMA.
- [`ramshared-uring`](../ramshared-uring/README.md) — `io_uring` block queue submission.
- [`ramshared-dxg`](../ramshared-dxg/README.md) — WDDM memory budget query.

## Safety Invariants

- **Safe Code Only:** `#![forbid(unsafe_code)]` enforced across the entire daemon.
- **Ordered Teardown:** Refuses detachment until devices are confirmed removed from `/proc/swaps`.

The native existing-file path uses `--native-origin-manifest` with the canonical
protected manifest. It requires root and measured native Linux, resolves the
prepared volume afresh, and holds the verified writable file descriptor and
exclusive serving lease. It derives capacity from that immutable allocation.
It serves without GPU cache authority at this stage. WSL uses its separate
sealed block manifest and host admission; neither path's evidence substitutes
for the other. See the [operations guide](../../Documentation/admin-guide/operations.rst) for the
remaining installation and activation boundaries.

## Testing & Execution

```bash
cargo test -p ramshared-wsl2d
```
