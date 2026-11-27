// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/scheduled.rs
//! One scheduled execution of the existing native update entrypoint.

use std::path::Path;

use super::{InstallEnvironment, require_root};
use crate::customer::install_fs::fs_error;
use crate::customer::release_ordering::read_auto_update_policy;
use crate::customer::{CustomerError, CustomerReason, CustomerState};

pub(crate) const SOCKET_PATH: &str = "/run/ramshared-update/wake.sock";

pub(super) fn is_invocation(args: &[String]) -> bool {
    args == ["--auto-update"] && std::env::var("RAMSHARED_SCHEDULED_UPDATE").as_deref() == Ok("1")
}

pub(super) fn prepare(env: &InstallEnvironment) -> Result<bool, CustomerError> {
    require_root(env)?;
    ramshared_exec::systemd_wake::consume(Path::new(SOCKET_PATH)).map_err(|err| {
        fs_error(
            CustomerReason::ObservationUnavailable,
            format!("cannot consume scheduled update notification: {err}"),
            CustomerState::InstalledOff,
        )
    })?;
    Ok(read_auto_update_policy(&env.default_policy, Some(0))?.is_some_and(|policy| policy.enabled))
}
