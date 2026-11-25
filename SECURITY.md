# Security Policy
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: SECURITY.md
-->

## Supported Versions

Security fixes are maintained against the active `v1.0.x` release line.

| Version | Security support |
| ------- | ---------------- |
| `v1.0.x` (current release line) | Supported |
| `< v1.0.0` (historical releases) | End of life |

## Surface Scope

| Surface | Maturity | Scope |
| ------- | -------- | ----- |
| Linux / WSL2 userspace cascade (`ramshared`, `ramsharedd`) | Supported | Full security support |
| Linux kernel PCIe BAR0 block driver (`drivers/block/ramshared`) | Experimental | Best-effort fix upon reproduction |
| Windows StorPort miniport and broker (`drivers/windows`, `ramshared-winsvc`) | Experimental | Best-effort fix upon reproduction |

## Reporting a Vulnerability

Do not open public GitHub issues for security vulnerabilities.

Use GitHub Private Vulnerability Reporting (**Security** -> **Report a vulnerability**)
on this repository, or contact the maintainer listed in `MAINTAINERS`:
`Emerson Busson <emersonbusson@gmail.com>`.

Include:

- Affected component and version (`ramshared --build-info`)
- Reproduction steps or proof of concept
- Impact on host/guest memory, privilege boundaries, or hardware state

See [Documentation/security/trust-model.rst](Documentation/security/trust-model.rst)
for the sealed release, signature verification, and process custody model.
