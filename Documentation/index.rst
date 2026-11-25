.. SPDX-License-Identifier: MIT
.. Author: Emerson Busson
.. Email: emersonbusson@gmail.com
.. GitHub: https://github.com/emersonbusson
.. LinkedIn: https://www.linkedin.com/in/emersonbusson
.. Source: https://github.com/emersonbusson/vramshared
.. File: Documentation/index.rst

=======================
RamShared Documentation
=======================

This tree contains technical documentation for RamShared, structured after the
Linux kernel ``Documentation/`` hierarchy.

.. toctree::
   :maxdepth: 2

   admin-guide/index
   architecture/three-tier-cascade
   block/ramshared
   security/trust-model
   process/submitting-patches

Directories
===========

* ``Documentation/admin-guide/`` — Installation, configuration wizard, runtime
  operations, teardown headroom checks, and recovery procedures.
* ``Documentation/architecture/`` — Three-tier memory cascade design, transport
  boundaries (``nbd`` / ``ublk``), GPU VRAM cache isolation, and reserve policy.
* ``Documentation/block/`` — Experimental in-tree Linux PCIe BAR0 block driver
  (``drivers/block/ramshared``).
* ``Documentation/security/`` — Sealed release verification, OpenPGP signatures,
  file/descriptor custody, and DLL/shared-library provenance.
* ``Documentation/process/`` — Patch submission rules, commit conventions, and
  testing gates.
* ``Documentation/ABI/`` — Wire-format protocol fixtures consumed by automated
  serialization tests.
