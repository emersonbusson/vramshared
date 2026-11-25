// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/joint_move.rs
//! Joint release move effects and outcomes.
use super::JointMovePlan;

/// A refused request is known not to have started; uncertainty may leave a
/// systemd job executing after the client has exited.
#[derive(Debug)]
pub(crate) enum RestartFailure {
    Refused(String),
    Uncertain(String),
}

impl std::fmt::Display for RestartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(reason) | Self::Uncertain(reason) => f.write_str(reason),
        }
    }
}

/// What happened to one unit during a joint move.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MoveOutcome {
    Restarted,
    AlreadyCurrent,
    LeftInactive,
    /// Up, but holding no running image (a completed `Type=oneshot`). Nothing
    /// to move and nothing to claim, but the unit is still named.
    LeftNoImage,
    /// Up and running, but the identity predicate could not classify the
    /// image. Left alone and named: an unknown observation is never proof of
    /// staleness.
    LeftUnjudged,
    /// Activity was never observed. Nothing was done and nothing is claimed.
    Unobserved,
    /// The request was refused before it could be submitted to systemd.
    Failed {
        observed: String,
    },
    /// The client ended but the system manager may still execute the job.
    Uncertain {
        observed: String,
        reason: String,
    },
    /// No effect was attempted after an earlier uncertain restart.
    Deferred,
}

/// The per-unit report of one joint move. Every planned unit appears exactly
/// once, so `restarted` is distinguishable from "nothing happened".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct JointMoveReport(pub(crate) Vec<(String, MoveOutcome)>);

impl JointMoveReport {
    pub(crate) fn failed_units(&self) -> Vec<&str> {
        self.0
            .iter()
            .filter_map(|(unit, outcome)| {
                matches!(
                    outcome,
                    MoveOutcome::Failed { .. }
                        | MoveOutcome::Uncertain { .. }
                        | MoveOutcome::Deferred
                )
                .then_some(unit.as_str())
            })
            .collect()
    }

    /// One line per unit, in the vocabulary the SPEC Observability table
    /// publishes. Every planned unit appears exactly once, so a reader can
    /// tell "restarted" from "nothing happened" and can see a unit the move
    /// deliberately did not judge.
    pub(crate) fn format_lines(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|(unit, outcome)| match outcome {
                MoveOutcome::Restarted => format!("{unit}: restarted"),
                MoveOutcome::AlreadyCurrent => format!("{unit}: already_current"),
                MoveOutcome::LeftInactive => format!("{unit}: left_inactive"),
                MoveOutcome::LeftNoImage => format!("{unit}: left_no_image"),
                MoveOutcome::LeftUnjudged => format!("{unit}: left_unjudged"),
                MoveOutcome::Unobserved => format!("{unit}: unobserved"),
                MoveOutcome::Failed { observed } => format!("{unit}: failed: {observed}"),
                MoveOutcome::Uncertain { observed, reason } => {
                    format!("{unit}: uncertain: {reason}; {observed}")
                }
                MoveOutcome::Deferred => format!("{unit}: deferred after uncertain restart"),
            })
            .collect()
    }
}

/// The **entire** effect surface of a joint move.
///
/// There is deliberately no direct `start`, `stop` or `kill` operation. The
/// system manager may still signal or forcibly terminate a unit as part of
/// its own restart policy; this interface does not establish safety of that
/// policy or custody of an outstanding job.
pub(crate) trait ReleaseCoherenceEffect {
    /// Restart `unit` in place. The caller only passes units that were already
    /// active at plan time.
    fn try_restart(&mut self, unit: &str) -> Result<(), RestartFailure>;
    /// The last observed state of `unit`, used only to name a failure.
    fn observed_state(&mut self, unit: &str) -> String;
}

/// Execute a plan that is already owned. Replaying a plan whose `restart` is
/// empty restarts nothing.
pub(crate) fn apply_joint_move(
    plan: &JointMovePlan,
    effect: &mut dyn ReleaseCoherenceEffect,
) -> JointMoveReport {
    let mut rows: Vec<(String, MoveOutcome)> = Vec::with_capacity(
        plan.restart.len()
            + plan.already_current.len()
            + plan.left_inactive.len()
            + plan.left_no_image.len()
            + plan.left_unjudged.len()
            + plan.unobserved.len(),
    );
    for unit in &plan.already_current {
        rows.push((unit.clone(), MoveOutcome::AlreadyCurrent));
    }
    for unit in &plan.left_inactive {
        rows.push((unit.clone(), MoveOutcome::LeftInactive));
    }
    for unit in &plan.left_no_image {
        rows.push((unit.clone(), MoveOutcome::LeftNoImage));
    }
    for unit in &plan.left_unjudged {
        rows.push((unit.clone(), MoveOutcome::LeftUnjudged));
    }
    for unit in &plan.unobserved {
        rows.push((unit.clone(), MoveOutcome::Unobserved));
    }
    let mut uncertain = false;
    for unit in &plan.restart {
        if uncertain {
            rows.push((unit.clone(), MoveOutcome::Deferred));
            continue;
        }
        let outcome = match effect.try_restart(unit) {
            Ok(()) => MoveOutcome::Restarted,
            Err(RestartFailure::Refused(_)) => MoveOutcome::Failed {
                observed: effect.observed_state(unit),
            },
            Err(RestartFailure::Uncertain(reason)) => {
                uncertain = true;
                MoveOutcome::Uncertain {
                    observed: effect.observed_state(unit),
                    reason,
                }
            }
        };
        rows.push((unit.clone(), outcome));
    }
    JointMoveReport(rows)
}
