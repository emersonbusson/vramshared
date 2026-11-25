.. SPDX-License-Identifier: MIT
.. Author: Emerson Busson
.. Email: emersonbusson@gmail.com
.. GitHub: https://github.com/emersonbusson
.. LinkedIn: https://www.linkedin.com/in/emersonbusson
.. Source: https://github.com/emersonbusson/vramshared
.. File: Documentation/admin-guide/operations.rst

========================
Lifecycle & Operations
========================

This document covers day-to-day RamShared operations, safety gates, monitoring,
and incident recovery on Linux and WSL2.

System Requirements
===================

* **OS:** Linux >= 5.15 or WSL2 (Windows 10 19044+ / Windows 11).
* **GPU:** NVIDIA GPU with runtime CUDA + NVML libraries, or a Vulkan 1.1+
  adapter providing transfer queues and fresh ``VK_EXT_memory_budget`` data.
  In GPU-less / headless hosts, the VRAM cache target is zero and RamShared
  operates origin-only.
* **Host RAM:** 8 GiB minimum (16 GiB+ recommended).
* **Storage:** NVMe SSD with >= 16 GiB free space for the authoritative origin.

Pre-Flight Readiness
====================

Run ``check`` (admission gate: exit ``0`` when ready, ``1`` when blocked) or
``doctor`` (readiness plus repair guidance)::

    $ ramshared check
    $ ramshared check --json
    $ ramshared doctor
    $ ramshared config wizard --json

Interactive Resource Editor
===========================

Edit desired ZRAM, GPU VRAM, origin, fallback, and emergency tier sizes::

    $ ramshared config
    $ ramshared config plan --profile /path/to/profile.toml

Key bindings inside ``ramshared config``:

* ``Tab`` — select tier bar
* ``Left`` / ``Right`` — adjust requested size
* ``Home`` / ``End`` — clear request / select valid maximum
* ``v`` — cycle eligible storage volume
* ``p`` — cycle GPU reserve preset
* ``r`` — refresh live hardware observations
* ``w`` — open the Setup & Configuration Assistant overlay
* ``s`` — save a new private (``0600``) draft profile after typing ``SAVE``

On native Linux while idle, benchmark candidate storage volumes with::

    $ ramshared config benchmark --plan

Starting the Cascade (``up``)
=============================

::

    $ ramshared up
    $ ramshared up --vram 4096 --zram 2048

``up`` validates GPU headroom policy, verifies the sealed authoritative SSD
origin (never formatting it), initializes the block transport (``nbd`` on
standard WSL2 when explicitly acknowledged with ``--accept-wsl2-nbd``, or
``ublk`` on qualified kernels), and mounts the three-tier swap hierarchy:
**zram (prio 200) -> RamShared VRAM/SSD (prio 100) -> fallback disk (prio -2)**.

Status, Monitoring & Diagnostics
================================

Inspect live status, launch the interactive TUI, or stream JSONL telemetry::

    $ ramshared status
    $ ramshared status --json
    $ ramshared top
    $ ramshared monitor --once
    $ ramshared monitor --compact
    $ ramshared monitor --jsonl --output /var/log/ramshared/monitor.jsonl --heartbeat /run/ramshared/heartbeat.json

Export a bounded (<= 16 MiB, redacted) diagnostic bundle or summarize an event log::

    $ ramshared diagnose export --output ./ramshared-diag.json
    $ ramshared diagnose --events /var/log/ramshared/telemetry.jsonl --json

Releasing GPU VRAM Cache (``demote``)
=====================================

Yield clean GPU VRAM cache chunks back to the graphics driver before starting
a heavy compute or 3D workload::

    $ ramshared demote

The authoritative SSD origin continues serving all pages without data loss.

VRAM Headroom & Reserve Policy
==============================

* **Capacity reserve:**
  ``max(gpu_reserve_min_mib, floor(capacity * gpu_reserve_percent / 100), ceil(capacity / 5))``
  (default floor ``2048 MiB`` / ``20%``).
* **Runtime free buffer:** ``256 MiB`` held back from NVML free VRAM before
  admitting new allocations, paired with admission/abort hysteresis and
  post-CUDA-context re-clamping.
* **Windows StorPort reserve:** ``max(configured_reserve, 512 MiB, 10%)``.

Restart Headroom Check (``down --plan``) & Teardown (``down``)
==============================================================

Always evaluate teardown headroom before draining active tiers::

    $ ramshared down --plan
    $ ramshared down --plan --json
    $ ramshared down

``down --plan`` is strictly read-only. ``verdict=Safe`` requires resident
``MemAvailable`` alone to cover managed swap bytes plus safety reserve and
margin (``A >= M + R + G``). Free fallback swap is never credited because
``swapoff`` must decompress pages into resident RAM first.

When ``verdict=Blocked``, ``down`` performs zero mutations and has no
``--force`` override. Free guest memory or perform an attended reboot.

On ``Safe``, ``down`` enforces strict **swapoff-first** ordering:

1. ``swapoff`` managed tier devices (``/dev/nbd*`` / ``/dev/ublk*`` / ``/dev/zram*``).
2. Flush pending blocks to the authoritative SSD origin.
3. Detach userspace block devices and terminate daemon workers.
4. Release GPU VRAM allocations cleanly.

Fallback Swap Rebalancing (``swap-rebalance``)
==============================================

When only the lowest-priority fallback swap row needs to be drained back into
higher-priority tiers without tearing down the cascade::

    $ ramshared swap-rebalance --plan --target /dev/sdX --json
    $ ramshared swap-rebalance --apply --target /dev/sdX

Peer Failure Recovery
=====================

Never stop ``ramsharedd`` while its block device is mounted in ``/proc/swaps``.
If the daemon exits abnormally while pages remain swapped on the tier,
processes touching those pages receive ``SIGBUS``. Do not re-arm the tier until
the root cause is diagnosed:

1. Inspect ``ramshared down --plan``.
2. Only when ``Safe``, ``swapoff`` orphaned ``zram`` devices and reset them
   with ``zramctl -r``.
3. Remove stale ownership files under ``/run/ramshared`` only after verifying
   the named devices are absent from ``/proc/swaps``.

Legacy Migration
================

To migrate an active earlier cascade to the current sealed lifecycle binding::

    $ ramshared migrate-cascade --from-legacy
