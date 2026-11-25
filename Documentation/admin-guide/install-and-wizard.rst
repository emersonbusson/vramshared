.. SPDX-License-Identifier: MIT
.. Author: Emerson Busson
.. Email: emersonbusson@gmail.com
.. GitHub: https://github.com/emersonbusson
.. LinkedIn: https://www.linkedin.com/in/emersonbusson
.. Source: https://github.com/emersonbusson/vramshared
.. File: Documentation/admin-guide/install-and-wizard.rst

=========================================
Installation, Setup Wizard & Kernel Guide
=========================================

Building from Source
====================

Build the CLI (``ramshared``) and block tiering daemon (``ramsharedd``)::

    git clone https://github.com/emersonbusson/vramshared.git
    cd vramshared
    cargo build --release --locked -p ramshared-cli -p ramshared-wsl2d

Sealed Release Installation
===========================

Production deployments use an immutable release directory under
``/opt/ramshared/releases/<version>`` selected atomically by
``/opt/ramshared/current``.

1. Build the release bundle as an unprivileged user::

    ./scripts/package/build-linux-bundle.sh

2. Inspect the installation plan without modifying the host::

    ramshared install --plan

3. Apply with the explicit release version and manifest SHA-256 digest::

    sudo ./scripts/install-release.sh \
        --approve-nbd-product-install <version> \
        --approve-manifest-sha256 <digest> \
        --lower-sink <safe-absolute-dir> \
        --posture <writeback|swap> \
        --skip-build

Package installation (``.deb``, ``.rpm``, Arch) finishes in the **Installed/Off**
state: installing a package never implicitly arms swap or enables boot units.

Unified Setup & Configuration Assistant
=======================================

Run the read-only setup assistant at any time (before or after installation)::

    ramshared config wizard
    ramshared config wizard --json
    ramshared setup wizard --json

The assistant is also available inside the interactive resource editor
(``ramshared config``) by pressing ``w``. It performs zero filesystem mutations
and probes six surfaces:

1. **Sealed installation:** active release selector and binary integrity.
2. **Three-tier configuration:** ``/etc/ramshared/cascade.conf`` and resource
   profile status.
3. **Tier 2 transport:** ``nbd`` / ``ublk`` availability for the running platform.
4. **GPU & control plane:** detected GPUs and live VRAM cache status from
   ``/run/ramshared/cache-status.json``.
5. **Boot & auto-update:** systemd unit state and update policy.
6. **Kernel optimizations (optional):** read-only ``O_NOFOLLOW`` inspection of
   ``/proc/config.gz`` and ``/sys/devices/virtual/block/zram*``.

Kernel Optimization Symbols
===========================

When running on a custom Linux or WSL2 kernel, RamShared automatically adapts
to the kernel's compiled capabilities:

+-----------------------------------+----------------------------------------------------+
| Kernel config symbol              | Runtime effect                                     |
+===================================+====================================================+
| ``CONFIG_ZRAM_BACKEND_LZ4``       | Low-latency primary Tier 1 ZRAM compressor         |
+-----------------------------------+----------------------------------------------------+
| ``CONFIG_ZRAM_BACKEND_ZSTD``      | High-ratio secondary recompression backend         |
+-----------------------------------+----------------------------------------------------+
| ``CONFIG_ZRAM_MULTI_COMP``        | Enables ``recomp_algorithm=zstd`` on cold pages    |
+-----------------------------------+----------------------------------------------------+
| ``CONFIG_ZRAM_TRACK_ENTRY_ACTIME``| Tracks per-page access time for cold-page spills   |
+-----------------------------------+----------------------------------------------------+
| ``CONFIG_ZRAM_WRITEBACK``         | Enables ZRAM writeback to Tier 2 backing device    |
+-----------------------------------+----------------------------------------------------+
| ``CONFIG_LRU_GEN``                | Multi-Gen LRU (MGLRU) page reclaim                 |
+-----------------------------------+----------------------------------------------------+
| ``CONFIG_PSI``                    | Pressure Stall Information for adaptive spill      |
+-----------------------------------+----------------------------------------------------+
| ``CONFIG_BLK_DEV_UBLK``           | Zero-copy ``io_uring`` userspace block transport   |
+-----------------------------------+----------------------------------------------------+

To build a WSL2 kernel with these symbols enabled, use::

    ./scripts/kernel/build-wsl-kernel.sh

Systemd Boot Integration
========================

Boot autostart is strictly opt-in. After sealing the release and verifying
``/etc/ramshared/origin.conf``::

    sudo systemctl enable ramshared-host-gate.service \
        ramshared-cascade.service \
        ramshared-supervisor.service

Disable autostart with ``systemctl disable`` on the same three units.
