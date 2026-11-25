# vRamShared
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: README.md
-->

vRamShared (CLI/daemon: `ramshared` / `ramsharedd`) is a hardware-accelerated
three-tier memory cascade and experimental kernel block driver stack for Linux
and WSL2. It places idle GPU VRAM as a volatile, bounded, write-through cache
between compressed RAM (`zram`) and an authoritative SSD origin.

## Architecture

```text
Tier 1 (prio 200)   zram (compressed RAM, LZ4/Zstd, optional multi-comp)
        |
        v
Tier 2 (prio 100)   ramsharedd (NBD / ublk block server + isolated GPU VRAM cache)
        |
        v
Tier 3 (backing)    Authoritative SSD origin (durable correctness boundary)
        |
        v
Fallback (prio -2)  OS / WSL2 disk swap
```

- **Fail-closed correctness:** Every dirty page is backed by the authoritative
  SSD origin. GPU VRAM acts strictly as a volatile read/write-through cache that
  yields immediately when external GPU workloads need headroom.
- **Process-isolated GPU worker:** CUDA/NVML/Vulkan/DXG calls execute inside a
  bounded child worker so a driver reset or allocation failure never stalls the
  block server.
- **Swapoff-first teardown:** `ramshared down` refuses teardown if resident RAM
  cannot absorb active managed swap (`down --plan`), preventing swap-drain
  deadlocks.
- **Safe Rust control plane:** `ramshared` and `ramsharedd` enforce
  `#![deny(unsafe_code)]` / `#![forbid(unsafe_code)]` with FFI isolated in
  dedicated provider crates (`ramshared-cuda`, `ramshared-vulkan`, `ramshared-dxg`).

## Requirements

- Linux kernel >= 5.15 or WSL2 (Windows 10 19044+ / Windows 11)
- Rust toolchain pinned in `rust-toolchain.toml` (for building from source)
- NVIDIA GPU with runtime CUDA/NVML driver (or compatible Vulkan 1.1+ adapter);
  runs origin-only without a GPU

## Quick Start

Build from source:

```bash
git clone https://github.com/emersonbusson/vramshared.git
cd vramshared
cargo build --release --locked -p ramshared-cli -p ramshared-wsl2d
```

Inspect system readiness, detected hardware, and kernel optimization symbols:

```bash
./target/release/ramshared config wizard
./target/release/ramshared config wizard --json
./target/release/ramshared check
```

Monitor an active cascade:

```bash
ramshared status
ramshared top
```

## Packaging

Release binaries and packages (`.deb`, `.rpm`, Arch tarball, sealed Linux bundle)
are built from a single payload manifest (`scripts/package/package-payload-manifest.sh`):

```bash
./scripts/package/build-linux-bundle.sh
./scripts/package/build-deb-package.sh v1.0.0
./scripts/package/build-rpm-package.sh v1.0.0
```

## Documentation

Complete technical documentation follows the Linux kernel `Documentation/` layout:

- [Documentation/index.rst](Documentation/index.rst) — Documentation index
- [Documentation/admin-guide/install-and-wizard.rst](Documentation/admin-guide/install-and-wizard.rst) — Installation, setup wizard, and kernel tuning
- [Documentation/admin-guide/operations.rst](Documentation/admin-guide/operations.rst) — Lifecycle operations, teardown headroom, and recovery
- [Documentation/admin-guide/configuration.rst](Documentation/admin-guide/configuration.rst) — CLI surface, environment variables, and files
- [Documentation/architecture/three-tier-cascade.rst](Documentation/architecture/three-tier-cascade.rst) — Three-tier cascade, transports, and GPU cache worker
- [Documentation/block/ramshared.rst](Documentation/block/ramshared.rst) — Experimental Linux PCIe BAR0 block driver (`drivers/block/ramshared`)
- [Documentation/security/trust-model.rst](Documentation/security/trust-model.rst) — Sealed bundles, OpenPGP signatures, and process custody
- [Documentation/process/submitting-patches.rst](Documentation/process/submitting-patches.rst) — Contributing, coding style, and patch checklist

## License

Userspace crates and documentation are licensed under the MIT License (`LICENSE`).
The Linux kernel block driver (`drivers/block/ramshared`) and GPL-tagged packaging
and safety scripts are licensed under `GPL-2.0-only`.
