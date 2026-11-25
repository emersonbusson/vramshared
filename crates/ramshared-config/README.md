# ramshared-config
<!--
Author: Emerson Busson
Email: emersonbusson@gmail.com
GitHub: https://github.com/emersonbusson
LinkedIn: https://www.linkedin.com/in/emersonbusson
Source: https://github.com/emersonbusson/vramshared
File: crates/ramshared-config/README.md
-->

Resource-profile schema and fail-closed limit validation.

## Scope & Responsibility

`ramshared-config` owns the **resource profile** document and its limits. That is
the surface the product consumes.

- **Resource Profile Model (consumed):** Versioned variable tier ceilings and
  multiple stable Linux/WSL2 storage targets. Swap and origin can be placed on
  the same or different volumes; checked capacity is grouped by stable volume
  identity. Loaded by `ramshared-cli` through `resource_config`; this crate
  stays a pure parser/validator and performs no I/O or persistence.
- **Broker & Agent `Config` schema (not wired):** `Config`, `BrokerConfig` and
  `AgentConfig` are a hardened second TOML schema — `deny_unknown_fields`,
  independent endpoint validation, checked slice bounds. **No runtime startup
  invokes it.** The broker uses its own `ArbiterConfig`, the agent takes argv,
  and the Windows services use their own types. Do not read this type as the
  product's configuration path; a future wiring decision is a separate change
  and must absorb the existing surfaces rather than sit beside them.
- **Fail-Closed Validation:** Every parser rejects unknown schema fields and
  unsafe paths before any caller can act on the document.
- **Pure Library Design:** Parsing logic is fully decoupled from I/O to enable
  deterministic offline unit testing.

## Workspace Dependencies

- Pure configuration library; zero internal workspace dependencies.

## Safety Invariants

- **Safe Code Only:** `#![forbid(unsafe_code)]` enforced.
- **Typed Errors:** `resource_profile` returns [`ResourceProfileError`](src/resource_profile.rs);
  the `Config` schema returns [`ConfigError`](src/error.rs). Neither terminates
  the process.
- **Resource Profile Bounds:** The profile parser caps input at 64 KiB, rejects
  unknown schema fields and unsafe paths, and keeps host/guest mutations outside
  this crate.

## Testing

```bash
cargo test -p ramshared-config
```

The resource-profile slice has its own test target, `tests/resource_profile.rs`.
