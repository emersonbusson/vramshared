// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/runtime/mod.rs
// Native Linux runtime modules (SPEC zero-script-native-runtime).
// The mutating lifecycle reaches these through `native_ops`; no staging
// allowance is needed because every entrypoint has a production caller.
pub(crate) mod lifecycle_plan;
pub(crate) mod linux_swap_sys;
pub(crate) mod native_ops;
pub(crate) mod preflight_procfs;
pub(crate) mod zram_sysfs;
