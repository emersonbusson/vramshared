// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tui/handoff.rs
//! Non-authoritative display continuity carried across seamless TUI self-re-exec.

use std::collections::VecDeque;
use std::ffi::OsString;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::super::dashboard::Screen;
use super::super::i18n::Language;
use super::super::latency::TierAccumulator;
use super::super::render::DashHistory;
#[cfg(unix)]
use crate::release_coherence::PREVIOUS_RELEASE_HINT_ENV;

pub(in crate::monitor) const TUI_HANDOFF_ENV: &str = "RAMSHARED_TUI_HANDOFF_STATE";
const HANDOFF_SCHEMA_VERSION: u8 = 1;
const MAX_HANDOFF_BYTES: usize = 32_768;
const HANDOFF_MAX_HISTORY_SAMPLES: usize = 180;

/// Display-only TUI continuity snapshot transferred across an in-place update `exec`.
///
/// Mirrors `PREVIOUS_RELEASE_HINT_ENV` (SPEC DT-4 / NFR-6): never names an executable,
/// never establishes install identity, and fails closed to clean defaults if malformed
/// or oversized.
#[derive(Clone, Serialize, Deserialize)]
pub(in crate::monitor) struct TuiHandoffState {
    pub(in crate::monitor) schema_version: u8,
    pub(in crate::monitor) language: Language,
    pub(in crate::monitor) screen: Screen,
    pub(in crate::monitor) history: DashHistory,
    pub(in crate::monitor) zram_acc: TierAccumulator,
    pub(in crate::monitor) vram_acc: TierAccumulator,
    pub(in crate::monitor) disk_acc: TierAccumulator,
    pub(in crate::monitor) swap_peak_mbs: f64,
    pub(in crate::monitor) swap_read_peak_mbs: f64,
    pub(in crate::monitor) swap_write_peak_mbs: f64,
    pub(in crate::monitor) rate_windows: u64,
    pub(in crate::monitor) zram_peak_used_kib: Option<u64>,
    pub(in crate::monitor) vram_peak_used_kib: Option<u64>,
    pub(in crate::monitor) disk_peak_used_kib: Option<u64>,
}

impl Default for TuiHandoffState {
    fn default() -> Self {
        Self::with_language(Language::default())
    }
}

impl TuiHandoffState {
    pub(in crate::monitor) fn with_language(language: Language) -> Self {
        Self {
            schema_version: HANDOFF_SCHEMA_VERSION,
            language,
            screen: Screen::default(),
            history: DashHistory::default(),
            zram_acc: TierAccumulator::default(),
            vram_acc: TierAccumulator::default(),
            disk_acc: TierAccumulator::default(),
            swap_peak_mbs: 0.0,
            swap_read_peak_mbs: 0.0,
            swap_write_peak_mbs: 0.0,
            rate_windows: 0,
            zram_peak_used_kib: None,
            vram_peak_used_kib: None,
            disk_peak_used_kib: None,
        }
    }
}

fn trim_deque<T>(deque: &mut VecDeque<T>, limit: usize) {
    let excess = deque.len().saturating_sub(limit);
    if excess > 0 {
        deque.drain(..excess);
    }
}

fn clamp_history(history: &mut DashHistory, limit: usize) {
    for series in [
        &mut history.ram,
        &mut history.gpu,
        &mut history.zram_saved,
        &mut history.cache_saved,
        &mut history.cache_held,
        &mut history.cache_hit_rate,
        &mut history.cache_origin_reads,
        &mut history.cache_fill,
        &mut history.pressure_full,
    ] {
        trim_deque(series, limit);
    }
    trim_deque(&mut history.cache_active, limit);
}

impl TuiHandoffState {
    fn validated(mut self) -> Option<Self> {
        if self.schema_version != HANDOFF_SCHEMA_VERSION {
            return None;
        }
        let peaks_valid = [
            self.swap_peak_mbs,
            self.swap_read_peak_mbs,
            self.swap_write_peak_mbs,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
            && self.zram_acc.is_finite_non_negative()
            && self.vram_acc.is_finite_non_negative()
            && self.disk_acc.is_finite_non_negative();
        if !peaks_valid {
            return None;
        }
        clamp_history(&mut self.history, HANDOFF_MAX_HISTORY_SAMPLES);
        Some(self)
    }

    pub(in crate::monitor) fn encode(&self) -> Option<String> {
        let mut copy = self.clone();
        copy.schema_version = HANDOFF_SCHEMA_VERSION;
        for limit in [HANDOFF_MAX_HISTORY_SAMPLES, 32, 0] {
            clamp_history(&mut copy.history, limit);
            if let Ok(encoded) = serde_json::to_string(&copy)
                && encoded.len() <= MAX_HANDOFF_BYTES
            {
                return Some(encoded);
            }
        }
        None
    }

    pub(in crate::monitor) fn decode(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.len() > MAX_HANDOFF_BYTES {
            return None;
        }
        serde_json::from_str::<Self>(raw)
            .ok()
            .and_then(Self::validated)
    }
}

pub(in crate::monitor) fn read_handoff_from<F>(lookup: &F) -> Option<TuiHandoffState>
where
    F: Fn(&str) -> Option<OsString>,
{
    let raw = lookup(TUI_HANDOFF_ENV)?;
    let text = raw.to_str()?;
    TuiHandoffState::decode(text)
}

pub(in crate::monitor) fn read_handoff_env() -> Option<TuiHandoffState> {
    read_handoff_from(&|key| std::env::var_os(key))
}

/// Drops terminal raw mode before an in-place `exec` without leaving the
/// alternate screen, preventing a shell scrollback flash between frames.
pub(in crate::monitor) fn leave_raw_mode_for_handoff() {
    let _ = ratatui::crossterm::terminal::disable_raw_mode();
}

#[cfg(all(unix, test))]
pub(in crate::monitor) fn reexec_command(
    executable: &Path,
    argv0: &std::ffi::OsStr,
    args: &[OsString],
    previous_release: Option<&str>,
) -> Command {
    reexec_command_with_handoff(executable, argv0, args, previous_release, None)
}

#[cfg(unix)]
pub(in crate::monitor) fn reexec_command_with_handoff(
    executable: &Path,
    argv0: &std::ffi::OsStr,
    args: &[OsString],
    previous_release: Option<&str>,
    handoff: Option<&TuiHandoffState>,
) -> Command {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(executable);
    command.arg0(argv0).args(args);
    if let Some(from) = previous_release {
        // Display-only (SPEC DT-4 / NFR-6). The next process reads it to name
        // the transition; it cannot choose which executable runs.
        command.env(PREVIOUS_RELEASE_HINT_ENV, from);
    }
    if let Some(encoded) = handoff.and_then(TuiHandoffState::encode) {
        // Display-only UI continuity (language, active view, bounded chart
        // history). Never influences target selection or identity.
        command.env(TUI_HANDOFF_ENV, encoded);
    }
    command
}

#[cfg(unix)]
pub(in crate::monitor) fn exec_restarted_process(
    executable: &Path,
    previous_release: Option<&str>,
    handoff: Option<&TuiHandoffState>,
) -> Result<(), std::io::Error> {
    use std::os::unix::process::CommandExt;

    let mut arguments = std::env::args_os();
    let argv0 = arguments
        .next()
        .unwrap_or_else(|| executable.as_os_str().to_os_string());
    let args = arguments.collect::<Vec<_>>();
    Err(reexec_command_with_handoff(executable, &argv0, &args, previous_release, handoff).exec())
}

#[cfg(not(unix))]
pub(in crate::monitor) fn exec_restarted_process(
    _executable: &std::path::Path,
    _previous_release: Option<&str>,
    _handoff: Option<&TuiHandoffState>,
) -> Result<(), std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "automatic re-exec is supported only on Unix hosts",
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn roundtrip_preserves_language_screen_history_and_clamps_trailing_samples() {
        let mut state = TuiHandoffState {
            language: Language::PtBr,
            screen: Screen::Pressure,
            swap_peak_mbs: 42.5,
            swap_read_peak_mbs: 12.25,
            swap_write_peak_mbs: 30.25,
            rate_windows: 9,
            zram_peak_used_kib: Some(1_048_576),
            vram_peak_used_kib: Some(524_288),
            disk_peak_used_kib: None,
            ..Default::default()
        };
        state.zram_acc.record(10.0, 20.0);
        for sample in 0..250u64 {
            state.history.ram.push_back(sample);
            state.history.cache_active.push_back(sample % 2 == 0);
        }

        let encoded = state.encode().expect("encoded handoff");
        assert!(encoded.len() <= MAX_HANDOFF_BYTES);

        let decoded = read_handoff_from(&|key| {
            (key == TUI_HANDOFF_ENV).then(|| OsString::from(encoded.clone()))
        })
        .expect("decoded handoff");
        assert_eq!(decoded.language, Language::PtBr);
        assert_eq!(decoded.screen, Screen::Pressure);
        assert_eq!(decoded.swap_peak_mbs, 42.5);
        assert_eq!(decoded.rate_windows, 9);
        assert_eq!(decoded.zram_peak_used_kib, Some(1_048_576));
        assert_eq!(decoded.zram_acc.count, 1);
        assert_eq!(decoded.history.ram.len(), HANDOFF_MAX_HISTORY_SAMPLES);
        assert_eq!(decoded.history.ram.front().copied(), Some(70));
        assert_eq!(decoded.history.ram.back().copied(), Some(249));
        assert_eq!(
            decoded.history.cache_active.len(),
            HANDOFF_MAX_HISTORY_SAMPLES
        );
    }

    #[test]
    fn decode_rejects_oversized_malformed_wrong_schema_and_negative_values() {
        assert!(TuiHandoffState::decode("").is_none());
        assert!(TuiHandoffState::decode("not-json").is_none());
        let oversized = "x".repeat(MAX_HANDOFF_BYTES + 1);
        assert!(TuiHandoffState::decode(&oversized).is_none());

        let bad_schema = TuiHandoffState {
            schema_version: 99,
            ..Default::default()
        };
        let json = serde_json::to_string(&bad_schema).unwrap();
        assert!(TuiHandoffState::decode(&json).is_none());

        let negative_peak = TuiHandoffState {
            swap_peak_mbs: -1.0,
            ..Default::default()
        };
        let json = serde_json::to_string(&negative_peak).unwrap();
        assert!(TuiHandoffState::decode(&json).is_none());

        let mut negative_acc = TuiHandoffState::default();
        negative_acc.vram_acc.min_mbs = -0.1;
        let json = serde_json::to_string(&negative_acc).unwrap();
        assert!(TuiHandoffState::decode(&json).is_none());

        assert!(read_handoff_from(&|_| None).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn reexec_command_with_handoff_attaches_both_display_hints() {
        let state = TuiHandoffState {
            language: Language::PtBr,
            screen: Screen::Details,
            ..Default::default()
        };
        let command = reexec_command_with_handoff(
            Path::new("/opt/ramshared/current/bin/ramshared"),
            std::ffi::OsStr::new("ramshared"),
            &[OsString::from("top")],
            Some("v0.14.1-old"),
            Some(&state),
        );
        let envs: std::collections::BTreeMap<OsString, Option<OsString>> = command
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|val| val.to_os_string())))
            .collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new(PREVIOUS_RELEASE_HINT_ENV)),
            Some(&Some(OsString::from("v0.14.1-old")))
        );
        let handoff_raw = envs
            .get(std::ffi::OsStr::new(TUI_HANDOFF_ENV))
            .and_then(|v| v.as_ref())
            .and_then(|v| v.to_str())
            .expect("handoff env present");
        let parsed = TuiHandoffState::decode(handoff_raw).expect("valid encoded handoff");
        assert_eq!(parsed.language, Language::PtBr);
        assert_eq!(parsed.screen, Screen::Details);
    }
}
