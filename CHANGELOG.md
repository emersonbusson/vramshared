# Changelog
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: CHANGELOG.md
-->

All notable changes to RamShared are documented in this file.
The project adheres to Semantic Versioning (`MAJOR.MINOR.PATCH`).

## [1.0.5] - 2026-11-27

### Added

- Opt-in selectable Hyper-V pooled VRAM read cache (`RSCP` v1 over `AF_VSOCK` / `AF_HYPERV` port `2762`, `ramshared-pooled-cache`, `ramshared setup cache-mode stage --mode pooled`, and `hv_storvsc` storage discovery), keeping `Independent` as the default profile while enforcing host RAM and GPU VRAM reserves (#35).

### Fixed

- Raised the 7-day hardware-health journal boot-scan ceiling from 8 to 32 boots while continuing to fail closed on overflow or any machine-check event, and hardened WSL boot-canary scripts (#33).
- Enforced single-component release-name path confinement across installer transition pending records, selector effect journals, and transaction reconciliation, binding rollback cleanup to the durable stage journal (#34).

## [1.0.4] - 2026-11-27

### Added

- Signed same-series patch update acquisition and staging (`v1.0.x`) using verified digests and detached OpenPGP signatures (#30).
- Interactive update wake trigger from installed `ramshared top` with fail-closed enrollment path preparation (#31).
- Live swap-drain context and measured package installation progress banners in `ramshared top` during upgrades (#29).

## [1.0.3] - 2026-11-27

### Added

- Authenticated per-engine GPU and hardware video encode/decode activity in `ramshared top` (#27).
- Hosted Windows runtime CI qualification (#17).

### Fixed

- Preserved GPU worker exit diagnostics and reported worker codec deadlines before exit (#16, #23).
- Distinguished cache read timeouts from disconnects and preserved IPC heartbeats when GPU observations expire (#12, #13).
- Reported accurate resident ZRAM compression during writeback, observed the unbounded native cgroup hierarchy root, and bound writeback tiers in pressure evidence (#14, #20, #21).
- Preserved cascade and zram/NBD startup custody across upgrades, explained managed workload admission privileges, and hid update notices in standalone clients (#18, #19, #24, #25).
- Ignored HCS VM rows without running-state proof during automatic routing, isolated authority-only Windows installs from StorPort storage state, and bound the Arch `PKGBUILD` source archive to the checked-in commit (#15, #22, #26).

## [1.0.2] - 2026-11-25

### Fixed

- Show the busiest measured NVIDIA engine in the GPU panel, including video
  encode and decode when NVML supplies them.
- Keep cache history visible during worker recovery and explain GPU-only
  telemetry delays without changing the reported swap protection state.
- Preserve cascade activation across Linux upgrades and start release packaging
  automatically after CI and Security pass on the release commit.

## [1.0.1] - 2026-11-24

### Fixed & Security

- Upgraded `rustix` `1.1.4` -> `1.1.6` to resolve `RUSTSEC-2026-0486` (zero-length xattr buffer unsoundness).
- Made `cascade_io` daemon fixture and `package_trust` failing verifier fixture portable across Ubuntu 26.04 multicall `uutils` coreutils.
- Updated `scripts/ci/check-kernel-sparse.sh` to invoke `kbuild` (`make C=2`) against modern Linux 7.0+ kernel headers.
- Provisioned `checkpatch.pl` in guest-safety CI and scoped `.gitleaks.toml` allowlists to synthetic sequential test GPU UUIDs.

## [1.0.0] - 2026-11-24

### Added

- Unified interactive Setup & Configuration Assistant (`ramshared config wizard [--json]`,
  `ramshared setup wizard [--json]`, and `w` inside `ramshared config`) implemented in safe Rust.
- Read-only kernel capability prober (`CONFIG_ZRAM_MULTI_COMP`, `CONFIG_ZRAM_BACKEND_LZ4`,
  `CONFIG_ZRAM_BACKEND_ZSTD`, `CONFIG_ZRAM_TRACK_ENTRY_ACTIME`, `CONFIG_LRU_GEN`, `CONFIG_PSI`,
  `CONFIG_BLK_DEV_UBLK`, `CONFIG_ZRAM_WRITEBACK`) with `O_NOFOLLOW` descriptor custody.
- Adaptive ZRAM secondary recompression (`recomp_algorithm=zstd`) when the booted kernel
  advertises `CONFIG_ZRAM_MULTI_COMP` and Zstd backend support.
- Full SemVer release ordering (`v0.14.1` -> `v1.0.0` -> `v1.0.1`) across the native Rust
  updater and shell release helpers, refusing `-dirty`/`+dirty` candidates.
- Linux-kernel-standard `Documentation/` tree (`admin-guide`, `architecture`, `block`,
  `security`, `process`, and `ABI/fixtures`) and `MAINTAINERS`.

### Changed

- Kept Tier 2 VRAM cache active under `CRITICAL` host memory pressure instead of forcing a
  premature demote loop, reporting honest pause reasons when admission is temporarily gated.
- Lowered GPU runtime free-buffer margin from `640 MiB` to `256 MiB` with admission/abort
  hysteresis and post-CUDA-context re-clamp to eliminate allocation busy-loops on 6 GiB GPUs.
- Added pressure-aware adaptive ZRAM writeback spill pumping (`ramsharedd`).
- Bounded Windows host GPU catalog heartbeat probe cadence to `500 ms`.
- Updated WSL2 kernel builder defaults (`scripts/kernel/build-wsl-kernel.sh`) to enable
  LZ4/Zstd ZRAM backends, multi-compression (`CONFIG_ZRAM_MULTI_COMP`), and MGLRU (`CONFIG_LRU_GEN`).
