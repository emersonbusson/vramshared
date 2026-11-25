// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/source_admission_scenarios.rs

fn exercise_source_admission<'a>(
    root: &std::path::Path,
    payload: &crate::customer::package::PayloadManifest,
    lines: &[String],
    checksums: &str,
    bundle: &'a DeclaredBundle,
) -> (VerifiedBundle, StreamVerifiedBundle<'a>) {
    use crate::customer::package::ManifestArchitecture::{Amd64, Arm64};
    assert!(bundle.verify_all_for(1, Amd64).is_none());
    assert!(bundle.verify_all_for(1024 * 1024, Arm64).is_none());
    assert_eq!(
        bundle.verify_all(1024 * 1024).is_some(),
        observed_host_architecture() == Some(Amd64)
    );
    let verified = bundle.verify_all_for(1024 * 1024, Amd64).unwrap();
    assert!(bundle.verify_streamed_for(1, u64::MAX, Amd64).is_none());
    assert!(bundle.verify_streamed_for(u64::MAX, 1, Amd64).is_none());
    assert!(
        bundle
            .verify_streamed_for(u64::MAX, u64::MAX, Arm64)
            .is_none()
    );
    let total_size: u64 = bundle
        .stage_roles()
        .unwrap()
        .iter()
        .map(|role| bundle.member_size(role).unwrap())
        .sum();
    assert!(
        bundle
            .verify_streamed_for(u64::MAX, total_size, Amd64)
            .is_some()
    );
    assert!(
        bundle
            .verify_streamed_for(u64::MAX, total_size - 1, Amd64)
            .is_none()
    );
    // Admission checks observations repeatedly, not only at the start.
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let requirement = crate::customer::package_source::stream::MemoryRequirement {
        work_bytes: 100,
        guest_reserve_bytes: 200,
        host_reserve_bytes: 200,
        max_age_ms: 5_000,
    };
    let mut count = 0;
    let gated = bundle.verify_streamed(u64::MAX, u64::MAX, &requirement, &mut || {
        count += 1;
        (count == 1).then(
            || crate::customer::package_source::stream::MemoryObservation {
                platform: Some(ramshared_block::platform::PlatformKind::NativeLinux),
                guest: ramshared_tier::memory_headroom::MemorySample {
                    total_bytes: 1_000,
                    available_bytes: 700,
                    sampled_at_unix_ms: now_ms,
                },
                host: ramshared_tier::memory_headroom::HostScope::Native,
                cgroups: Some(
                    crate::customer::package_source::stream::MemoryObservation::fixture_cgroups(
                        now_ms,
                    ),
                ),
            },
        )
    });
    assert!(gated.is_none());
    assert_eq!(count, 2);
    let gated_streamed = bundle
        .verify_streamed(u64::MAX, u64::MAX, &requirement, &mut || {
            let now = u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_millis(),
            )
            .ok()?;
            Some(crate::customer::package_source::stream::MemoryObservation {
                platform: Some(ramshared_block::platform::PlatformKind::NativeLinux),
                guest: ramshared_tier::memory_headroom::MemorySample {
                    total_bytes: 1_000,
                    available_bytes: 700,
                    sampled_at_unix_ms: now,
                },
                host: ramshared_tier::memory_headroom::HostScope::Native,
                cgroups: Some(
                    crate::customer::package_source::stream::MemoryObservation::fixture_cgroups(
                        now,
                    ),
                ),
            })
        })
        .unwrap();
    {
        let streamed = bundle
            .verify_streamed_for(u64::MAX, u64::MAX, Amd64)
            .unwrap();
        assert_eq!(streamed.manifest_sha256(), verified.manifest_sha256());
    }
    let other_commit = "d".repeat(40);
    std::fs::write(root.join("SOURCE_COMMIT"), format!("{other_commit}\n")).unwrap();
    let wrong_lines: Vec<_> = lines
        .iter()
        .map(|line| {
            if line.ends_with("  ./SOURCE_COMMIT") {
                format!(
                    "{}  ./SOURCE_COMMIT",
                    digest(format!("{other_commit}\n").as_bytes()).unwrap()
                )
            } else {
                line.clone()
            }
        })
        .collect();
    let wrong = format!("{}\n", wrong_lines.join("\n"));
    std::fs::write(root.join("SHA256SUMS"), &wrong).unwrap();
    let wrong_bundle = DeclaredBundle::open(root, &digest(wrong.as_bytes()).unwrap()).unwrap();
    assert!(wrong_bundle.verify_all_for(1024 * 1024, Amd64).is_none());
    assert!(
        wrong_bundle
            .verify_streamed_for(u64::MAX, u64::MAX, Amd64)
            .is_none()
    );
    std::fs::write(
        root.join("SOURCE_COMMIT"),
        format!("{}\n", payload.source.commit),
    )
    .unwrap();
    assert_eq!(
        verified.manifest_sha256,
        digest(checksums.as_bytes()).unwrap()
    );
    assert_eq!(
        verified.members.get("ramshared").unwrap().destination,
        "bin/ramshared"
    );
    assert_eq!(verified.members.get("ramshared").unwrap().mode, "0755");
    (verified, gated_streamed)
}
