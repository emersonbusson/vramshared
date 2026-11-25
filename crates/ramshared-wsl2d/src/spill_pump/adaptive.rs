// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/spill_pump/adaptive.rs
//! Pressure-aware zram writeback schedule selection.
//!
//! Under calm load the pump preserves the conservative 32 MiB / 60 s / 5 s
//! defaults to avoid needless SSD write amplification. As zram occupancy
//! climbs past 60% and 85%, it scales budget and cadence up so Tier 1 drains
//! into Tier 2 (`/dev/nbd0` + VRAM cache) before spilling to Tier 3 fallback
//! swap, while keeping a non-zero aging window so active working-set pages
//! clear `ZRAM_IDLE`.

use std::path::Path;
use std::time::Duration;

use super::PumpConfig;

/// Zram utilization threshold (%) for elevated writeback drain.
pub const ELEVATED_UTIL_PCT: u8 = 60;
/// Zram utilization threshold (%) for full-throttle writeback drain.
pub const SATURATED_UTIL_PCT: u8 = 85;

/// Per-tick writeback budget (128 MiB) during elevated zram pressure.
pub const ELEVATED_BUDGET_PAGES: u64 = 32_768;
/// Per-tick writeback budget (256 MiB) during saturated zram pressure.
pub const SATURATED_BUDGET_PAGES: u64 = 65_536;

/// Idle aging window (15 s) during elevated zram pressure.
pub const ELEVATED_IDLE_SECONDS: u64 = 15;
/// Minimum idle aging window (5 s) during saturated zram pressure.
pub const SATURATED_IDLE_SECONDS: u64 = 5;

/// Tick sleep interval during elevated zram pressure.
pub const ELEVATED_INTERVAL: Duration = Duration::from_secs(2);
/// Tick sleep interval during saturated zram pressure.
pub const SATURATED_INTERVAL: Duration = Duration::from_millis(500);

/// Pure plan for one adaptive spill-pump tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveSchedule {
    pub budget_pages: u64,
    pub idle_seconds: u64,
    pub interval: Duration,
}

impl EffectiveSchedule {
    /// Compute the cycle budget, aging window, and sleep cadence for the
    /// observed zram utilization. Absent or unreadable utilization fails safe
    /// to the caller's configured defaults.
    pub fn for_utilization(config: &PumpConfig, util_pct: Option<u8>) -> Self {
        match util_pct {
            Some(pct) if pct >= SATURATED_UTIL_PCT => Self {
                budget_pages: config.budget_pages.max(SATURATED_BUDGET_PAGES),
                idle_seconds: config.idle_seconds.clamp(1, SATURATED_IDLE_SECONDS),
                interval: config.interval.min(SATURATED_INTERVAL),
            },
            Some(pct) if pct >= ELEVATED_UTIL_PCT => Self {
                budget_pages: config.budget_pages.max(ELEVATED_BUDGET_PAGES),
                idle_seconds: config.idle_seconds.clamp(1, ELEVATED_IDLE_SECONDS),
                interval: config.interval.min(ELEVATED_INTERVAL),
            },
            _ => Self {
                budget_pages: config.budget_pages,
                idle_seconds: config.idle_seconds,
                interval: config.interval,
            },
        }
    }

    pub(super) fn apply_to(self, config: &PumpConfig) -> PumpConfig {
        PumpConfig {
            tier_device: config.tier_device.clone(),
            sys_block_root: config.sys_block_root.clone(),
            budget_pages: self.budget_pages,
            idle_seconds: self.idle_seconds,
            interval: self.interval,
        }
    }
}

/// Parse zram utilization percentage from sysfs `mm_stat` (field 0:
/// `orig_data_size` in bytes) and `disksize` (bytes). Returns `None` when
/// either value is malformed or `disksize` is zero.
pub fn parse_zram_utilization_pct(mm_stat: &str, disksize: &str) -> Option<u8> {
    let orig_bytes: u64 = mm_stat.split_whitespace().next()?.parse().ok()?;
    let disk_bytes: u64 = disksize.trim().parse().ok()?;
    if disk_bytes == 0 {
        return None;
    }
    let pct = (u128::from(orig_bytes) * 100) / u128::from(disk_bytes);
    Some(pct.min(100) as u8)
}

/// Read live zram utilization for the tier's bound zram device.
pub fn read_zram_utilization_pct(config: &PumpConfig) -> Option<u8> {
    let zram_device = config.target()?;
    let name = zram_device.rsplit('/').next().unwrap_or(&zram_device);
    let dir = Path::new(&config.sys_block_root).join(name);
    let mm_stat = std::fs::read_to_string(dir.join("mm_stat")).ok()?;
    let disksize = std::fs::read_to_string(dir.join("disksize")).ok()?;
    parse_zram_utilization_pct(&mm_stat, &disksize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_utilization_handles_valid_saturated_and_malformed_sysfs() {
        assert_eq!(parse_zram_utilization_pct("0 0 0", "4294967296\n"), Some(0));
        assert_eq!(
            parse_zram_utilization_pct("2576980378 800000000 850000000", "4294967296\n"),
            Some(60)
        );
        assert_eq!(
            parse_zram_utilization_pct("4294967296 1200000000 1300000000", "4294967296"),
            Some(100)
        );
        assert_eq!(
            parse_zram_utilization_pct("9999999999 0 0", "1000"),
            Some(100)
        );
        // Refusal cases: zero disksize, empty/corrupt inputs never fabricate pressure.
        assert_eq!(parse_zram_utilization_pct("1024 0 0", "0"), None);
        assert_eq!(parse_zram_utilization_pct("", "4096"), None);
        assert_eq!(parse_zram_utilization_pct("abc", "4096"), None);
        assert_eq!(parse_zram_utilization_pct("1024", "not-a-number"), None);
    }

    #[test]
    fn schedule_scales_by_pressure_band_and_fails_safe_when_unknown() {
        let base = PumpConfig::new("/dev/nbd0");
        assert_eq!(
            EffectiveSchedule::for_utilization(&base, None),
            EffectiveSchedule {
                budget_pages: base.budget_pages,
                idle_seconds: base.idle_seconds,
                interval: base.interval,
            }
        );
        assert_eq!(
            EffectiveSchedule::for_utilization(&base, Some(59)),
            EffectiveSchedule {
                budget_pages: base.budget_pages,
                idle_seconds: base.idle_seconds,
                interval: base.interval,
            }
        );
        assert_eq!(
            EffectiveSchedule::for_utilization(&base, Some(60)),
            EffectiveSchedule {
                budget_pages: ELEVATED_BUDGET_PAGES,
                idle_seconds: ELEVATED_IDLE_SECONDS,
                interval: ELEVATED_INTERVAL,
            }
        );
        assert_eq!(
            EffectiveSchedule::for_utilization(&base, Some(85)),
            EffectiveSchedule {
                budget_pages: SATURATED_BUDGET_PAGES,
                idle_seconds: SATURATED_IDLE_SECONDS,
                interval: SATURATED_INTERVAL,
            }
        );
        // Never clamps idle window to zero even if caller configured a tiny value.
        let tiny_idle = PumpConfig {
            idle_seconds: 0,
            ..PumpConfig::new("/dev/nbd0")
        };
        assert_eq!(
            EffectiveSchedule::for_utilization(&tiny_idle, Some(95)).idle_seconds,
            1
        );
    }

    #[test]
    fn read_utilization_returns_none_without_bound_zram_and_reads_fake_sysfs() -> std::io::Result<()>
    {
        let dir = std::env::temp_dir().join(format!("ramshared-adaptive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let zram_dir = dir.join("zram1");
        std::fs::create_dir_all(&zram_dir)?;
        let cfg = PumpConfig {
            sys_block_root: dir.clone(),
            ..PumpConfig::new("/dev/nbd0")
        };
        assert_eq!(read_zram_utilization_pct(&cfg), None);
        std::fs::write(zram_dir.join("backing_dev"), "/dev/nbd0\n")?;
        std::fs::write(zram_dir.join("disksize"), "4000000000\n")?;
        std::fs::write(zram_dir.join("mm_stat"), "3600000000 900000000 950000000\n")?;
        assert_eq!(read_zram_utilization_pct(&cfg), Some(90));
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
