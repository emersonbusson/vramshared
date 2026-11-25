.. SPDX-License-Identifier: MIT
.. Author: Emerson Busson
.. Email: emersonbusson@gmail.com
.. GitHub: https://github.com/emersonbusson
.. LinkedIn: https://www.linkedin.com/in/emersonbusson
.. Source: https://github.com/emersonbusson/vramshared
.. File: Documentation/admin-guide/configuration.rst

=======================
Configuration Reference
=======================

The authoritative CLI command catalog lives in
``crates/ramshared-cli/src/usage.rs`` and the resource-profile schema lives in
``crates/ramshared-config/src/resource_profile.rs``.

Command Surface
===============

Core CLI verbs:

* Lifecycle & installation: ``install``, ``setup``, ``boot``, ``up``, ``down``,
  ``demote``, ``migrate-cascade``, ``swap-rebalance``, ``ensure-origin``,
  ``ensure-guardian``, ``service``, ``package``
* Diagnostics & telemetry: ``check``, ``doctor``, ``status``, ``top``,
  ``monitor``, ``diagnose``
* Configuration & testing: ``config``, ``run``, ``session``, ``supervise``,
  ``recover``, ``stress``, ``bench``

Run ``ramshared --help`` or ``ramshared <verb> --help`` for exact flags.

Operator Environment Variables
==============================

+------------------------------+--------------------------+-----------------------------------------------+
| Variable                     | Consumer                 | Purpose                                       |
+==============================+==========================+===============================================+
| ``RAMSHARED_VRAM_MIB``       | ``boot``, cascade        | Requested Tier 2 logical capacity (MiB)       |
+------------------------------+--------------------------+-----------------------------------------------+
| ``RAMSHARED_ZRAM_MIB``       | ``boot``, cascade        | Tier 1 zram size (MiB); ``0`` disables zram   |
+------------------------------+--------------------------+-----------------------------------------------+
| ``RAMSHARED_BIN_DIR``        | installer scripts        | Root-owned directory holding binaries         |
+------------------------------+--------------------------+-----------------------------------------------+
| ``RAMSHARED_ALLOW_PRESSURE`` | stress harnesses         | Set ``1`` to permit allocating pressure runs  |
+------------------------------+--------------------------+-----------------------------------------------+
| ``RAMSHARED_VSOCK_PORT``     | ``ramsharedd``           | Host-guest vsock control-plane port           |
+------------------------------+--------------------------+-----------------------------------------------+
| ``RAMSHARED_VSOCK_KEY_FILE`` | ``ramsharedd``           | Root-owned (``0600``) handshake key file      |
+------------------------------+--------------------------+-----------------------------------------------+

Configuration & State Files
===========================

+------------------------------------------+------------------+-------------------------------------------+
| Path                                     | Owner            | Purpose                                   |
+==========================================+==================+===========================================+
| ``/etc/ramshared/cascade.conf``          | administrator    | Host Tier 1 / Tier 2 sizing defaults      |
+------------------------------------------+------------------+-------------------------------------------+
| ``/etc/ramshared/origin.conf``           | host gate        | Sealed authoritative origin binding       |
+------------------------------------------+------------------+-------------------------------------------+
| ``/etc/ramshared/resource-profile.toml`` | administrator    | Versioned resource targets and caps       |
+------------------------------------------+------------------+-------------------------------------------+
| ``/etc/ramshared/native-origin.toml``    | setup apply      | Prepared native Linux file-origin record  |
+------------------------------------------+------------------+-------------------------------------------+
| ``/run/ramshared/cache-status.json``     | ``ramsharedd``   | Live GPU cache residency & pause status   |
+------------------------------------------+------------------+-------------------------------------------+
| ``/run/ramshared/demote-status.json``    | ``ramsharedd``   | Demote counters and pending age           |
+------------------------------------------+------------------+-------------------------------------------+

Exit Codes
==========

* ``0`` — Success or admitted readiness check
* ``1`` — Operational refusal, blocked admission, or runtime failure
* ``2`` — Invalid CLI arguments or disabled direct path
* ``7`` — Windows service stop refused because an active pagefile remains mounted
* ``77`` — Safety harness refused an allocating stress drill

Examples
========

::

    $ ramshared status --json
    $ ramshared check --json
    $ ramshared config wizard --json
    $ ramshared monitor --jsonl --output /var/log/ramshared/monitor.jsonl
