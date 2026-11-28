// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/supervisor/memory_policy.rs
//! Shared control-plane memory thresholds used by the supervisor and display.

pub(crate) const GUARDED_PSI_FULL_AVG10_PERCENT: f64 = 2.0;
pub(crate) const RECOVERY_MARGIN_BYTES: u64 = 1024 * 1024 * 1024;
pub(super) const CACHE_RECOVERY_MARGIN_BYTES: u64 = 256 * 1024 * 1024;
const MIN_CONTROL_RESERVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Return the control reserve using the same byte-precise rounding as admission.
pub(crate) fn control_reserve_bytes(total_bytes: u64) -> u64 {
    total_bytes.div_ceil(4).max(MIN_CONTROL_RESERVE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_uses_the_sealed_floor_and_rounds_up_the_fraction() {
        let gib = 1024 * 1024 * 1024u64;
        assert_eq!(control_reserve_bytes(8 * gib), 4 * gib);
        assert_eq!(control_reserve_bytes(20 * gib), 5 * gib);
        assert_eq!(control_reserve_bytes(16 * gib + 1), 4 * gib + 1);
        assert_eq!(control_reserve_bytes(u64::MAX), u64::MAX.div_ceil(4));
    }
}
