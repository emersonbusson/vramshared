.. SPDX-License-Identifier: GPL-2.0-only
.. Author: Emerson Busson
.. Email: emersonbusson@gmail.com
.. GitHub: https://github.com/emersonbusson
.. LinkedIn: https://www.linkedin.com/in/emersonbusson
.. Source: https://github.com/emersonbusson/vramshared
.. File: Documentation/block/ramshared.rst

================================================
RamShared PCIe BAR0 Block Driver (Experimental)
================================================

``drivers/block/ramshared`` is an experimental in-tree Linux ``blk-mq`` block
driver that maps an explicitly attested PCIe BAR0 aperture via
``devm_ioremap_wc()`` and exposes it as a block device.

Status and Attestation Gate
===========================

**Experimental / Unqualified.** PCI class codes and BAR sizes establish address
range only, never ownership or safety for storage writes. The module therefore
binds **nothing** by default: probe returns ``-ENODEV`` unless the integrator
passes the exact PCI slot name via the ``storage_aperture`` module parameter::

    modprobe ramshared storage_aperture=0000:01:00.0 capacity_mb=1024

Transport Model
===============

* **Data path:** CPU-mediated write-combining MMIO (``memcpy_toio`` /
  ``memcpy_fromio``) with ``dma_wmb()`` / ``dma_rmb()`` barriers.
* **Single-page reclaim hook:** Implements ``.rw_page`` (guarded for kernels
  ``< 6.12``) to avoid intermediate bounce buffers.
* **Not bus-master DMA:** Historical ``dma`` symbol and sysfs attribute names
  (``/sys/block/ramshared0/dma_transfers_total``) count completed aperture
  transfers and do not imply a device-side DMA descriptor ring.

Kconfig
=======

::

    CONFIG_BLK_DEV_RAMSHARED=m

Dependencies: ``CONFIG_BLOCK``, ``CONFIG_PCI``, ``CONFIG_SWAP``.

Module Parameters
=================

+----------------------+---------+-----------+------------------+---------------------------------------------+
| Parameter            | Type    | Default   | Range            | Description                                 |
+======================+=========+===========+==================+=============================================+
| ``capacity_mb``      | ulong   | 1024      | 1 .. (1<<20)     | Capacity in MiB; refuses undersized BAR0    |
|                      |         |           |                  | with ``-ERANGE`` (no silent truncation).    |
+----------------------+---------+-----------+------------------+---------------------------------------------+
| ``queue_depth``      | uint    | 256       | 16 .. 1024       | ``blk-mq`` tag-set depth (clamped at probe).|
+----------------------+---------+-----------+------------------+---------------------------------------------+
| ``storage_aperture`` | charp   | unset     | PCI slot string  | Required per-device attestation gate.       |
+----------------------+---------+-----------+------------------+---------------------------------------------+

Static Verification
===================

Kernel code gates live under ``scripts/ci/``::

    ./scripts/ci/check-kernel-style.sh
    ./scripts/ci/check-kernel-abi-guard.sh
    ./scripts/ci/check-kernel-checkpatch.sh
