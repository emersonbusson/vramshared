# ramshared-hostlaunch
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: crates/ramshared-hostlaunch/README.md
-->

Windowless Windows host launcher for non-interactive WSL-to-Windows product
calls (`SPEC wsl2-transparent-tier-service`, DT-11/DT-12).

## Scope & Responsibility

One GUI-subsystem executable that creates its child with `CREATE_NO_WINDOW` and
forwards the exit status unchanged:

- **Console ownership:** `sudo.exe` and `powershell.exe` allocate their own
  console; `-WindowStyle Hidden` hides neither. This launcher is the single
  place that decides a host call must not show a window.
- **Strict option surface:** `--sudo <boundary>`, `--timeout-ms <ms>`, `--`,
  then the program and its verbatim arguments. Unknown or duplicate options and
  relative targets are refused before any Windows API is reached.
- **Bounded wait:** the child is waited for once, terminated at the deadline,
  and its 32-bit exit code is preserved. `--timeout-ms 0` means "no deadline"
  and is reserved for the long-lived guardian producer action.
- **Forwarded stdio:** the caller's standard handles are duplicated as
  inheritable before `CreateProcessW`, so a GUI-subsystem launcher still
  returns the child's stdout/stderr to the guest or the postmortem collector.
- **No product policy:** the launcher never elevates on its own, never writes,
  and has no direct-PowerShell fallback. A missing launcher is refused by the
  guest with `VISIBLE_ELEVATION_REFUSED`.

The executable is a build product. `scripts/windows/Build-RamSharedHostLaunch.ps1`
builds it, verifies the PE subsystem is GUI (2), and stages it beside the
guardian payload sources; the installer then seals its SHA-256 into
`payload.seal.json` (schema 2).

## Workspace Dependencies

- `windows-sys` (Windows target only) — `CreateProcessW`, standard handles,
  `WaitForSingleObject`, `TerminateProcess`.

## Safety Invariants

- **No console allocation:** the image is GUI-subsystem and the child is created
  with `CREATE_NO_WINDOW`; there is no `-WindowStyle Hidden` and no shell.
- **No implicit elevation:** `--sudo` only names an existing boundary the
  launcher runs as a child. A relative target without that boundary is refused.
- **Fail-closed parsing:** NUL arguments, unknown/duplicate options, a timeout
  above the ceiling, and a command line over 32,767 UTF-16 units are refusals
  before any Windows API is called.
- **Exact status forwarding:** the child's 32-bit exit code is preserved, and a
  timeout terminates the child rather than leaving it unbounded.

## Testing

```bash
cargo test -p ramshared-hostlaunch
cargo clippy -p ramshared-hostlaunch --target x86_64-pc-windows-gnu --all-targets
```

`launch.rs` is a Windows-only drill: the non-Windows build compiles a stub that
returns `WINDOWS_ONLY` and the argv/option tests run on every host.
