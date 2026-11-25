// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/platform.rs
//! Adapt the shared bounded platform observation to readiness output.
use super::probes::{Observation, PlatformKind, PlatformProbe};

pub(crate) fn probe_platform() -> PlatformProbe {
    adapt(ramshared_block::platform::probe_platform())
}

fn adapt(probe: ramshared_block::platform::PlatformProbe) -> PlatformProbe {
    let platform = match probe.platform {
        Some(ramshared_block::platform::PlatformKind::NativeLinux) => {
            Observation::Measured(PlatformKind::NativeLinux)
        }
        Some(ramshared_block::platform::PlatformKind::Wsl1) => {
            Observation::Measured(PlatformKind::Wsl1)
        }
        Some(ramshared_block::platform::PlatformKind::Wsl2) => {
            Observation::Measured(PlatformKind::Wsl2)
        }
        None => Observation::Unknown,
    };
    PlatformProbe {
        platform,
        release: probe.release,
        version: probe.version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_preserves_measured_platform_and_unknown_evidence() {
        use ramshared_block::platform::PlatformKind as Shared;
        for (source, expected) in [
            (
                Some(Shared::NativeLinux),
                Observation::Measured(PlatformKind::NativeLinux),
            ),
            (
                Some(Shared::Wsl1),
                Observation::Measured(PlatformKind::Wsl1),
            ),
            (
                Some(Shared::Wsl2),
                Observation::Measured(PlatformKind::Wsl2),
            ),
            (None, Observation::Unknown),
        ] {
            let observed = adapt(ramshared_block::platform::PlatformProbe {
                platform: source,
                release: "observed release".into(),
                version: "observed version".into(),
            });
            assert_eq!(observed.platform, expected);
            assert_eq!(observed.release, "observed release");
            assert_eq!(observed.version, "observed version");
        }
    }
}
