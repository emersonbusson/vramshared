// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/platform_admission/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn daemon_platform_preserves_shared_measured_or_unknown_evidence() {
    for (platform, expected) in [
        (Some(PlatformKind::NativeLinux), Some(false)),
        (Some(PlatformKind::Wsl1), Some(true)),
        (Some(PlatformKind::Wsl2), Some(true)),
        (None, None),
    ] {
        assert_eq!(observed_wsl(platform), expected);
        assert_eq!(
            ublk_platform_guard(observed_wsl(platform)).is_ok(),
            expected == Some(false)
        );
    }
}

#[test]
fn daemon_platform_runtime_guard_uses_the_shared_observation() {
    let observed = observed_wsl(probe_platform().platform);
    assert_eq!(guard_not_wsl2().is_ok(), observed == Some(false));
}
