// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_drain.rs

//! Durable ownership of a systemd stop that may outlive the installer.

use std::cell::OnceCell;
use std::ffi::OsStr;
use std::fs::File;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

use super::CustomerError;
use super::install_fs::place_atomic_regular_in_dir;
use super::install_lifecycle::{LifecycleHost, LiveRuntimeState};
use super::package_selector::{SelectorEffectPhase, SelectorEffectRecord};
use super::transition::{
    CustomerTransitionGuard, OperationOwner, TransitionPaths, acquire_transition, busy_refusal,
    read_pending, recovery_refusal,
};
use super::transition_custody::{
    clear_journal_file, open_directory_no_follow, open_named_file, read_journal_body,
    validate_directory, validate_regular,
};
use crate::operation_progress::{UpgradeDrainProgress, unix_epoch_ms};
use crate::workload::{current_boot_id, valid_boot_id};

mod progress;
pub(super) use progress::InstallPhase;
use progress::InstallerProgress;
pub(super) mod systemd;
use systemd::{ObservedUnit, StopHost, StopProcess, WaitForProgress};

const RECEIPT_NAME: &str = "upgrade-drain.json";
const RECEIPT_LABEL: &str = "upgrade drain receipt";
const LOCK_NAME: &str = "installer.lock";
const STALL_WINDOW: Duration = Duration::from_secs(45);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
static STAGING_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// Project display-safe versions from the current stop owned by this process.
pub(crate) fn observe_upgrade_drain(pid: u32, start_ticks: u64) -> Option<UpgradeDrainProgress> {
    read_upgrade_drain(&TransitionPaths::production(), pid, start_ticks)
}

fn read_upgrade_drain(
    paths: &TransitionPaths,
    pid: u32,
    start_ticks: u64,
) -> Option<UpgradeDrainProgress> {
    let body = read_journal_body(paths, OsStr::new(RECEIPT_NAME), RECEIPT_LABEL).ok()??;
    let receipt: Receipt = serde_json::from_str(&body).ok()?;
    let Phase::Submitted {
        process: Some(process),
        ..
    } = receipt.phase
    else {
        return None;
    };
    if receipt.schema_version != 1
        || receipt.boot_id != current_boot_id().ok()?
        || !receipt.prior_runtime.cascade_active
        || process.pid.get() != pid
        || process.start_ticks != start_ticks
    {
        return None;
    }
    UpgradeDrainProgress::new(
        receipt.inputs.previous_release,
        receipt.inputs.candidate_release,
    )
}

pub(super) fn require_no_drain(paths: &TransitionPaths) -> Result<(), CustomerError> {
    if read_journal_body(paths, OsStr::new(RECEIPT_NAME), RECEIPT_LABEL)?.is_some() {
        return Err(recovery_refusal(
            "an owned installer drain must settle before package mutation",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DrainInputs {
    pub(super) previous_release: String,
    pub(super) previous_manifest: String,
    pub(super) previous_directory: (u64, u64),
    pub(super) candidate_release: String,
    pub(super) candidate_manifest: String,
    pub(super) sink_path: String,
    pub(super) sink_identity: String,
    pub(super) posture: String,
    pub(super) no_activate: bool,
    pub(super) no_enroll: bool,
    pub(super) enroll_default: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", deny_unknown_fields)]
enum Phase {
    Intent,
    Submitted {
        job: NonZeroU32,
        process: Option<StopProcess>,
    },
    Drained,
    Committed(CommitWitness),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommitWitness {
    pub(super) directory: (u64, u64),
    pub(super) sealed_manifest: String,
    pub(super) enrolled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    boot_id: String,
    inputs: DrainInputs,
    prior_runtime: LiveRuntimeState,
    #[serde(default)]
    effective_enrollment: Option<bool>,
    phase: Phase,
}

pub(super) struct PackageRecoveryPlan {
    pub(super) inputs: DrainInputs,
    pub(super) enroll: bool,
    pub(super) committed: Option<CommitWitness>,
}

pub(super) enum DrainOutcome {
    Unneeded,
    Deferred(String),
    Drained(LiveRuntimeState),
}

pub(super) struct InstallerLock {
    paths: TransitionPaths,
    lock_file: File,
    progress: OnceCell<InstallerProgress>,
}

impl Drop for InstallerLock {
    fn drop(&mut self) {
        drop(self.progress.take());
        let _ = flock(&self.lock_file, FlockOperation::Unlock);
    }
}

impl InstallerLock {
    pub(super) fn acquire(paths: &TransitionPaths) -> Result<Self, CustomerError> {
        let file = open_named_file(
            &paths.lock_root,
            OsStr::new(LOCK_NAME),
            "installer lock",
            true,
        )?
        .ok_or_else(|| recovery_refusal("cannot hold the installer lock"))?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self {
                paths: paths.clone(),
                lock_file: file,
                progress: OnceCell::new(),
            }),
            Err(rustix::io::Errno::WOULDBLOCK) => Err(busy_refusal(
                "another installer is handling a release transition",
            )),
            Err(error) => Err(recovery_refusal(format!(
                "cannot lock the installer: {error}"
            ))),
        }
    }

    fn revalidate(&self) -> Result<(), CustomerError> {
        let named = open_named_file(
            &self.paths.lock_root,
            OsStr::new(LOCK_NAME),
            "installer lock",
            false,
        )?
        .ok_or_else(|| recovery_refusal("installer lock disappeared"))?;
        if validate_regular(&named, "installer lock")?
            != validate_regular(&self.lock_file, "held installer lock")?
        {
            return Err(recovery_refusal("installer lock identity changed"));
        }
        Ok(())
    }

    fn read(&self) -> Result<Option<Receipt>, CustomerError> {
        let body = read_journal_body(&self.paths, OsStr::new(RECEIPT_NAME), RECEIPT_LABEL)?;
        body.map(|body| {
            let receipt: Receipt = serde_json::from_str(&body)
                .map_err(|error| recovery_refusal(format!("invalid {RECEIPT_LABEL}: {error}")))?;
            if receipt.schema_version != 1
                || !valid_boot_id(&receipt.boot_id)
                || !receipt.prior_runtime.cascade_active
            {
                return Err(recovery_refusal(
                    "invalid upgrade drain receipt identity or posture",
                ));
            }
            Ok(receipt)
        })
        .transpose()
    }

    fn publish(&self, receipt: &Receipt) -> Result<(), CustomerError> {
        self.revalidate()?;
        let directory = open_directory_no_follow(self.paths.transactions_root(), true)?
            .ok_or_else(|| recovery_refusal("cannot open upgrade drain receipt directory"))?;
        validate_directory(&directory, RECEIPT_LABEL)?;
        let bytes = serde_json::to_vec(receipt)
            .map_err(|error| recovery_refusal(format!("cannot encode {RECEIPT_LABEL}: {error}")))?;
        let seq = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let staging = format!(
            ".upgrade-drain.{}.{}.{seq}",
            std::process::id(),
            unix_epoch_ms()
        );
        self.revalidate()?;
        place_atomic_regular_in_dir(
            &directory,
            RECEIPT_NAME,
            &staging,
            &bytes,
            0o600,
            "cannot publish upgrade drain receipt",
            "cannot sync upgrade drain receipt directory",
        )?;
        self.revalidate()
    }

    fn publish_intent(&self, receipt: &Receipt) -> Result<(), CustomerError> {
        // The active teardown holds the transition lock. Only initial intent
        // needs it. The installer lock serializes all subsequent replacements.
        let _guard = acquire_transition(OperationOwner::Up, &self.paths)?;
        self.publish(receipt)
    }

    pub(super) fn active_inputs(&self) -> Result<Option<DrainInputs>, CustomerError> {
        self.revalidate()?;
        Ok(self.read()?.map(|receipt| receipt.inputs))
    }

    #[cfg(test)]
    pub(super) fn fixture_record_drained(
        &self,
        inputs: DrainInputs,
        prior_runtime: LiveRuntimeState,
    ) -> Result<(), CustomerError> {
        self.publish_intent(&Receipt {
            schema_version: 1,
            boot_id: current_boot_id().map_err(recovery_refusal)?,
            inputs,
            prior_runtime,
            effective_enrollment: None,
            phase: Phase::Drained,
        })
    }

    pub(super) fn record_package_plan(&self, enroll: bool) -> Result<(), CustomerError> {
        let Some(mut receipt) = self.read()? else {
            return Ok(());
        };
        if receipt.phase != Phase::Drained
            || receipt
                .effective_enrollment
                .is_some_and(|recorded| recorded != enroll)
        {
            return Err(recovery_refusal(
                "package enrollment differs from the drained plan",
            ));
        }
        if receipt.effective_enrollment.is_none() {
            receipt.effective_enrollment = Some(enroll);
            self.publish(&receipt)?;
        }
        Ok(())
    }

    pub(super) fn package_recovery_plan(
        &self,
        pending: &super::transition::PackagePending,
    ) -> Result<Option<PackageRecoveryPlan>, CustomerError> {
        self.revalidate()?;
        let Some(receipt) = self.read()? else {
            return Ok(None);
        };
        if receipt.inputs.previous_manifest != pending.old_hash
            || receipt.inputs.candidate_manifest != pending.new_hash
            || pending.target_release.as_ref() != Some(&receipt.inputs.candidate_release)
        {
            return Err(recovery_refusal(
                "package recovery differs from the drained upgrade",
            ));
        }
        let committed = match receipt.phase {
            Phase::Drained => None,
            Phase::Committed(witness) => Some(witness),
            _ => {
                return Err(recovery_refusal(
                    "package recovery requires a completed drain",
                ));
            }
        };
        let enroll = receipt
            .effective_enrollment
            .ok_or_else(|| recovery_refusal("package enrollment choice was not recorded"))?;
        Ok(Some(PackageRecoveryPlan {
            inputs: receipt.inputs,
            enroll,
            committed,
        }))
    }

    pub(super) fn record_committed(
        &self,
        guard: &CustomerTransitionGuard,
        effect: &SelectorEffectRecord,
        enrolled: bool,
    ) -> Result<(), CustomerError> {
        let Some(mut receipt) = self.read()? else {
            return Ok(());
        };
        guard.revalidate()?;
        if guard.owner() != OperationOwner::Package
            || guard.paths() != &self.paths
            || effect.phase != SelectorEffectPhase::Verified
            || read_pending(&self.paths)?.as_ref() != Some(&effect.pending)
            || receipt.phase != Phase::Drained
            || receipt
                .effective_enrollment
                .is_none_or(|requested| enrolled && !requested)
            || receipt.inputs.previous_manifest != effect.pending.old_hash
            || receipt.inputs.candidate_manifest != effect.input_manifest_sha256
            || effect.pending.target_release.as_ref() != Some(&receipt.inputs.candidate_release)
            || effect
                .previous
                .as_ref()
                .map(|previous| previous.root_identity)
                != Some(receipt.inputs.previous_directory)
        {
            return Err(recovery_refusal(
                "published selector does not match the drained upgrade",
            ));
        }
        receipt.phase = Phase::Committed(CommitWitness {
            directory: effect.target_identity,
            sealed_manifest: effect.sealed_manifest_sha256.clone(),
            enrolled,
        });
        self.publish(&receipt)
    }

    pub(super) fn committed_runtime(
        &self,
    ) -> Result<Option<(DrainInputs, CommitWitness, LiveRuntimeState)>, CustomerError> {
        self.revalidate()?;
        let Some(receipt) = self.read()? else {
            return Ok(None);
        };
        let Phase::Committed(witness) = receipt.phase else {
            return Ok(None);
        };
        Ok(Some((receipt.inputs, witness, receipt.prior_runtime)))
    }

    pub(super) fn clear_finished(
        &self,
        inputs: Option<&DrainInputs>,
        package_committed: bool,
    ) -> Result<(), CustomerError> {
        self.revalidate()?;
        let Some(receipt) = self.read()? else {
            return Ok(());
        };
        let guard = acquire_transition(OperationOwner::Package, &self.paths)?;
        if Some(&receipt.inputs) != inputs {
            return Err(recovery_refusal(
                "upgrade drain receipt changed before completion",
            ));
        }
        if matches!(receipt.phase, Phase::Committed(_)) && !package_committed {
            return Ok(());
        }
        if !(receipt.phase == Phase::Drained && !package_committed
            || matches!(receipt.phase, Phase::Committed(_)) && package_committed)
        {
            return Err(recovery_refusal(
                "upgrade drain receipt is not at its terminal phase",
            ));
        }
        if read_pending(&self.paths)?.is_some() {
            return Err(recovery_refusal(
                "package effects remain pending after drain",
            ));
        }
        clear_journal_file(&guard, OsStr::new(RECEIPT_NAME), RECEIPT_LABEL)
    }

    pub(super) fn validate_no_prior_install(&self) -> Result<(), CustomerError> {
        self.revalidate()?;
        if self.read()?.is_some() {
            return Err(recovery_refusal(
                "upgrade drain receipt exists without its prior selector",
            ));
        }
        Ok(())
    }

    pub(super) fn prepare(
        &self,
        inputs: Option<&DrainInputs>,
        live: &LiveRuntimeState,
        lifecycle: &dyn LifecycleHost,
        systemd: &dyn StopHost,
    ) -> Result<DrainOutcome, CustomerError> {
        self.revalidate()?;
        self.progress_phase(InstallPhase::CheckDrain);
        let existing = self.read()?;
        if existing.is_none() && !live.cascade_active {
            return Ok(DrainOutcome::Unneeded);
        }
        let inputs = inputs.ok_or_else(|| {
            recovery_refusal("active upgrade lacks signed prior release identity")
        })?;
        let boot_id = current_boot_id().map_err(recovery_refusal)?;
        let mut receipt = match existing {
            Some(receipt) => {
                if matches!(receipt.phase, Phase::Committed(_)) {
                    return Err(recovery_refusal(
                        "committed upgrade needs runtime restoration",
                    ));
                }
                receipt.check_identity(inputs, &boot_id)?;
                receipt
            }
            None => {
                let observed = systemd.observe()?;
                if !observed.admits_stop() {
                    return Ok(DrainOutcome::Deferred(
                        "CASCADE_NOT_SETTLED_FOR_STOP".to_owned(),
                    ));
                }
                let verdict = lifecycle.evaluate_drain_safety()?;
                if !verdict.safe {
                    return Ok(DrainOutcome::Deferred(verdict.reason));
                }
                let mut receipt = Receipt {
                    schema_version: 1,
                    boot_id,
                    inputs: inputs.clone(),
                    prior_runtime: live.clone(),
                    effective_enrollment: None,
                    phase: Phase::Intent,
                };
                self.publish_intent(&receipt)?;
                let job = systemd.submit()?;
                receipt.phase = Phase::Submitted { job, process: None };
                self.publish(&receipt)?;
                receipt
            }
        };
        self.progress_drain(None);
        let mut progress = WaitForProgress::new(Instant::now());
        loop {
            let observation = systemd.observe()?;
            let absence = observation.successfully_inactive() && systemd.no_managed_lifecycle();
            match receipt.decide_stop(&observation, absence)? {
                StopDecision::Complete => {
                    if receipt.phase != Phase::Drained {
                        receipt.phase = Phase::Drained;
                        self.publish(&receipt)?;
                    }
                    return Ok(DrainOutcome::Drained(receipt.prior_runtime));
                }
                StopDecision::UnknownJob => {
                    return Ok(DrainOutcome::Deferred("STOP_REPLY_NOT_RECORDED".to_owned()));
                }
                StopDecision::Observe { job, process } => {
                    if let Some(process) = process {
                        let bound = Phase::Submitted {
                            job,
                            process: Some(process),
                        };
                        if receipt.phase != bound {
                            receipt.phase = bound;
                            self.publish(&receipt)?;
                        }
                        let sample = systemd.progress();
                        let owned = progress.observe(Instant::now(), process, sample.as_ref());
                        self.progress_drain(owned);
                    } else {
                        self.progress_drain(None);
                    }
                    let now = Instant::now();
                    if progress.expired(now, systemd.observation_window(), STALL_WINDOW) {
                        return Ok(DrainOutcome::Deferred(format!("DRAIN_PENDING_JOB_{job}")));
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum StopDecision {
    Complete,
    UnknownJob,
    Observe {
        job: NonZeroU32,
        process: Option<StopProcess>,
    },
}

impl Receipt {
    fn check_identity(&self, inputs: &DrainInputs, boot_id: &str) -> Result<(), CustomerError> {
        if self.inputs != *inputs || self.boot_id != boot_id {
            return Err(recovery_refusal(
                "upgrade drain belongs to a different signed input or boot",
            ));
        }
        Ok(())
    }

    fn decide_stop(
        &self,
        observation: &ObservedUnit,
        no_managed_lifecycle: bool,
    ) -> Result<StopDecision, CustomerError> {
        if observation.successfully_inactive() && no_managed_lifecycle {
            return Ok(StopDecision::Complete);
        }
        if self.phase == Phase::Intent && observation.job.is_some() {
            return Ok(StopDecision::UnknownJob);
        }
        if let Phase::Submitted { job, process } = self.phase
            && observation.is_stop_job(job)
        {
            let observed = observation.process;
            if let (Some(expected), Some(actual)) = (process, observed)
                && expected != actual
            {
                return Err(recovery_refusal(
                    "systemd stop control process identity changed",
                ));
            }
            return Ok(StopDecision::Observe {
                job,
                process: observed,
            });
        }
        Err(recovery_refusal(
            "upgrade drain has neither its exact live stop nor verified successful quiescence",
        ))
    }
}

#[cfg(test)]
mod tests;
