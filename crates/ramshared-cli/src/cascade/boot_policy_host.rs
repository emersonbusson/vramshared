// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/boot_policy_host.rs
//! The systemd process boundary for boot policy (ITEM-6).
//!
//! [`super::boot_policy_effect`] owns every decision: what a pair move means,
//! what an exit code means, when a half-state is a refusal. This module only
//! spawns `systemctl` on the two sealed unit names and hands the raw status
//! back. A spawn wrapper has no policy in it, so there is nothing here to
//! decide and nothing to branch on beyond the platform's own exit.
//!
//! The unit names are compile-time constants of the effect module, never argv
//! input, so this cannot be steered at a foreign unit. The live proof of this
//! boundary is the attended host install, not a unit test: a test never
//! reaches systemd.

use super::boot_policy_effect::{BootPolicyHost, enabled_from_exit_code};

/// One bounded `systemctl` verb on exactly one unit name.
fn run_unit_verb_with(program: &str, verb: &str, unit: &str) -> Result<(), String> {
    let output = std::process::Command::new(program)
        .args([verb, "--quiet", unit])
        .output()
        .map_err(|error| format!("systemctl {verb} {unit} could not run: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl {verb} {unit} failed with status {:?}",
            output.status.code()
        ))
    }
}

fn query_unit_enabled_with(program: &str, unit: &str) -> Result<bool, String> {
    let output = std::process::Command::new(program)
        .args(["is-enabled", "--quiet", unit])
        .output()
        .map_err(|error| format!("systemctl is-enabled {unit} could not run: {error}"))?;
    enabled_from_exit_code(output.status.code()).ok_or_else(|| {
        format!(
            "systemctl is-enabled {unit} returned an unknown status {:?}",
            output.status.code()
        )
    })
}

/// Production host: the live systemd on this machine.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemBootPolicyHost;

impl BootPolicyHost for SystemBootPolicyHost {
    fn enable_unit(&self, unit: &str) -> Result<(), String> {
        run_unit_verb_with("systemctl", "enable", unit)
    }

    fn disable_unit(&self, unit: &str) -> Result<(), String> {
        run_unit_verb_with("systemctl", "disable", unit)
    }

    fn unit_is_enabled(&self, unit: &str) -> Result<bool, String> {
        query_unit_enabled_with("systemctl", unit)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn unit_verb_and_enabled_query_translate_exit_statuses_without_touching_systemd() {
        assert!(run_unit_verb_with("/bin/true", "enable", "fixture.service").is_ok());
        assert!(run_unit_verb_with("/bin/false", "disable", "fixture.service").is_err());
        assert!(
            run_unit_verb_with("/nonexistent-systemctl-bin", "enable", "fixture.service").is_err()
        );

        assert!(query_unit_enabled_with("/bin/true", "fixture.service").unwrap());
        assert!(!query_unit_enabled_with("/bin/false", "fixture.service").unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = std::env::temp_dir().join(format!(
                "ramshared-boot-host-exit7-{}.sh",
                std::process::id()
            ));
            std::fs::write(&script, "#!/bin/sh\nexit 7\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            let res = query_unit_enabled_with(script.to_str().unwrap(), "fixture.service");
            let _ = std::fs::remove_file(&script);
            assert!(res.is_err());
        }
        assert!(query_unit_enabled_with("/nonexistent-systemctl-bin", "fixture.service").is_err());

        // Read-only probe against host `SystemBootPolicyHost` covers the concrete adapter safely.
        let _ = SystemBootPolicyHost.unit_is_enabled("ramshared-nonexistent-unit-probe.service");
    }
}
