// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/sync.rs
//! Exclusive release synchronization and fleet planning.
use super::sync_custody::ReleaseSyncGuard;
use super::systemd::{SystemProductProcessView, SystemdReleaseCoherenceEffect};
use super::{
    JointMoveReport, ProductProcessView, ReleaseCoherenceEffect, apply_joint_move, observe_fleet,
    plan_joint_move,
};
use crate::customer::transition::TransitionPaths;
use crate::installed_image::PRODUCT_ROOT;
use std::path::Path;

/// Move every already-active stale product unit onto the selected release.
///
/// `None` when there is nothing to move against (no install). The plan is
/// built before any effect; a unit the operator left off is never started; a
/// uncertain restart is named and defers the remaining units.
pub(crate) fn sync_selected_release() -> Option<JointMoveReport> {
    let progress = crate::operation_progress::OperationProgressGuard::start(
        crate::operation_progress::OperationKind::ReleaseSync,
        "observe_fleet",
        2,
    );
    let reporter = progress.reporter();
    sync_selected_release_from(
        Path::new(PRODUCT_ROOT),
        &TransitionPaths::production(),
        |guard, root, selected| {
            reporter.update(crate::operation_progress::ProgressUpdate {
                phase: "restart_services",
                step_index: 2,
                target: Some("systemd"),
                completed_bytes: 0,
                total_bytes: 0,
                throughput_mib_per_s: None,
            });
            apply_systemd_guarded(
                guard,
                root,
                selected,
                &SystemProductProcessView::default(),
                "systemctl",
            )
        },
    )
}

pub(super) fn apply_systemd_guarded(
    guard: &ReleaseSyncGuard,
    root: &Path,
    selected: &str,
    view: &dyn ProductProcessView,
    systemctl_bin: &str,
) -> Option<JointMoveReport> {
    let mut effect = SystemdReleaseCoherenceEffect {
        custody: guard,
        product_root: root,
        selected,
        systemctl_bin,
    };
    sync_selected_release_under(root, Some(selected.to_string()), view, &mut effect)
}

pub(super) fn sync_selected_release_from(
    product_root: &Path,
    paths: &TransitionPaths,
    run: impl FnOnce(&ReleaseSyncGuard, &Path, &str) -> Option<JointMoveReport>,
) -> Option<JointMoveReport> {
    sync_selected_release_from_with_acquire(product_root, paths, ReleaseSyncGuard::acquire, run)
}

pub(super) fn sync_selected_release_from_with_acquire(
    product_root: &Path,
    paths: &TransitionPaths,
    acquire: impl FnOnce(&TransitionPaths) -> Result<ReleaseSyncGuard, String>,
    run: impl FnOnce(&ReleaseSyncGuard, &Path, &str) -> Option<JointMoveReport>,
) -> Option<JointMoveReport> {
    let selected = match super::selector::selected_release_observation(product_root) {
        Ok(Some(selected)) => selected,
        Ok(None) => return None,
        Err(reason) => {
            return Some(JointMoveReport(vec![(
                "release-sync".into(),
                super::joint_move::MoveOutcome::Failed { observed: reason },
            )]));
        }
    };
    with_sync_custody_result(acquire(paths), |guard| {
        if let Err(reason) = super::selector::require_selection(product_root, &selected) {
            return Some(JointMoveReport(vec![(
                "release-sync".into(),
                super::joint_move::MoveOutcome::Failed { observed: reason },
            )]));
        }
        run(guard, product_root, &selected)
    })
}

#[cfg(test)]
pub(super) fn with_sync_custody(
    paths: &TransitionPaths,
    run: impl FnOnce(&ReleaseSyncGuard) -> Option<JointMoveReport>,
) -> Option<JointMoveReport> {
    with_sync_custody_result(ReleaseSyncGuard::acquire(paths), run)
}

fn with_sync_custody_result(
    acquired: Result<ReleaseSyncGuard, String>,
    run: impl FnOnce(&ReleaseSyncGuard) -> Option<JointMoveReport>,
) -> Option<JointMoveReport> {
    match acquired {
        Ok(guard) => run(&guard),
        Err(reason) => Some(JointMoveReport(vec![(
            "release-sync".into(),
            super::joint_move::MoveOutcome::Failed {
                observed: format!("not submitted: {reason}"),
            },
        )])),
    }
}

pub(super) fn sync_selected_release_under(
    product_root: &Path,
    selected: Option<String>,
    view: &dyn ProductProcessView,
    effect: &mut dyn ReleaseCoherenceEffect,
) -> Option<JointMoveReport> {
    let selected = selected?;
    let release_root = product_root.join("current");
    let fleet = match observe_fleet(&release_root, Some(selected), view) {
        Ok(fleet) => fleet,
        Err(error) => {
            return Some(JointMoveReport(vec![(
                "release-sync".into(),
                super::joint_move::MoveOutcome::Failed {
                    observed: format!("fleet observation failed: {error}"),
                },
            )]));
        }
    };
    let plan = plan_joint_move(&fleet);
    Some(apply_joint_move(&plan, effect))
}
