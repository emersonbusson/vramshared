.. SPDX-License-Identifier: MIT
.. Author: Emerson Busson
.. Email: emersonbusson@gmail.com
.. GitHub: https://github.com/emersonbusson
.. LinkedIn: https://www.linkedin.com/in/emersonbusson
.. Source: https://github.com/emersonbusson/vramshared
.. File: Documentation/security/trust-model.rst

======================
Security & Trust Model
======================

RamShared manages swap devices, privileged systemd units, and GPU driver
libraries. Every trust boundary is designed to fail closed.

Sealed Release Bundles & OpenPGP Verification
=============================================

* Release payloads are described by a single manifest
  (``scripts/package/package-payload-manifest.sh``) shared across ``.deb``,
  ``.rpm``, Arch, and sealed Linux bundles.
* Scheduled updates (``ramshared install --auto-update``) require:

  1. A root-owned, non-writable bundle path and keyring ancestry.
  2. A valid detached OpenPGP signature (``SHA256SUMS.asc``) verified via an
     absolute-path ``gpgv`` binary (never ``PATH`` lookup), refusing expired or
     revoked keys.
  3. Strict monotonic SemVer progression (e.g. ``v1.0.0`` -> ``v1.0.1``);
     equal, older, malformed, or ``-dirty`` / ``+dirty`` candidates are refused.
  4. Never executing any binary from an unverified candidate bundle.

Descriptor & Filesystem Custody
===============================

* Security-sensitive configuration, identity, and kernel-probe reads use
  ``O_NOFOLLOW``, bounded reads, and regular-file ownership checks.
* Sealed installation writes immutable release directories under
  ``/opt/ramshared/releases/<version>`` and flips ``/opt/ramshared/current``
  atomically.

Subprocess Custody (``ramshared-exec``)
=======================================

Short-lived helper invocations run as leaders of dedicated process groups with
hard wall-clock deadlines and per-stream output bounds (64 KiB default). On
timeout or pipe error, the entire owned process group is terminated and reaped.

Dynamic Driver Library Provenance
=================================

* **Linux:** ``ramshared-cuda`` resolves ``libcuda.so.1`` and
  ``libnvidia-ml.so.1`` only through canonical root-owned system library
  directories (``/usr/lib/wsl/lib``, multiarch system paths), rejecting
  symlinks, world/group-writable paths, and ambient ``LD_PRELOAD`` /
  ``LD_LIBRARY_PATH`` / ``LD_AUDIT`` when running as uid 0.
* **Windows:** CUDA/NVML DLLs resolve strictly from ``GetSystemDirectoryW``
  with ``LOAD_LIBRARY_SEARCH_SYSTEM32``; working-directory and ``PATH`` lookup
  are excluded.
* **DXG process identity:** WSL2 ``/dev/dxg`` scopes retain a kernel ``pidfd``,
  boot ID, PID-namespace inode, and process start ticks so numeric PID reuse
  across namespaces cannot forge adapter liveness.
