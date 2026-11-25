// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/diagnostics.rs
//! Bounded local diagnostic export (ITEM-4, DT-10).
//!
//! The collector takes only allowlisted scalar observations from a
//! [`DiagnosticSource`]. It cannot read origin or swap bytes, open a device,
//! or run a command: there is no path through it that does. Values that look
//! like secrets or kernel addresses are redacted before they enter the bundle,
//! and both the collection deadline and the bundle-size bound are reported as
//! explicit truncation rather than silently dropped.
#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

/// Hard ceiling on one exported bundle (SPEC: `diagnose export` ≤16 MiB).
pub(crate) const MAX_BUNDLE_BYTES: usize = 16 * 1024 * 1024;

/// Hard ceiling on one collection pass (SPEC: 30 s total).
pub(crate) const COLLECTION_DEADLINE: Duration = Duration::from_secs(30);

/// Replacement for any value that must not leave the machine.
pub(crate) const REDACTED: &str = "[redacted]";

/// Per-collection bounds. Defaults are the SPEC limits; a caller may tighten
/// them (tests do) but [`DiagnosticLimits::clamp`] refuses to loosen them past
/// the product ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticLimits {
    pub(crate) max_bundle_bytes: usize,
    pub(crate) deadline: Duration,
    pub(crate) max_value_bytes: usize,
}

impl Default for DiagnosticLimits {
    fn default() -> Self {
        Self {
            max_bundle_bytes: MAX_BUNDLE_BYTES,
            deadline: COLLECTION_DEADLINE,
            max_value_bytes: 1024,
        }
    }
}

impl DiagnosticLimits {
    /// Never exceed the product ceiling, whatever the caller asked for.
    pub(crate) fn clamp(self) -> Self {
        Self {
            max_bundle_bytes: self.max_bundle_bytes.min(MAX_BUNDLE_BYTES),
            deadline: self.deadline.min(COLLECTION_DEADLINE),
            max_value_bytes: self.max_value_bytes,
        }
    }
}

/// One allowlisted observation. The name is static so a source cannot invent a
/// field the export contract has not named.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticEntry {
    pub(crate) name: &'static str,
    pub(crate) value: String,
    /// Freshness of the underlying observation, when the source knows it.
    pub(crate) age_ms: Option<u64>,
}

/// A source of allowlisted observations. Implementations report what they
/// already know; they never open devices, read origin/swap payloads, spawn
/// processes or touch the network.
pub(crate) trait DiagnosticSource {
    fn observe(&self, deadline: Instant) -> Vec<DiagnosticEntry>;
}

/// Read-only readiness observations for `diagnose export`.
///
/// Names are fixed literals, so this source cannot invent a field the export
/// contract has not named. Swap and origin **payloads** are structurally out
/// of reach: only counts, kinds and the readiness reason are taken. Host and
/// user identifiers are never exported (SPEC data model).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReadinessDiagnosticSource;

impl DiagnosticSource for ReadinessDiagnosticSource {
    fn observe(&self, _deadline: Instant) -> Vec<DiagnosticEntry> {
        let report = crate::readiness::run_check();
        let mut entries = vec![
            entry("readiness_decision", report.decision().as_str(), None),
            entry(
                "platform",
                report.platform.platform.as_str(|k| k.as_str()),
                None,
            ),
            entry("transport", report.surface.transport.as_str(), None),
            entry(
                "gpu_backend",
                report.surface.gpu_backend.as_str(|k| k.as_str()),
                None,
            ),
            entry(
                "capacity_source",
                report.capacity.source.as_str(),
                report.capacity.age_ms,
            ),
        ];
        match report.capacity.effective_bytes {
            Some(bytes) => entries.push(entry(
                "capacity_effective_mib",
                &(bytes / (1024 * 1024)).to_string(),
                report.capacity.age_ms,
            )),
            // Unknown, never a fabricated zero (DT-7 / DT-10).
            None => entries.push(entry(
                "capacity_effective_mib",
                "unknown",
                report.capacity.age_ms,
            )),
        }
        entries.push(entry(
            "readiness_reason",
            &if report.blockers.is_empty() {
                "ready".to_string()
            } else {
                report.blockers.join("; ")
            },
            None,
        ));
        // `unknown` is a table that could not be read; `0` is a measured empty
        // one. Publishing the length either way would claim a count the probe
        // did not take.
        let swap_entry_count = match &report.swaps {
            crate::readiness::Observation::Unknown => "unknown".to_string(),
            crate::readiness::Observation::Measured(swaps) => swaps.len().to_string(),
        };
        entries.push(entry("swap_entry_count", &swap_entry_count, None));
        entries
    }
}

fn entry(name: &'static str, value: &str, age_ms: Option<u64>) -> DiagnosticEntry {
    DiagnosticEntry {
        name,
        value: value.to_string(),
        age_ms,
    }
}

/// Why an entry's value is not in the bundle verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValueDisposition {
    /// Carried through unchanged.
    AsReported,
    /// Looked like a secret or a kernel address and was replaced.
    Redacted,
    /// Longer than `max_value_bytes` and cut at that bound.
    ValueTruncated,
}

/// What the collection did to fit the limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Truncation {
    None,
    /// Stopped at the bundle-size bound.
    Size,
    /// Stopped at the collection deadline.
    Deadline,
}

/// A bounded, redacted, local bundle. Truncation is a reported fact, never a
/// silent drop, so a reader can tell a short answer from an incomplete one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticBundle {
    pub(crate) entries: Vec<DiagnosticEntry>,
    pub(crate) dispositions: Vec<ValueDisposition>,
    pub(crate) truncation: Truncation,
    pub(crate) deadline_exceeded: bool,
    pub(crate) total_bytes: usize,
    pub(crate) dropped_observations: usize,
}

impl DiagnosticBundle {
    /// JSON body of the export. Stable field names; no host or user identity.
    pub(crate) fn to_json(&self) -> String {
        let entries: Vec<String> = self
            .entries
            .iter()
            .zip(self.dispositions.iter())
            .map(|(entry, disposition)| {
                format!(
                    "{{\"name\":\"{}\",\"value\":\"{}\",\"age_ms\":{},\"disposition\":\"{}\"}}",
                    entry.name,
                    json_escape(&entry.value),
                    match entry.age_ms {
                        Some(age) => age.to_string(),
                        None => "null".to_string(),
                    },
                    disposition.as_str(),
                )
            })
            .collect();
        format!(
            "{{\"schema\":\"ramshared-diagnostics/v1\",\"truncation\":\"{}\",\"deadline_exceeded\":{},\"total_bytes\":{},\"dropped_observations\":{},\"entries\":[{}]}}",
            self.truncation.as_str(),
            self.deadline_exceeded,
            self.total_bytes,
            self.dropped_observations,
            entries.join(","),
        )
    }
}

impl ValueDisposition {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AsReported => "as_reported",
            Self::Redacted => "redacted",
            Self::ValueTruncated => "value_truncated",
        }
    }
}

impl Truncation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Size => "size",
            Self::Deadline => "deadline",
        }
    }
}

/// True when the value must not leave the machine: a secret-shaped name, or a
/// canonical kernel virtual address (KASLR material).
pub(crate) fn is_sensitive(name: &str, value: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    const SECRET_FRAGMENTS: [&str; 6] = [
        "secret",
        "token",
        "password",
        "credential",
        "private_key",
        "api_key",
    ];
    if SECRET_FRAGMENTS
        .iter()
        .any(|fragment| lowered.contains(fragment))
    {
        return true;
    }
    looks_like_kernel_address(value)
}

/// Canonical kernel virtual addresses are `0xffff…`. A user pointer is not.
fn looks_like_kernel_address(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut idx = 0;
    while idx + 6 <= bytes.len() {
        let window = &value[idx..idx + 6];
        if window.eq_ignore_ascii_case("0xffff") {
            return true;
        }
        idx += 1;
    }
    false
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Collect a bounded, redacted bundle from `source`.
///
/// The deadline is checked between entries and again before each value is
/// admitted, so a source that yields a huge first entry cannot blow past the
/// bound by itself. Everything the collector refuses is counted, not hidden.
pub(crate) fn collect_diagnostics(
    source: &(impl DiagnosticSource + ?Sized),
    limits: DiagnosticLimits,
) -> DiagnosticBundle {
    let limits = limits.clamp();
    let started = Instant::now();
    let deadline = started + limits.deadline;

    let mut bundle = DiagnosticBundle {
        entries: Vec::new(),
        dispositions: Vec::new(),
        truncation: Truncation::None,
        deadline_exceeded: false,
        total_bytes: 0,
        dropped_observations: 0,
    };
    let mut size_truncated = false;

    let observed = source.observe(deadline);

    for entry in observed {
        if Instant::now() >= deadline {
            bundle.deadline_exceeded = true;
            bundle.dropped_observations = bundle.dropped_observations.saturating_add(1);
            continue;
        }

        let sensitive = is_sensitive(entry.name, &entry.value);
        let value = if sensitive {
            REDACTED.to_string()
        } else if entry.value.len() > limits.max_value_bytes {
            entry.value[..limits.max_value_bytes].to_string()
        } else {
            entry.value.clone()
        };
        let disposition = if sensitive {
            ValueDisposition::Redacted
        } else if entry.value.len() > limits.max_value_bytes {
            ValueDisposition::ValueTruncated
        } else {
            ValueDisposition::AsReported
        };

        let admitted_bytes = entry.name.len() + value.len();
        if bundle.total_bytes.saturating_add(admitted_bytes) > limits.max_bundle_bytes {
            size_truncated = true;
            bundle.dropped_observations = bundle.dropped_observations.saturating_add(1);
            continue;
        }

        bundle.total_bytes = bundle.total_bytes.saturating_add(admitted_bytes);
        bundle.entries.push(DiagnosticEntry {
            name: entry.name,
            value,
            age_ms: entry.age_ms,
        });
        bundle.dispositions.push(disposition);
    }

    // Derived once from what actually happened, so a run that hit both limits
    // reports the deadline (the stronger claim: the pass did not finish) and
    // never leaves the flags and the summary in disagreement.
    bundle.truncation = match (bundle.deadline_exceeded, size_truncated) {
        (false, false) => Truncation::None,
        (true, _) => Truncation::Deadline,
        (false, true) => Truncation::Size,
    };
    bundle
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::Mutex;

    struct FixedSource {
        entries: Vec<DiagnosticEntry>,
        delay: Duration,
        observed: Mutex<usize>,
    }

    impl DiagnosticSource for FixedSource {
        fn observe(&self, _deadline: Instant) -> Vec<DiagnosticEntry> {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            *self.observed.lock().unwrap() += 1;
            self.entries.clone()
        }
    }

    fn entry(name: &'static str, value: &str) -> DiagnosticEntry {
        DiagnosticEntry {
            name,
            value: value.to_string(),
            age_ms: Some(12),
        }
    }

    #[test]
    fn export_redacts_secrets_and_marks_deadline_and_size_truncation() {
        // Secrets and kernel addresses never leave, whatever their name.
        let source = FixedSource {
            entries: vec![
                entry("release_identity", "ramshared 0.15.0"),
                entry("api_key", "AKIA-not-a-real-key"),
                entry("budget_pointer", "stale at 0xffff888012345678"),
                entry("bootslot", "healthy"),
            ],
            delay: Duration::ZERO,
            observed: Mutex::new(0),
        };
        let bundle = collect_diagnostics(
            &source,
            DiagnosticLimits {
                max_bundle_bytes: 4096,
                deadline: Duration::from_secs(1),
                max_value_bytes: 64,
            },
        );
        let by_name = |name: &str| {
            bundle
                .entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("missing entry {name}"))
        };
        assert_eq!(by_name("release_identity").value, "ramshared 0.15.0");
        assert_eq!(by_name("api_key").value, REDACTED);
        assert!(by_name("budget_pointer").value.contains(REDACTED));
        assert!(bundle.dispositions.contains(&ValueDisposition::Redacted));
        assert!(!bundle.to_json().contains("AKIA-not-a-real-key"));
        assert!(!bundle.to_json().contains("0xffff888012345678"));

        // A value past the per-field bound is cut and marked, not dropped.
        let long = "x".repeat(200);
        let size_source = FixedSource {
            entries: vec![entry("kernel_release", &long)],
            delay: Duration::ZERO,
            observed: Mutex::new(0),
        };
        let sized = collect_diagnostics(
            &size_source,
            DiagnosticLimits {
                max_bundle_bytes: 4096,
                deadline: Duration::from_secs(1),
                max_value_bytes: 16,
            },
        );
        assert_eq!(sized.entries[0].value.len(), 16);
        assert!(
            sized
                .dispositions
                .contains(&ValueDisposition::ValueTruncated)
        );

        // A source that outlives the deadline is marked, and the remaining
        // observations are counted as dropped rather than silently lost.
        let slow_source = FixedSource {
            entries: vec![
                entry("first", "a"),
                entry("second", "b"),
                entry("third", "c"),
            ],
            delay: Duration::from_millis(80),
            observed: Mutex::new(0),
        };
        let slow = collect_diagnostics(
            &slow_source,
            DiagnosticLimits {
                max_bundle_bytes: 4096,
                deadline: Duration::from_millis(50),
                max_value_bytes: 64,
            },
        );
        assert!(slow.deadline_exceeded);
        assert_eq!(slow.truncation, Truncation::Deadline);
        assert!(slow.dropped_observations >= 1);

        // A bundle-size bound stops admission and says so.
        let wide_source = FixedSource {
            entries: vec![
                entry("padding0", &"p".repeat(64)),
                entry("padding1", &"p".repeat(64)),
                entry("padding2", &"p".repeat(64)),
                entry("padding3", &"p".repeat(64)),
                entry("marker", &"q".repeat(64)),
            ],
            delay: Duration::ZERO,
            observed: Mutex::new(0),
        };
        let wide = collect_diagnostics(
            &wide_source,
            DiagnosticLimits {
                max_bundle_bytes: 200,
                deadline: Duration::from_secs(1),
                max_value_bytes: 64,
            },
        );
        assert_eq!(wide.truncation, Truncation::Size);
        assert!(wide.dropped_observations >= 1);
        assert!(wide.total_bytes <= 200);
    }

    #[test]
    fn limits_cannot_loosen_the_product_ceiling() {
        let loose = DiagnosticLimits {
            max_bundle_bytes: MAX_BUNDLE_BYTES * 4,
            deadline: COLLECTION_DEADLINE * 4,
            max_value_bytes: 4096,
        }
        .clamp();
        assert_eq!(loose.max_bundle_bytes, MAX_BUNDLE_BYTES);
        assert_eq!(loose.deadline, COLLECTION_DEADLINE);
    }

    #[test]
    fn kernel_address_detection_is_narrow() {
        assert!(looks_like_kernel_address("0xffff888000000000"));
        assert!(looks_like_kernel_address("ptr=0xFFFF8880"));
        assert!(!looks_like_kernel_address("/usr/lib/libcuda.so.1"));
        assert!(!looks_like_kernel_address("0x00007f1234567890"));
        assert!(!looks_like_kernel_address("ffff"));
    }

    #[test]
    fn json_escapes_control_characters() {
        let source = FixedSource {
            entries: vec![entry("note", "line\n\"quoted\"\\slash\r\ttab\u{1}ctrl")],
            delay: Duration::ZERO,
            observed: Mutex::new(0),
        };
        let bundle = collect_diagnostics(&source, DiagnosticLimits::default());
        let json = bundle.to_json();
        assert!(json.contains("line\\n\\\"quoted\\\"\\\\slash\\r\\ttab\\u0001ctrl"));
        assert!(!json.contains('\n'));
        assert!(!json.contains('\r'));
    }

    /// The readiness source is the production allowlist: fixed names, no host
    /// or user identifier, no swap or origin payload, and an unknown capacity
    /// is reported as unknown rather than a fabricated zero.
    #[test]
    fn readiness_source_exports_only_named_allowlisted_observations() {
        let entries = ReadinessDiagnosticSource.observe(Instant::now());
        let names: Vec<&str> = entries.iter().map(|e| e.name).collect();
        for required in [
            "readiness_decision",
            "platform",
            "transport",
            "gpu_backend",
            "capacity_source",
            "capacity_effective_mib",
            "readiness_reason",
            "swap_entry_count",
        ] {
            assert!(names.contains(&required), "allowlist lost {required}");
        }
        // Host/user identity and swap payloads are structurally absent: the
        // source only ever names these literals.
        assert_eq!(names.len(), 8, "the source invented a field");
        let capacity = entries
            .iter()
            .find(|e| e.name == "capacity_effective_mib")
            .unwrap();
        // Either a real MiB figure or the explicit unknown — never "0" from a
        // missing sample (DT-7).
        if capacity.value != "unknown" {
            assert!(capacity.value.chars().all(|c| c.is_ascii_digit()));
        }
    }

    /// Every disposition and every truncation summary has a stable string, and
    /// a bundle that carried no age reports `null` rather than dropping the
    /// field. A reader must be able to tell a short answer from an incomplete
    /// one without guessing.
    #[test]
    fn bundle_json_reports_every_disposition_and_truncation() {
        assert_eq!(ValueDisposition::AsReported.as_str(), "as_reported");
        assert_eq!(ValueDisposition::Redacted.as_str(), "redacted");
        assert_eq!(ValueDisposition::ValueTruncated.as_str(), "value_truncated");
        assert_eq!(Truncation::None.as_str(), "none");
        assert_eq!(Truncation::Size.as_str(), "size");
        assert_eq!(Truncation::Deadline.as_str(), "deadline");

        let source = FixedSource {
            entries: vec![
                DiagnosticEntry {
                    name: "no_age",
                    value: "v".to_string(),
                    age_ms: None,
                },
                entry("api_key", "leak"),
                entry("long", &"z".repeat(40)),
            ],
            delay: Duration::ZERO,
            observed: Mutex::new(0),
        };
        let bundle = collect_diagnostics(
            &source,
            DiagnosticLimits {
                max_bundle_bytes: 4096,
                deadline: Duration::from_secs(1),
                max_value_bytes: 8,
            },
        );
        let json = bundle.to_json();
        assert!(json.contains("\"age_ms\":null"));
        assert!(json.contains("\"disposition\":\"as_reported\""));
        assert!(json.contains("\"disposition\":\"redacted\""));
        assert!(json.contains("\"disposition\":\"value_truncated\""));
        assert!(json.contains("\"truncation\":\"none\""));

        // A size stop is summarized as size; a deadline stop as deadline, and
        // a run that hit both reports the deadline — the pass did not finish.
        let wide = FixedSource {
            entries: vec![
                entry("a", &"p".repeat(64)),
                entry("b", &"p".repeat(64)),
                entry("c", &"p".repeat(64)),
            ],
            delay: Duration::ZERO,
            observed: Mutex::new(0),
        };
        let wide_bundle = collect_diagnostics(
            &wide,
            DiagnosticLimits {
                max_bundle_bytes: 40,
                deadline: Duration::from_secs(1),
                max_value_bytes: 64,
            },
        );
        assert_eq!(wide_bundle.truncation, Truncation::Size);
        assert!(wide_bundle.to_json().contains("\"truncation\":\"size\""));

        let slow = FixedSource {
            entries: vec![entry("a", "1"), entry("b", "2")],
            delay: Duration::from_millis(80),
            observed: Mutex::new(0),
        };
        let slow_bundle = collect_diagnostics(
            &slow,
            DiagnosticLimits {
                max_bundle_bytes: 4096,
                deadline: Duration::from_millis(20),
                max_value_bytes: 64,
            },
        );
        assert_eq!(slow_bundle.truncation, Truncation::Deadline);
        assert!(
            slow_bundle
                .to_json()
                .contains("\"truncation\":\"deadline\"")
        );
    }

    /// A secret-shaped *name* is enough: the value never has to look secret
    /// for the collector to refuse to export it.
    #[test]
    fn secret_shaped_names_are_redacted_by_name() {
        for name in [
            "session_token",
            "root_password",
            "signing_credential",
            "private_key_pem",
            "client_api_key",
            "bearer_token",
        ] {
            assert!(is_sensitive(name, "harmless"), "{name} was not redacted");
        }
        assert!(!is_sensitive("platform", "native_linux"));
    }
}
