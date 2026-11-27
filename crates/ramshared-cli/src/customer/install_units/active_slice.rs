// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_units/active_slice.rs

//! Admission for an active static workloads slice during a package upgrade.

use std::path::Path;

use super::{
    CustomerError, CustomerReason, CustomerState, LegacyUnitApproval, PlannedUnit, PriorUnitSpec,
    UnitHost, WORKLOADS_SLICE_UNIT, plan_units, unit_error,
};

fn unit_body_after_header(bytes: &[u8]) -> Option<&[u8]> {
    let mut body = bytes;
    while body.starts_with(b"#") {
        let line_end = body.iter().position(|byte| *byte == b'\n')?;
        let line = &body[..line_end];
        if line.contains(&0) || line.contains(&b'\r') || line.ends_with(b"\\") {
            return None;
        }
        body = &body[line_end + 1..];
    }
    body.starts_with(b"[Unit]\n").then_some(body)
}

pub(super) fn preserves_active_workloads_slice(plans: &[PlannedUnit]) -> bool {
    let Some(plan) = plans.iter().find(|plan| plan.unit == WORKLOADS_SLICE_UNIT) else {
        return false;
    };
    let PriorUnitSpec::OwnedPrior(prior) = &plan.prior else {
        return false;
    };
    prior == &plan.new_bytes
        || unit_body_after_header(prior)
            .is_some_and(|old| Some(old) == unit_body_after_header(&plan.new_bytes))
}

pub(in crate::customer) fn check_slice_before_drain(
    host: &dyn UnitHost,
    release_root: &Path,
    prior_release_root: Option<&Path>,
    product_root: &Path,
    systemd_dir: &Path,
    approvals: &[LegacyUnitApproval],
) -> Result<(), CustomerError> {
    let status = host.query_unit(WORKLOADS_SLICE_UNIT)?;
    if matches!(status.active_state.as_str(), "inactive" | "failed") {
        return Ok(());
    }
    let plans = plan_units(
        release_root,
        prior_release_root,
        product_root,
        systemd_dir,
        approvals,
    )?;
    if status.active_state == "active"
        && status.file_state == "static"
        && preserves_active_workloads_slice(&plans)
    {
        return Ok(());
    }
    Err(unit_error(
        CustomerReason::ActiveSession,
        format!(
            "NBD_INSTALL_REASON=PRODUCT_UNIT_ACTIVE ({}: {})",
            WORKLOADS_SLICE_UNIT, status.active_state
        ),
        CustomerState::Active,
    ))
}
