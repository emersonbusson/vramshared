# RamShared Linux Kernel Block Driver (`drivers/block/ramshared`)
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: drivers/block/ramshared/README.md
-->

Native in-tree Linux block driver that exposes a GPU VRAM BAR0 aperture as a
block device for memory tiering and swap paging. The transfer path is
**CPU-mediated MMIO** over the mapped aperture; it is not bus-master DMA. See
[Transport model](#transport-model) below.

## Qualification status: experimental / unqualified

**This surface is unqualified.** It stays unqualified until an explicit
supported-device and resource contract exists.

PCI class and BAR length establish **range**, never **meaning** or
**ownership**. Matching `PCI_CLASS_DISPLAY_*` or
`PCI_CLASS_ACCELERATOR_PROCESSING` and a large enough BAR0 does not prove the
aperture is dedicated storage. There is no GPU BO allocation, no DRM/TTM
reservation, no buffer lifetime, no display headroom policy and no durable
origin behind this mapping. On a device whose BAR0 is registers or shared GPU
memory, block writes can corrupt device state or another owner's contents, and
reads can expose previous contents.

The `storage_aperture` module parameter is a **per-device attestation gate**,
not a supported-device list. The platform integrator names the one PCI slot
whose BAR0 has been attested as a dedicated storage aperture; probe refuses
every other device with `-ENODEV`. Loading the module without that attestation
binds nothing. Declaring a display or accelerator BAR does not make it storage.

Do not treat this module, its historical observations below, or the fork-tree
facts as qualification of a production GPU storage path. Live WSL2 tiering runs
on the NBD logical device; see
[Relationship to the other RamShared surfaces](#relationship-to-the-other-ramshared-surfaces).

## Architecture

1. **`blk-mq` Multi-Queue Dispatch:**
   - Atomic `gendisk` allocation via `blk_mq_alloc_disk()`.
   - Hardware multi-queue request processing without global queue lock contention.
   - Tag set configuration with hardware-enforced request sizing.

2. **Synchronous `.rw_page` path:**
   - Implements `.rw_page` in `block_device_operations`
     (`ramshared_bdev_rw_page`, `queue.c`).
   - Lets the kernel page-reclaim and swap subsystem move a single page to or
     from the mapped aperture without allocating an intermediate request
     buffer. The bytes themselves are still copied by the CPU (see transport).

3. **PCIe Aperture Mapping:**
   - Maps GPU VRAM BAR0 into kernel virtual address space with
     `devm_ioremap_wc()` (`dma.c`), not `pci_iomap_wc()`.
   - Write-combining memory barriers (`dma_wmb()` / `dma_rmb()`) enforce store
     and load ordering over PCIe bus transactions.

4. **Checked Arithmetic & Boundary Hardening:**
   - Safe 64-bit capacity calculation via `check_mul_overflow()` to eliminate integer overflow during device initialization.
   - Exact BAR0 capacity contract: probe refuses a missing or undersized BAR0
     with `-ERANGE`; a successful device maps exactly its advertised capacity
     and never silently truncates the mapping to fit the aperture.
   - Bio and request bounds checking: verifies sector start and total byte length against physical PCIe aperture bounds, returning semantic `-ERANGE` on violations.
   - BAR0 `PAGE_SIZE` alignment validation on device probe.
   - Queue depth clamping: `queue_depth` is clamped to `[16..1024]` at probe.
   - Clean probe unwinding: linear error cleanup with `pci_clear_master()` during device initialization failure and module teardown.

5. **Linux 6.18+ Modernization & Ring 0 Hardening:**
   - Dynamic major allocation: sets `disk->minors = 0` and registers via `device_add_disk()` with sysfs attribute groups, conforming to Linux 6.18 `BLOCK_EXT_MAJOR` requirements.
   - Version-guarded `.rw_page` and `page_endio` hooks under `LINUX_VERSION_CODE < KERNEL_VERSION(6, 12, 0)` to maintain multi-kernel compatibility.
   - Type abstraction `ramshared_blk_mode_t` in `compat.h` bridging modern `blk_mode_t` (6.5+) and legacy `fmode_t`.
   - Transfer counter exposed at `/sys/block/ramshared0/dma_transfers_total`.
     The attribute name is historical: it counts completed aperture transfers,
     which are CPU MMIO copies, not DMA descriptors.

## Transport model

Three transport claims are easy to conflate. They are distinct here:

| Claim | This driver |
| --- | --- |
| CPU MMIO copy (`memcpy_toio` / `memcpy_fromio`) | **Yes** — this is the actual data path (`queue.c`) |
| Zero intermediate bounce buffer | **Yes** — `.rw_page` moves the page directly to/from the mapped aperture |
| Bus-master DMA (device-driven `dma_map_*` transfers) | **No** — `dma.c` only maps the BAR; there is no bus-master descriptor ring |

`struct ramshared_dma_region` and `dma_transfers_total` keep the historical
`dma` naming for the aperture mapping and its counter. Neither implies a
bus-master engine.

## Relationship to the other RamShared surfaces

Evidence produced on one surface does not qualify another. Keep them separate:

| Surface | What it is | Evidence class |
| --- | --- | --- |
| Native PCI driver (`drivers/block/ramshared`) | This module, on a bare-metal or custom-kernel PCI GPU | Fork-tree / lab; see the note below |
| Userspace CUDA host mapping | `cuMemHostRegister` + `PinnedHostMapping` in `ramshared-cuda` | Library surface, not this driver |
| NBD logical device | Standard WSL2 userspace path (`ramshared` / `ramsharedd`) | The live WSL2 baseline specified in the root docs |
| Custom-kernel `ublk` | Conditional `io_uring` transport on a WSL2 custom kernel | Bounded transport qualification, not the default |

**Live WSL2 tiering runs on the NBD logical device.** `ublk` is available only
when a custom kernel provides it; it is not the production baseline.

## Kernel Configuration

To build RamShared as a Linux kernel module:

```kconfig
CONFIG_BLK_DEV_RAMSHARED=m
```

Associated subsystem requirements:
```kconfig
CONFIG_BLOCK=y
CONFIG_PCI=y
CONFIG_SWAP=y
CONFIG_IO_URING=y
```

## Module Parameters

Module parameters are `capacity_mb`, `queue_depth` and `storage_aperture`.
`max_sectors` is an internal helper argument in `compat.h` and is **not**
loadable.

| Parameter | Type | Default | Valid Range | Description |
| :--- | :---: | :---: | :---: | :--- |
| `capacity_mb` | `ulong` | `1024` | `1..(1<<20)` | Device capacity in MiB. Refuses a BAR0 smaller than the requested capacity with `-ERANGE`. |
| `queue_depth` | `uint` | `256` | `16..1024` (clamped) | Tag-set queue depth per blk-mq queue. Values outside the range are clamped at probe and logged. |
| `storage_aperture` | `charp` | unset | one PCI slot name | Attestation gate. The PCI slot (e.g. `0000:01:00.0`) whose BAR0 is an attested dedicated storage aperture. Unset or mismatched → probe returns `-ENODEV`. Not a supported-device list. |

## Historical hardware observations (`legacy-unqualified`)

The following figures were collected on a host with an NVIDIA GeForce RTX 2060
over PCIe Gen 3 x16 under WSL2 2.7.14.0 with custom kernel
`6.18.40.1-microsoft-standard-WSL2+`. **No repository evidence envelope binds
them to this exact driver source, device and revision**, so they are historical/unqualified: they cannot be a baseline, a regression PASS, or
a promotion claim. They are recorded only so the original observation is not
lost.

| Metric / Dimension | Baseline (Stock NBD) | Custom Kernel 6.18.40.1 | Reported delta |
| :--- | :---: | :---: | :---: |
| Reclaim bus throughput | 6.33 GB/s | 10.17 GB/s | +60.7% |
| VRAM discharge duration | 1,516.60 ms | 61.47 ms | -95.9% |
| Allocation latency (P50) | 0.10 ms | 0.0006 ms | -99.4% |
| Tail latency (P99 jitter) | 1.10 ms | 0.0018 ms | -99.8% |
| Post-test free RAM | 7,073 MB | 9,824 MB | reported clean release |
| Host stability status | Uncalibrated risk | `PASS_ZERO_PANIC` | reported zero lockups |

Do not treat this table as a measurement of the PCI driver alone: the custom
kernel, the NBD baseline and the host configuration all moved together.

## Upstream & Fork Status

Fork-tree facts only. They describe the fork and the RFC artifacts, not a
merged mainline series.

- **LKML RFC v3:** RFC v3 patchset including `control.c`, bounds validation,
  dynamic major allocation, and transfer telemetry
  (LKML RFC v3 series).
- **WSL2 Kernel Fork:** Synchronized on [`emersonbusson/WSL2-Linux-Kernel`](https://github.com/emersonbusson/WSL2-Linux-Kernel) on branch `feature/ramshared-driver-6.18` (commit `0c3b9f2e3`).
- **Kernel documentation:** A matching `Documentation/block/ramshared.rst`
  exists in that kernel tree. It documents the fork, not a mainline release.
- **Code Quality Gates:** Validated with 0 errors on `checkpatch.pl --strict`, clean ABI guard (5/5 checks passed), and zero-panic runtime under live stress on the fork.
