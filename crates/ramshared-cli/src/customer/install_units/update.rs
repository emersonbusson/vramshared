// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_units/update.rs
//! The optional signed update scheduler group and its policy-driven activation.

use super::*;
use crate::customer::release_ordering::read_auto_update_policy;

pub(in crate::customer) const UPDATE_SOCKET_UNIT: &str = "ramshared-auto-update.socket";
pub(in crate::customer) const UPDATE_TIMER_UNIT: &str = "ramshared-auto-update.timer";
pub(in crate::customer) const UPDATE_UNITS: [&str; 3] = [
    "ramshared-auto-update.service",
    UPDATE_SOCKET_UNIT,
    UPDATE_TIMER_UNIT,
];

pub(super) fn has_signed_socket(directory: &File) -> Result<bool, CustomerError> {
    let error = || update_error("cannot validate signed update socket declaration");
    match statat(directory, UPDATE_SOCKET_UNIT, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        _ => Err(error()),
    }
}

pub(super) fn group_for_install(
    candidate: &File,
    prior: Option<&File>,
) -> Result<&'static [&'static str], CustomerError> {
    if has_signed_socket(candidate)? {
        return Ok(&UPDATE_UNITS);
    }
    if prior.map(has_signed_socket).transpose()?.unwrap_or(false) {
        return Err(update_error(
            "candidate is missing the installed update scheduler group",
        ));
    }
    Ok(&[])
}

pub(in crate::customer) fn arm_if_enrolled(
    release: &Path,
    default_policy: &Path,
    owner: u32,
    host: &mut dyn UnitHost,
) -> Result<(), CustomerError> {
    // The caller verified the selected release and its systemd files.
    let Some(units) = open_directory_no_follow(&release.join("systemd"), false)? else {
        return Ok(());
    };
    if has_signed_socket(&units)?
        && read_auto_update_policy(default_policy, Some(owner))?
            .is_some_and(|policy| policy.enabled)
    {
        host.enable_update_schedule()?;
    }
    Ok(())
}

pub(super) fn enable() -> Result<(), CustomerError> {
    let run = |args| {
        run_systemctl_capture(
            args,
            "installer update schedule",
            CustomerReason::ObservationUnavailable,
            CustomerState::InstalledOff,
            "cannot enable signed update schedule",
        )
        .map(|_| ())
    };
    run(&["enable", "--", UPDATE_TIMER_UNIT][..])?;
    // If a prior timer was already running, start the new socket explicitly.
    // Queue the jobs without waiting on an updater that may be the caller.
    run(&[
        "start",
        "--no-block",
        "--",
        UPDATE_TIMER_UNIT,
        UPDATE_SOCKET_UNIT,
    ])
}

fn update_error(detail: &str) -> CustomerError {
    unit_error(
        CustomerReason::IdentityChanged,
        detail,
        CustomerState::InstalledOff,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    struct Host {
        enable_calls: u32,
    }

    impl UnitHost for Host {
        fn query_unit(&self, _: &str) -> Result<UnitRuntimeStatus, CustomerError> {
            unreachable!("activation uses already qualified unit bytes")
        }
        fn daemon_reload(&mut self) -> Result<(), CustomerError> {
            Ok(())
        }
        fn sync_selected_release(&mut self) -> Result<(), CustomerError> {
            Ok(())
        }
        fn enable_update_schedule(&mut self) -> Result<(), CustomerError> {
            self.enable_calls += 1;
            Ok(())
        }
    }

    #[test]
    fn only_an_enrolled_release_with_the_signed_socket_arms_the_scheduler() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let units = root.join("systemd");
        let policy = root.join("policy");
        let owner = rustix::process::geteuid().as_raw();
        let mut host = Host { enable_calls: 0 };
        std::fs::create_dir(&units).unwrap();

        std::fs::write(&policy, b"enabled=1\nsource=github\n").unwrap();
        arm_if_enrolled(root, &policy, owner, &mut host).unwrap();
        assert_eq!(host.enable_calls, 0);
        std::fs::write(units.join(UPDATE_SOCKET_UNIT), b"socket").unwrap();
        std::fs::write(&policy, b"enabled=0\nsource=github\n").unwrap();
        arm_if_enrolled(root, &policy, owner, &mut host).unwrap();
        assert_eq!(host.enable_calls, 0);
        std::fs::write(&policy, b"enabled=1\nsource=github\n").unwrap();
        arm_if_enrolled(root, &policy, owner, &mut host).unwrap();
        assert_eq!(host.enable_calls, 1);
    }
}
