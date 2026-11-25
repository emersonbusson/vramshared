# ramshared-winsvc
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: crates/ramshared-winsvc/README.md
-->

Windows StorPort virtual disk worker daemon, volume locking governor, and pagefile safety monitor.

## Scope & Responsibility

`ramshared-winsvc` executes as the user-mode service backing the Windows StorPort virtual miniport driver:
- **StorPort Request Completion:** Fulfills read and write SCSI requests forwarded by `drivers/windows/ramshared`.
- **Volume Lock Isolation:** Uses the [`LockedVolume`](src/windows_host.rs) RAII guard pattern to isolate drive volumes during format and mount transitions.
- **Ordered pagefile teardown:** Refuses disk removal while active pagefile ownership is observed; this source policy does not qualify crash-free occupied-pagefile operation.
- **Cross-Platform Testability:** Decouples core logic from Windows APIs, enabling full unit test execution on Linux via `FakeDriver` in `#![forbid(unsafe_code)]`.

## Workspace Dependencies

- [`ramshared-broker`](../ramshared-broker/README.md) — Broker protocol tenant.
- [`ramshared-cuda`](../ramshared-cuda/README.md) — Host CUDA allocation probes.

## Safety Invariants

- **Safe Volume Locking:** Strictly manages volume handles with typed errors to prevent deadlock during unmounts.
- **Non-Windows Safety:** Entire non-Windows codebase compiles under `#![forbid(unsafe_code)]`.

## Testing

```bash
cargo test -p ramshared-winsvc
```

## Read-only native GPU identity

`ramshared-winsvc inspect-gpus --json` queries native CUDA UUID/LUID/node metadata
and the corresponding owned GPU-PV interface through Windows system GDI32. It requires no drive config,
SCM start or GPU context/allocation. A malformed, ambiguous or failed collection
refuses as a whole. Reports are bounded observations, not budget authority.
The configured service samples through an owned native process/job and bounded
overlapped capture. Pending custody blocks replacement; shutdown waits for actual
exit and an empty job. Authenticated host/guest transfer and guest worker scope
remain required before these observations can support allocation admission.
The command requires Windows. Independent GPUs retain separate identities;
linked/partition aliases are not qualified by this producer.

The compiled native public-command test checks the actual child PID/creation
FILETIME through its retained Windows handle. The verification adapter
`scripts/windows/Test-NativeGpuIdentity.ps1` selects the raw-command test or the
supervised/repeated-sampler test against hashed artifacts, without installation,
service transitions or swap effects. Native timeout, output-bound and inherited
writer controls remain separate from physical GPU or occupied-tier qualification.
