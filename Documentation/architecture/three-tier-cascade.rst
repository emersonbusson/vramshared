.. SPDX-License-Identifier: MIT
.. Author: Emerson Busson
.. Email: emersonbusson@gmail.com
.. GitHub: https://github.com/emersonbusson
.. LinkedIn: https://www.linkedin.com/in/emersonbusson
.. Source: https://github.com/emersonbusson/vramshared
.. File: Documentation/architecture/three-tier-cascade.rst

===================================
Three-Tier Memory Cascade Design
===================================

RamShared separates **correctness** (durable backing storage) from
**acceleration** (volatile compressed RAM and GPU VRAM).

Tier Hierarchy
==============

1. **Tier 1 — Compressed RAM (``zram``, swap priority 200):**
   Fastest overflow target in host RAM. When the kernel provides
   ``CONFIG_ZRAM_MULTI_COMP`` and ``CONFIG_ZRAM_BACKEND_ZSTD``, RamShared
   configures ``recomp_algorithm=zstd`` so idle pages can be recompressed at
   higher density while hot writes stay on LZ4.
2. **Tier 2 — Bounded GPU VRAM Cache (``ramsharedd``, swap priority 100):**
   Userspace block server backed by an isolated GPU cache worker. Dirty writes
   persist to Tier 3 (write-through), and clean pages are served from VRAM when
   headroom permits. Under critical memory pressure the cache remains active
   without thrashing into unnecessary demote loops; new admissions pause cleanly
   when free VRAM dips below the reserve plus ``256 MiB`` runtime buffer.
3. **Tier 3 — Authoritative SSD Origin (durable backing):**
   Sealed block device or preallocated file owning the complete logical address
   space. If the GPU cache is evicted, paused, or unavailable, all reads
   continue seamlessly from the origin.
4. **Fallback Swap (swap priority -2):**
   Standard OS/WSL2 disk swap acting as last-resort overflow below the managed
   cascade.

Block Transports
================

* **NBD (Network Block Device):** Baseline transport on stock WSL2 kernels.
  Requires explicit ``--accept-wsl2-nbd`` acknowledgment on WSL2 because a
  daemon exit while swap is armed causes ``SIGBUS`` on affected pages.
* **ublk (``io_uring`` Userspace Block Driver):** Supported on native Linux and
  custom WSL2 kernels providing ``CONFIG_BLK_DEV_UBLK``.

Process-Isolated GPU Cache Worker
=================================

All GPU driver FFI (``libcuda.so.1`` / ``libnvidia-ml.so.1`` / Vulkan /
``/dev/dxg``) runs inside a dedicated child worker process communicating over
a bounded local IPC channel (``ramshared-block``):

* Device-wide occupancy on NVIDIA GPUs is queried through NVML
  (``nvmlDeviceGetMemoryInfo``), never process-scoped ``cuMemGetInfo`` alone.
* On WSL2, guest DXG adapter LUIDs and host allocator UUIDs occupy distinct
  namespaces; adapter binding validates creator liveness via ``pidfd`` and
  never guesses correspondence by numeric index.
* Adaptive ZRAM spill pumping monitors ``/proc/pressure/memory`` (PSI) and
  adjusts writeback pacing to prevent reclaim stalls.

Workspace Crate Map
===================

* ``ramshared-cli`` — Operator CLI, setup assistant, installer, TUI monitor.
* ``ramshared-wsl2d`` — ``ramsharedd`` NBD/ublk block server and spill pump.
* ``ramshared-block`` — Authoritative origin store and isolated GPU cache worker.
* ``ramshared-vram`` — Backend-agnostic VRAM traits, reserve policy, hysteresis.
* ``ramshared-cuda`` — Dynamic CUDA Driver API and NVML loader.
* ``ramshared-vulkan`` — Vulkan 1.1+ transfer-queue VRAM provider.
* ``ramshared-dxg`` — Direct ``/dev/dxg`` WDDM budget query bindings.
* ``ramshared-uring`` — ``io_uring`` / ``ublk`` submission wrappers.
* ``ramshared-config`` — Resource profiles and prepared-origin manifests.
* ``ramshared-tier`` — Swap topology, headroom math, and readiness state machine.
* ``ramshared-ipc`` — Authenticated vsock host-guest control plane.
* ``ramshared-exec`` — Process-group bounded subprocess execution.
* ``ramshared-integrity`` — Page checksum and write-verification primitives.
* ``ramshared-broker`` / ``ramshared-agent`` — Multi-tenant slice arbitration.
* ``ramshared-winsvc`` / ``ramshared-winbroker`` / ``ramshared-hostlaunch`` —
  Windows host services and control-plane broker.
