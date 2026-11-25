// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/posture.rs
//! The posture decision and its refusal grammar (SPEC
//! wsl2-transparent-tier-service DT-1..DT-3).
//!
//! The posture is a binding on the version-scoped activation token, never a
//! second root-owned document. This module is pure: it parses the binding,
//! refuses an absent or unknown value, proves that the automatic posture has
//! the lower tier it needs, and names the activation argument. It never reads
//! a device, never starts anything, and never chooses a posture by default.

use std::fmt;

/// The two postures a token may bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    /// Lab-only: the VRAM tier is a swap device (measured parity 0.38–0.51x).
    Swap,
    /// Automatic: zram is the hot swap device and the tier is its
    /// `backing_dev`, so the tier is never in the swap table.
    Writeback,
}

impl Posture {
    /// The spelling published by `status --json`.
    pub(crate) const fn as_status(self) -> &'static str {
        match self {
            Self::Swap => "lab_swap",
            Self::Writeback => "writeback",
        }
    }
}

/// Deterministic posture refusals. The stable codes are what `status` and the
/// journal surface; the prose is only for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostureError {
    /// The token carries no `posture=` binding. Absence never chooses.
    Absent,
    /// The token binds a value outside `writeback|swap`.
    Unknown(String),
    /// `writeback` needs a non-zero zram tier to spill.
    WritebackRequiresZram,
    /// `writeback` needs a configured lower tier as zram's `backing_dev`.
    WritebackRequiresTier,
}

impl PostureError {
    /// Stable refusal code for the journal and the status document.
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::Absent => "POSTURE_ABSENT",
            Self::Unknown(_) => "POSTURE_UNKNOWN",
            Self::WritebackRequiresZram => "POSTURE_WRITEBACK_REQUIRES_ZRAM",
            Self::WritebackRequiresTier => "POSTURE_WRITEBACK_REQUIRES_TIER",
        }
    }
}

impl fmt::Display for PostureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => write!(
                f,
                "{}: the activation token carries no posture",
                self.code()
            ),
            Self::Unknown(value) => {
                write!(f, "{}: unknown posture binding {value:?}", self.code())
            }
            Self::WritebackRequiresZram => write!(
                f,
                "{}: writeback requires a positive zram size",
                self.code()
            ),
            Self::WritebackRequiresTier => write!(
                f,
                "{}: writeback requires a configured tier device",
                self.code()
            ),
        }
    }
}

impl std::error::Error for PostureError {}

/// Parse the token's `posture=` value. Strict and lowercase: an unknown
/// spelling is refused rather than corrected, and an empty value is absent.
pub(crate) fn parse_binding(raw: &str) -> Result<Posture, PostureError> {
    match raw.trim() {
        "" => Err(PostureError::Absent),
        "writeback" => Ok(Posture::Writeback),
        "swap" => Ok(Posture::Swap),
        other => Err(PostureError::Unknown(other.to_string())),
    }
}

/// Prove the posture can be activated on the resolved sizing and tier.
///
/// Only `writeback` has prerequisites; `swap` is the lab posture and keeps
/// its existing activation shape.
pub(crate) fn validate(
    posture: Posture,
    zram_mib: u64,
    tier_device: Option<&str>,
) -> Result<(), PostureError> {
    if posture != Posture::Writeback {
        return Ok(());
    }
    if zram_mib == 0 {
        return Err(PostureError::WritebackRequiresZram);
    }
    if !tier_device.is_some_and(|device| !device.trim().is_empty()) {
        return Err(PostureError::WritebackRequiresTier);
    }
    Ok(())
}

/// The activation arguments this posture adds. `swap` adds none: the tier is
/// the swap device and the existing path already does that.
pub(crate) fn activation_args(posture: Posture) -> &'static [&'static str] {
    match posture {
        Posture::Swap => &[],
        Posture::Writeback => &["--zram-writeback-tier"],
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn writeback_is_the_only_automatic_posture() {
        assert_eq!(parse_binding("writeback"), Ok(Posture::Writeback));
        assert_eq!(parse_binding("swap"), Ok(Posture::Swap));
        assert!(validate(Posture::Writeback, 1024, Some("/dev/nbd0")).is_ok());
        // The lab posture is selectable, but only by name: nothing derives it
        // from absence, from a failed writeback, or from a default.
        assert_eq!(parse_binding("").unwrap_err(), PostureError::Absent);
        assert_eq!(
            parse_binding("SWAP").unwrap_err(),
            PostureError::Unknown("SWAP".to_string())
        );
    }

    #[test]
    fn an_unknown_posture_binding_is_refused() {
        let error = parse_binding("tiered").expect_err("an unknown posture must refuse");
        assert_eq!(error.code(), "POSTURE_UNKNOWN");
        assert!(error.to_string().contains("tiered"));
        assert_eq!(parse_binding("   ").unwrap_err(), PostureError::Absent);
        assert_eq!(parse_binding("writeback ").unwrap(), Posture::Writeback);
    }

    #[test]
    fn writeback_requires_a_zram_tier() {
        assert_eq!(
            validate(Posture::Writeback, 0, Some("/dev/nbd0")).unwrap_err(),
            PostureError::WritebackRequiresZram
        );
        assert_eq!(
            validate(Posture::Writeback, 1024, None).unwrap_err(),
            PostureError::WritebackRequiresTier
        );
        assert_eq!(
            validate(Posture::Writeback, 1024, Some("")).unwrap_err(),
            PostureError::WritebackRequiresTier
        );
        // The swap posture has no such prerequisite.
        assert!(validate(Posture::Swap, 0, None).is_ok());
    }

    #[test]
    fn only_writeback_adds_the_activation_flag() {
        assert_eq!(
            activation_args(Posture::Writeback),
            &["--zram-writeback-tier"]
        );
        assert!(activation_args(Posture::Swap).is_empty());
        // The wire spelling is proven by `parse_binding`; the status spelling
        // is what an operator reads.
        assert_eq!(Posture::Swap.as_status(), "lab_swap");
        assert_eq!(Posture::Writeback.as_status(), "writeback");
    }
}
