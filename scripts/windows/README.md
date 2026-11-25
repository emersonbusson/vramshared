# Windows VM harness scripts (SPEC windows-swap-driver)
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: scripts/windows/README.md
-->

**SPEC:** [`Documentation/architecture/three-tier-cascade.rst`](../../Documentation/architecture/three-tier-cascade.rst)
**Safety:** run only inside a disposable Hyper-V VM (RNF-6). Never thrash pagefile/swap on the live host.

| Script | ITEM | Role |
| --- | --- | --- |
| `Invoke-KernelPageDrill.ps1` | 8 | Kernel paged-pool → pagefile-VRAM; kill service; B1 vs B2; ≥3 runs with residency gate (DT-21) |
| `Invoke-RevokeDrill.ps1` | RNF-5 | Holder-cooperative teardown + `LeaseRelease` (DT-19) — **no** invented broker Msg |
| `Measure-PagefileVram.ps1` | 9 | Side-by-side page-in latency vs disk; ≥3 runs → `docs/benchmarks/results.jsonl` + `BENCHMARKS.md` |
| `Invoke-DriverSoak.ps1` | 10 | Driver Verifier soak 3×24h (DT-12) |
| `Build-Sign-Install.ps1` | 11 | Build / attestation package / install (Partner Center flow) |
| `Get-WinDrivePreflight.ps1` | preflight | Host/VM readiness checklist (no driver load) |
| `Show-RamSharedWslStatus.ps1` | WSL observer | Plan-first, heartbeat-only status view that remains available when the guest is unresponsive |

The WSL status viewer performs no guest call:

```powershell
.\scripts\windows\Show-RamSharedWslStatus.ps1
.\scripts\windows\Show-RamSharedWslStatus.ps1 -Run
.\scripts\windows\Show-RamSharedWslStatus.ps1 -Run -Json
```

Verification for the viewer is two-sided: `Test-RamSharedWslStatusStatic.ps1`
holds the read-only contract and pins the viewer's supported schema to the
version the status producer publishes; `Test-RamSharedWslStatus.ps1` runs
synthetic heartbeats through the viewer and asserts the claim arms — a
measured zero stays a zero, a figure that never arrived stays absent, and
freshness is never claimed from half an age.

```powershell
.\scripts\windows\Test-RamSharedWslStatusStatic.ps1
.\scripts\windows\Test-RamSharedWslStatus.ps1
```

Stubs print the planned surface and exit non-zero until ITEM implementation fills them in.
