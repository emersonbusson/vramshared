// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_memory_budget/linux_observation/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;

#[test]
fn meminfo_requires_unique_consistent_kib_fields() {
    let sample = parse_meminfo("MemTotal: 1000 kB\nMemAvailable: 500 kB\n", 10).unwrap();
    assert_eq!(sample.total_bytes, 1_024_000);
    assert_eq!(sample.available_bytes, 512_000);
    for input in [
        "MemTotal: 1 kB\n",
        "MemTotal: 1 kB\nMemAvailable: 2 kB\n",
        "MemTotal: 1 MB\nMemAvailable: 1 kB\n",
        "MemTotal: 1 kB\nMemTotal: 2 kB\nMemAvailable: 1 kB\n",
        "MemTotal: 18446744073709551615 kB\nMemAvailable: 1 kB\n",
    ] {
        assert!(parse_meminfo(input, 10).is_err(), "{input}");
    }
}

#[test]
fn adapter_rejects_non_cgroup_filesystem() {
    assert!(visible_cgroups(std::path::Path::new("/tmp"), "0::/", 1).is_err());
}

#[test]
fn adapter_preserves_cgroup_identity_limits_and_sample_time() {
    let source = ramshared_block::memory_observation::CgroupSample {
        identity: ramshared_block::memory_observation::CgroupIdentity {
            device: 7,
            inode: 8,
        },
        limit_bytes: Some(900),
        current_bytes: Some(400),
        sampled_at_unix_ms: 123,
    };
    let result = map_group(source);
    assert_eq!(result.identity.device, 7);
    assert_eq!(result.identity.inode, 8);
    assert_eq!(result.limit_bytes, Some(900));
    assert_eq!(result.current_bytes, Some(400));
    assert_eq!(result.sampled_at_unix_ms, 123);
}
