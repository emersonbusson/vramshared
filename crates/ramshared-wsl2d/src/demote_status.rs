// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/demote_status.rs
//! Persist demote counters for CLI `status --json` (cascade-lifecycle-observability ITEM-3).
//!
//! Path: `/run/ramshared/demote-status.json` (same run dir as pid/sock).
//! Pure parse/render unit-tested; write is best-effort.

use std::fs;
use std::io;
use std::path::Path;

/// Default path next to `ramsharedd.pid` / `wsl2d.sock`.
pub const DEMOTE_STATUS_PATH: &str = "/run/ramshared/demote-status.json";

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DemoteStatusFile {
    pub total: u64,
    pub last_reason: Option<String>,
    pub in_progress: bool,
    /// Age in seconds of the `swapoff` currently in flight, once it has outlived
    /// the stuck-detection window (SPEC §9.2). `None` while nothing is pending
    /// or while a pending drain is still inside the window.
    pub pending_age_secs: Option<u64>,
}

impl DemoteStatusFile {
    /// A routine DEMOTE record: counters and state, no pending age.
    pub fn new(total: u64, last_reason: Option<String>, in_progress: bool) -> Self {
        Self {
            total,
            last_reason,
            in_progress,
            pending_age_secs: None,
        }
    }

    /// Attach the age of a drain that has outlived the stuck-detection window.
    pub fn with_pending_age_secs(mut self, secs: u64) -> Self {
        self.pending_age_secs = Some(secs);
        self
    }
}

/// Render one JSON object (no serde).
pub fn render_demote_status_json(s: &DemoteStatusFile) -> String {
    let reason = match &s.last_reason {
        Some(r) => format!("\"{}\"", json_escape_inner(r)),
        None => "null".into(),
    };
    let age = match s.pending_age_secs {
        Some(secs) => secs.to_string(),
        None => "null".into(),
    };
    format!(
        "{{\"total\":{},\"last_reason\":{},\"in_progress\":{},\"pending_age_secs\":{}}}",
        s.total, reason, s.in_progress, age
    )
}

fn json_escape_inner(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// Best-effort write; creates parent dir if needed.
pub fn write_demote_status(path: &Path, s: &DemoteStatusFile) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, render_demote_status_json(s))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Parse status file body. Tolerant of whitespace.
///
/// `total` and `in_progress` are required keys: a body without a bool this
/// face can witness is refused as a document (`None`), never parsed with a
/// substituted `false`. `false` is the measured refutation this daemon itself
/// writes; a body that never produced the key is a different fact.
pub fn parse_demote_status(text: &str) -> Option<DemoteStatusFile> {
    let t = text.trim();
    if !t.starts_with('{') {
        return None;
    }
    let total = extract_u64(t, "total")?;
    let in_progress = extract_bool(t, "in_progress")?;
    let last_reason = extract_string_or_null(t, "last_reason");
    // Absent or explicit `null` both mean "no pending age", so a status file
    // written before this field existed still parses.
    let pending_age_secs = extract_optional_u64(t, "pending_age_secs");
    Some(DemoteStatusFile {
        total,
        last_reason,
        in_progress,
        pending_age_secs,
    })
}

/// `None` for a missing key, an explicit `null`, or a non-numeric value.
fn extract_optional_u64(json: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let i = json.find(&pat)?;
    let rest = json[i + pat.len()..].trim_start();
    if rest.starts_with("null") {
        return None;
    }
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

fn extract_u64(json: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let i = json.find(&pat)?;
    let rest = json[i + pat.len()..].trim_start();
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

fn extract_bool(json: &str, key: &str) -> Option<bool> {
    let pat = format!("\"{key}\":");
    let i = json.find(&pat)?;
    let rest = json[i + pat.len()..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn extract_string_or_null(json: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let i = json.find(&pat)?;
    let rest = json[i + pat.len()..].trim_start();
    if rest.starts_with("null") {
        return None;
    }
    if !rest.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '"' => break,
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn roundtrip_render_parse() {
        let s = DemoteStatusFile {
            total: 3,
            last_reason: Some("Latency".into()),
            in_progress: true,
            pending_age_secs: Some(42),
        };
        let j = render_demote_status_json(&s);
        let p = parse_demote_status(&j).expect("parse");
        assert_eq!(p, s);
    }

    #[test]
    fn parse_null_reason() {
        let j = r#"{"total":0,"last_reason":null,"in_progress":false,"pending_age_secs":null}"#;
        let p = parse_demote_status(j).unwrap();
        assert_eq!(p.total, 0);
        assert!(p.last_reason.is_none());
        assert!(!p.in_progress);
        assert!(p.pending_age_secs.is_none());
    }

    #[test]
    fn parse_absent_pending_age_is_none() {
        // A status file written before the field existed must still parse.
        let j = r#"{"total":2,"last_reason":"WddmBudget","in_progress":true}"#;
        let p = parse_demote_status(j).expect("legacy body still parses");
        assert!(p.in_progress);
        assert!(p.pending_age_secs.is_none());
    }

    #[test]
    fn parse_never_publishes_a_measured_refutation_from_a_missing_in_progress_key() {
        // `false` is the measured refutation this daemon itself writes. A body
        // that never produced the key is a different fact and is refused as a
        // document, not parsed with a substituted `false`.
        let j = r#"{"total":2,"last_reason":"WddmBudget"}"#;
        assert_eq!(parse_demote_status(j), None);
    }

    #[test]
    fn parse_never_publishes_a_measured_refutation_from_a_non_bool_in_progress_value() {
        for body in [
            r#"{"total":2,"in_progress":null}"#,
            r#"{"total":2,"in_progress":1}"#,
            r#"{"total":2,"in_progress":"yes"}"#,
        ] {
            assert_eq!(parse_demote_status(body), None, "body {body}");
        }
    }

    #[test]
    fn parse_is_witness_backed_for_in_progress_tokens() {
        let t = parse_demote_status(r#"{"total":2,"in_progress":true}"#).expect("parse");
        assert!(t.in_progress);
        let f = parse_demote_status(r#"{"total":2,"in_progress":false}"#).expect("parse");
        assert!(!f.in_progress);
    }

    #[test]
    fn pending_age_roundtrips_zero() {
        // Zero is a real age, not "absent": the key must stay distinguishable
        // from `null`.
        let s = DemoteStatusFile {
            total: 1,
            last_reason: Some("FreeFloor".into()),
            in_progress: true,
            pending_age_secs: Some(0),
        };
        let j = render_demote_status_json(&s);
        let p = parse_demote_status(&j).expect("parse");
        assert_eq!(p.pending_age_secs, Some(0));
    }

    #[test]
    fn demote_status_fixture_parses() {
        let fixture = include_str!("../../../Documentation/ABI/fixtures/demote-status-v1.json");
        let parsed = parse_demote_status(fixture).expect("canonical fixture must parse");
        assert_eq!(parsed.total, 3);
        assert_eq!(parsed.last_reason.as_deref(), Some("Latency"));
        assert!(parsed.in_progress);
        assert_eq!(parsed.pending_age_secs, Some(42));
        assert_eq!(
            parse_demote_status(&render_demote_status_json(&parsed)),
            Some(parsed)
        );
    }

    #[test]
    fn test_write_demote_status() {
        let count = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "ramshared-demote-test-{}-{}.json",
            std::process::id(),
            count
        ));

        let s = DemoteStatusFile {
            total: 42,
            last_reason: Some("Test write".into()),
            in_progress: false,
            pending_age_secs: None,
        };

        write_demote_status(&path, &s).expect("write failed");

        assert!(path.exists(), "Final file should exist");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "Temp file should be renamed/deleted"
        );

        let content = fs::read_to_string(&path).expect("read failed");
        let parsed = parse_demote_status(&content).expect("parse failed");
        assert_eq!(parsed, s);

        // cleanup
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_write_demote_status_creates_dir() {
        let count = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let nested_dir =
            std::env::temp_dir().join(format!("ramshared-demote-nested-{}", std::process::id()));
        let path = nested_dir.join(format!("test-{}.json", count));

        let s = DemoteStatusFile {
            total: 10,
            last_reason: None,
            in_progress: true,
            pending_age_secs: Some(30),
        };

        // Ensure parent dir doesn't exist
        let _ = fs::remove_dir_all(&nested_dir);

        write_demote_status(&path, &s).expect("write failed");

        assert!(nested_dir.exists(), "Parent directory should be created");
        assert!(path.exists(), "Final file should exist");

        let content = fs::read_to_string(&path).expect("read failed");
        let parsed = parse_demote_status(&content).expect("parse failed");
        assert_eq!(parsed, s);

        // cleanup
        let _ = fs::remove_dir_all(&nested_dir);
    }
}
