// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/stream/guest.rs
//! Bounded read-only guest memory observation.

use std::fs::File;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use ramshared_tier::memory_headroom::MemorySample;

pub(super) fn now_ms() -> Option<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()
}

fn parse_guest(bytes: &[u8], sampled_at_unix_ms: u64) -> Option<MemorySample> {
    if sampled_at_unix_ms == 0 || bytes.len() > ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES {
        return None;
    }
    let memory = ramshared_tier::DrainMemory::parse(std::str::from_utf8(bytes).ok()?)?;
    Some(MemorySample {
        total_bytes: memory.total_bytes,
        available_bytes: memory.available_bytes,
        sampled_at_unix_ms,
    })
}

/// Only guest counters are measured; this never proves host or cgroup custody.
pub(in crate::customer) fn observe_guest() -> Option<MemorySample> {
    let sampled = now_ms()?;
    let mut bytes = Vec::new();
    File::open("/proc/meminfo")
        .ok()?
        .take((ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES as u64).checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    if now_ms()? < sampled {
        return None;
    }
    parse_guest(&bytes, sampled)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "missing fixture observations fail the test"
)]
mod tests {
    use super::*;

    #[test]
    fn guest_parser_rejects_absent_inconsistent_and_oversized_data() {
        let good = "MemTotal: 1000 kB\nMemAvailable: 500 kB\n";
        let sample = parse_guest(good.as_bytes(), 10).unwrap();
        assert_eq!(sample.total_bytes, 1_024_000);
        assert_eq!(sample.available_bytes, 512_000);
        assert_eq!(sample.sampled_at_unix_ms, 10);
        assert!(parse_guest(good.as_bytes(), 0).is_none());
        assert!(parse_guest(b"MemTotal: 1 kB\n", 10).is_none());
        assert!(parse_guest(b"MemTotal: 1 kB\nMemAvailable: 2 kB\n", 10).is_none());
        assert!(
            parse_guest(
                b"MemTotal: 1 kB\nMemAvailable: 1 kB\nMemAvailable: 1 kB\n",
                10
            )
            .is_none()
        );
        assert!(
            parse_guest(
                &vec![b'x'; ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES + 1],
                10
            )
            .is_none()
        );
    }

    #[test]
    fn live_guest_observation_is_bounded_and_nonzero() {
        let sample = observe_guest().unwrap();
        assert!(sample.total_bytes > 0);
        assert!(sample.available_bytes <= sample.total_bytes);
        assert!(sample.sampled_at_unix_ms > 0);
    }
}
