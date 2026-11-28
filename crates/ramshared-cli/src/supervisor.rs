// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/supervisor.rs
//! Preventive control-plane pressure state machine.

pub(crate) mod memory_policy;

mod actions;
mod runtime;
use crate::bounded_process;
use crate::published_budget::read_status_document;
use crate::workload::{self, OwnerIdentity};
#[cfg_attr(not(test), allow(unused_imports))]
use actions::*;
pub(crate) use runtime::parse_psi_full_avg10;
pub use runtime::run;
#[cfg_attr(not(test), allow(unused_imports))]
use runtime::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::ops::Deref;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONTROL_REQUEST_MAX_AGE_MS: u64 = 15_000;
const SYSTEMCTL_IDENTITY_TIMEOUT: Duration = Duration::from_secs(1);
const EMERGENCY_TERM_GRACE_MS: u64 = 5_000;
const ACTION_ERROR_MAX_BYTES: usize = 1_024;
const ACTION_ERROR_TRUNCATION_MARKER: &str = " [truncated]";
static RUNTIME_FILE_NONCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
type RuntimeWriteHook = Box<dyn Fn(&str, &serde_json::Value) -> Result<(), String>>;

#[cfg(test)]
thread_local! {
    static RUNTIME_WRITE_HOOK: std::cell::RefCell<Option<RuntimeWriteHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn invoke_runtime_write_hook(name: &str, value: &serde_json::Value) -> Result<(), String> {
    RUNTIME_WRITE_HOOK.with(|hook| match hook.borrow().as_ref() {
        Some(hook) => hook(name, value),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn invoke_runtime_write_hook(_name: &str, _value: &serde_json::Value) -> Result<(), String> {
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SupervisorState {
    Healthy,
    Guarded,
    Critical,
    Emergency,
}

impl SupervisorState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "HEALTHY",
            Self::Guarded => "GUARDED",
            Self::Critical => "CRITICAL",
            Self::Emergency => "EMERGENCY",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PressureSample {
    pub memory_available_bytes: u64,
    pub control_reserve_bytes: u64,
    pub psi_full_avg10: f64,
    pub sample_delay_ms: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorAction {
    CloseAdmission,
    ReduceVramCache,
    FreezeDiscardable,
    ThawDiscardable,
    RequestReclaim,
    TerminateDiscardable,
    KillDiscardable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorActionStatus {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisorActionError(String);

impl SupervisorActionError {
    fn new(raw: String) -> Self {
        let content_limit = ACTION_ERROR_MAX_BYTES - ACTION_ERROR_TRUNCATION_MARKER.len();
        let mut sanitized = String::with_capacity(raw.len().min(ACTION_ERROR_MAX_BYTES));
        let mut truncated = false;
        for character in raw.chars() {
            let character = if character.is_control() {
                ' '
            } else {
                character
            };
            if sanitized.len() + character.len_utf8() > content_limit {
                truncated = true;
                break;
            }
            sanitized.push(character);
        }
        if truncated {
            sanitized.push_str(ACTION_ERROR_TRUNCATION_MARKER);
        }
        Self(sanitized)
    }

    fn is_canonical(value: &str) -> bool {
        value.len() <= ACTION_ERROR_MAX_BYTES
            && !value.chars().any(char::is_control)
            && Self::new(value.to_string()).0 == value
    }
}

impl Deref for SupervisorActionError {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for SupervisorActionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for SupervisorActionError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SupervisorActionError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if Self::is_canonical(&value) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "supervisor action error is not canonical",
            ))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SupervisorActionResult {
    pub action: SupervisorAction,
    pub status: SupervisorActionStatus,
    pub error: Option<SupervisorActionError>,
}

impl SupervisorActionResult {
    fn from_result(action: SupervisorAction, result: Result<(), String>) -> Self {
        match result {
            Ok(()) => Self {
                action,
                status: SupervisorActionStatus::Succeeded,
                error: None,
            },
            Err(error) => Self {
                action,
                status: SupervisorActionStatus::Failed,
                error: Some(SupervisorActionError::new(error)),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisorDecision {
    pub state: SupervisorState,
    pub actions: Vec<SupervisorAction>,
    pub healthy_samples: u32,
    pub cache_recovery_samples: u32,
}

#[derive(Debug)]
pub struct Supervisor {
    state: SupervisorState,
    guarded_psi_samples: u32,
    critical_psi_samples: u32,
    emergency_psi_samples: u32,
    healthy_samples: u32,
    cache_recovery_samples: u32,
    emergency_kill_eligible_ms: Option<u64>,
    emergency_kill_sent: bool,
    discardable_frozen: bool,
    discardable_freeze_pending: bool,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self {
            state: SupervisorState::Healthy,
            guarded_psi_samples: 0,
            critical_psi_samples: 0,
            emergency_psi_samples: 0,
            healthy_samples: 0,
            cache_recovery_samples: 0,
            emergency_kill_eligible_ms: None,
            emergency_kill_sent: false,
            discardable_frozen: false,
            discardable_freeze_pending: false,
        }
    }
}

/// Whether a ledger publication failure must end the control plane.
///
/// This is the supervisor's lifetime policy over the busy signature that
/// [`workload::is_ledger_busy`] classifies. That one signature is transient:
/// another ledger actor holds the transition lock for a bounded critical
/// section and releases it. Ending the process there only crash-loops
/// `Restart=on-failure` under ordinary reservation churn and resets the
/// escalation state the recovery window is accumulating. Every other ledger
/// error — identity, path, I/O — is deterministic and must fail fast so the
/// fault surfaces instead of being retried forever.
fn ledger_failure_ends_control_plane(error: &str) -> bool {
    !workload::is_ledger_busy(error)
}

impl Supervisor {
    /// The supervisor a replacing control plane should start from.
    ///
    /// [`Supervisor::default`] is the fresh, never-observed case: a process
    /// with no memory of prior pressure, so a healthy host may open admission
    /// on the first sample. A restart is not that case. If the last published
    /// control state is still fresh and not `HEALTHY`, the process just
    /// replaced a supervisor that was already closing admission; adopting
    /// `Healthy` here would reopen on the first healthy sample and skip the
    /// 60-sample recovery window entirely. Start from `Guarded` instead, so
    /// admission stays closed while that window accumulates. A stale or
    /// absent record — a real reboot, or a clean first start — falls back to
    /// [`Supervisor::default`].
    fn resuming_from_prior_state(state_path: &Path, now_ms: u64) -> Self {
        match workload::last_fresh_control_state_at(state_path, now_ms) {
            Some(control_state) if control_state != SupervisorState::Healthy.as_str() => Self {
                state: SupervisorState::Guarded,
                ..Self::default()
            },
            _ => Self::default(),
        }
    }

    pub fn observe(&mut self, sample: PressureSample) -> SupervisorDecision {
        self.guarded_psi_samples = streak(
            self.guarded_psi_samples,
            sample.psi_full_avg10 >= memory_policy::GUARDED_PSI_FULL_AVG10_PERCENT,
        );
        self.critical_psi_samples = streak(self.critical_psi_samples, sample.psi_full_avg10 >= 5.0);
        self.emergency_psi_samples =
            streak(self.emergency_psi_samples, sample.psi_full_avg10 >= 10.0);

        let emergency = sample.memory_available_bytes
            < sample.control_reserve_bytes.saturating_div(2)
            || self.emergency_psi_samples >= 5
            || sample.sample_delay_ms >= 3_000;
        let critical = sample.memory_available_bytes
            < sample
                .control_reserve_bytes
                .saturating_mul(3)
                .saturating_div(4)
            || self.critical_psi_samples >= 5;
        let guarded = sample.memory_available_bytes < sample.control_reserve_bytes
            || self.guarded_psi_samples >= 3;
        let calm = sample.psi_full_avg10 < 1.0 && sample.sample_delay_ms < 3_000;
        let fully_healthy = calm
            && sample.memory_available_bytes
                >= sample
                    .control_reserve_bytes
                    .saturating_add(memory_policy::RECOVERY_MARGIN_BYTES);

        let requested = if emergency {
            SupervisorState::Emergency
        } else if critical {
            SupervisorState::Critical
        } else if guarded {
            SupervisorState::Guarded
        } else {
            SupervisorState::Healthy
        };

        self.cache_recovery_samples = streak(
            self.cache_recovery_samples,
            requested == SupervisorState::Healthy
                && calm
                && sample.memory_available_bytes
                    >= sample
                        .control_reserve_bytes
                        .saturating_add(memory_policy::CACHE_RECOVERY_MARGIN_BYTES),
        );
        let previous_state = self.state;
        if requested == SupervisorState::Healthy {
            self.healthy_samples = streak(self.healthy_samples, fully_healthy);
            if previous_state == SupervisorState::Healthy || self.healthy_samples >= 60 {
                self.state = SupervisorState::Healthy;
            } else {
                // Keep admission closed while the 60-second recovery window is
                // accumulating, but stop destructive actions immediately.
                self.state = SupervisorState::Guarded;
            }
        } else {
            self.healthy_samples = 0;
            self.state = requested;
        }

        if requested != SupervisorState::Emergency {
            self.emergency_kill_eligible_ms = None;
            self.emergency_kill_sent = false;
        }

        let mut actions = Vec::new();
        if (self.discardable_frozen || self.discardable_freeze_pending)
            && self.state != SupervisorState::Critical
        {
            actions.push(SupervisorAction::ThawDiscardable);
        }
        match self.state {
            SupervisorState::Healthy => {}
            SupervisorState::Guarded => actions.push(SupervisorAction::CloseAdmission),
            SupervisorState::Critical => {
                // Keep the Tier 2 VRAM cache active while guest RAM is under
                // CRITICAL pressure so all tiers absorb swap load together;
                // GPU VRAM pressure is independently guarded by WDDM inside
                // the worker, and guest RAM revocation is reserved for
                // EMERGENCY control-plane starvation.
                actions.push(SupervisorAction::CloseAdmission);
                if !self.discardable_frozen {
                    actions.push(SupervisorAction::FreezeDiscardable);
                }
                actions.push(SupervisorAction::RequestReclaim);
            }
            SupervisorState::Emergency => {
                actions.extend([
                    SupervisorAction::CloseAdmission,
                    SupervisorAction::ReduceVramCache,
                    SupervisorAction::RequestReclaim,
                ]);
                if self
                    .emergency_kill_eligible_ms
                    .is_some_and(|eligible| sample.elapsed_ms >= eligible)
                {
                    if !self.emergency_kill_sent {
                        actions.push(SupervisorAction::KillDiscardable);
                    }
                } else if self.emergency_kill_eligible_ms.is_none() {
                    actions.push(SupervisorAction::TerminateDiscardable);
                }
            }
        }

        SupervisorDecision {
            state: self.state,
            actions,
            healthy_samples: self.healthy_samples,
            cache_recovery_samples: self.cache_recovery_samples,
        }
    }

    fn commit_action_results(
        &mut self,
        action_results: &[SupervisorActionResult],
        elapsed_ms: u64,
    ) {
        for result in action_results {
            if result.status != SupervisorActionStatus::Succeeded {
                continue;
            }
            match result.action {
                SupervisorAction::FreezeDiscardable => {
                    self.discardable_frozen = true;
                    self.discardable_freeze_pending = false;
                }
                SupervisorAction::ThawDiscardable => {
                    self.discardable_frozen = false;
                    self.discardable_freeze_pending = false;
                }
                SupervisorAction::TerminateDiscardable => {
                    self.emergency_kill_eligible_ms =
                        Some(elapsed_ms.saturating_add(EMERGENCY_TERM_GRACE_MS));
                }
                SupervisorAction::KillDiscardable => self.emergency_kill_sent = true,
                SupervisorAction::CloseAdmission
                | SupervisorAction::ReduceVramCache
                | SupervisorAction::RequestReclaim => {}
            }
        }
    }

    fn recover_action_state(
        &mut self,
        runtime: &Path,
        now_unix_ms: u64,
        elapsed_ms: u64,
    ) -> Result<(), String> {
        match read_frozen_target(runtime)? {
            Some((FrozenPhase::Pending, _)) => {
                self.discardable_frozen = false;
                self.discardable_freeze_pending = true;
            }
            Some((FrozenPhase::Applied, _)) => {
                self.discardable_frozen = true;
                self.discardable_freeze_pending = false;
            }
            None => {
                self.discardable_frozen = false;
                self.discardable_freeze_pending = false;
            }
        }
        match read_emergency_target(runtime)? {
            Some(record) if record.phase == EmergencyPhase::Termed => {
                let term_time = record
                    .term_succeeded_at_unix_ms
                    .ok_or("durable TERM record omitted its successful time")?;
                if term_time > now_unix_ms {
                    return Err("durable TERM record is from the future".into());
                }
                let age = now_unix_ms.saturating_sub(term_time);
                self.emergency_kill_eligible_ms =
                    Some(elapsed_ms.saturating_add(EMERGENCY_TERM_GRACE_MS.saturating_sub(age)));
                self.emergency_kill_sent = false;
            }
            Some(record) if record.phase == EmergencyPhase::Killed => {
                self.emergency_kill_eligible_ms = Some(elapsed_ms);
                self.emergency_kill_sent = true;
            }
            Some(_) | None => {
                self.emergency_kill_eligible_ms = None;
                self.emergency_kill_sent = false;
            }
        }
        Ok(())
    }
}

fn streak(current: u32, condition: bool) -> u32 {
    if condition {
        current.saturating_add(1)
    } else {
        0
    }
}

pub fn discard_priority(class: &str) -> Option<u8> {
    match class {
        "batch" => Some(0),
        "browser-test" => Some(1),
        "build" => Some(2),
        "interactive" => Some(3),
        _ => None,
    }
}

/// The one definition of a canonical managed-workload scope name.
///
/// Both the reservation table and the supervisor's victim selection ask this
/// question, and they must agree: a name one side accepts and the other
/// rejects is how a row becomes a phantom victim.
pub(crate) fn valid_scope_unit(unit: &str) -> bool {
    unit.starts_with("ramshared-")
        && unit.ends_with(".scope")
        && unit
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@'))
}

#[cfg(test)]
#[path = "supervisor/tests.rs"]
mod tests;
