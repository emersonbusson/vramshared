// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/rebalance/selector.rs
//! Operator target selector for the fallback rebalance.
//!
//! The CLI parser already refuses a malformed shape with exit 2; these are the
//! defensive typed checks the action repeats before it observes anything.

use super::*;

/// Parse the operator selector shape. The CLI parser already guarantees exit 2
/// for a malformed shape; this is the defensive second check and returns a
/// typed argument error.
pub(super) fn parse_target_selector(value: &str) -> Result<TargetSelector, CascadeError> {
    if let Some(uuid) = value.strip_prefix("UUID=") {
        let uuid = canonical_swap_uuid(uuid).ok_or_else(|| {
            CascadeError::Arg(format!("--target: malformed UUID selector: {value}"))
        })?;
        return Ok(TargetSelector::Uuid(uuid));
    }
    canonical_device_path(value)
        .map(TargetSelector::Path)
        .ok_or_else(|| CascadeError::Arg(format!("--target: malformed device path: {value}")))
}

/// Canonical lowercase `8-4-4-4-12` UUID, or `None` for any other shape.
pub(super) fn canonical_swap_uuid(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.len() != 36 {
        return None;
    }
    for (index, byte) in lower.bytes().enumerate() {
        let separator = matches!(index, 8 | 13 | 18 | 23);
        if separator {
            if byte != b'-' {
                return None;
            }
        } else if !byte.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(lower)
}

/// An absolute `/dev/...` path with no traversal segment. The path is kept as
/// typed; the observation canonicalizes it against `/proc/swaps`.
fn canonical_device_path(value: &str) -> Option<String> {
    let value = value.trim();
    if !value.starts_with("/dev/") || value.len() <= "/dev/".len() {
        return None;
    }
    if value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let rest = value.strip_prefix('/')?;
    if rest
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return None;
    }
    Some(value.to_string())
}
