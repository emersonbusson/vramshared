.. SPDX-License-Identifier: MIT
.. Author: Emerson Busson
.. Email: emersonbusson@gmail.com
.. GitHub: https://github.com/emersonbusson
.. LinkedIn: https://www.linkedin.com/in/emersonbusson
.. Source: https://github.com/emersonbusson/vramshared
.. File: Documentation/process/submitting-patches.rst

==================
Submitting Patches
==================

Contributions to RamShared are welcome via GitHub pull requests.

Coding Standards
================

* **Language:** English for all code, comments, commit messages, documentation,
  and pull requests.
* **Rust:** Format with ``cargo fmt --all`` and ensure zero warnings under
  ``cargo clippy --workspace --all-targets -- -D warnings``. Keep ``unsafe``
  isolated inside dedicated FFI crates; CLI and daemon code enforce
  ``#![deny(unsafe_code)]`` / ``#![forbid(unsafe_code)]``.
* **Linux kernel C:** Follow Linux kernel coding style and validate changes
  under ``drivers/block/ramshared/`` with ``scripts/ci/check-kernel-style.sh``,
  ``scripts/ci/check-kernel-abi-guard.sh``, and ``checkpatch.pl --strict``.
* **Fail-closed safety:** Never stall swap/block I/O queues on device or peer
  error; never bypass ``down --plan`` headroom checks; never implicitly arm
  swap during installation.

Commit Messages
===============

Use Conventional Commits with an imperative English title <= 72 characters::

    type(scope): imperative summary

    Explain why the change is needed and what invariants it preserves.

    Rollback trigger: measurable metric or condition requiring revert.
    Signed-off-by: Your Name <you@example.com>

* Non-trivial changes affecting locking, DMA, memory management, swap teardown,
  or release trust **must** include a ``Rollback trigger:`` line.
* Do not include automated attribution trailers.

Pre-Submission Checklist
========================

Run the full local verification suite before opening a pull request::

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace -- --test-threads=1
    ./scripts/ci/check-kernel-style.sh
    ./scripts/ci/check-kernel-abi-guard.sh
