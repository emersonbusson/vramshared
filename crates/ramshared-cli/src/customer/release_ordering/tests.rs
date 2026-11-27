// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/release_ordering/tests.rs
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::{PermissionsExt, symlink};

use rustix::process::geteuid;

use super::*;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn release_is_newer_matches_shell_ordering_contract() {
    assert_eq!(
        release_is_newer("v0.14.1-951-gaaaa", "v0.14.1-971-gbbbb"),
        ReleaseOrder::Older
    );
    assert_eq!(
        release_is_newer("v0.14.1-971-gaaaa", "v0.14.1-971-gbbbb"),
        ReleaseOrder::Equal
    );
    assert_eq!(
        release_is_newer("v0.14.1-972-gaaaa", "v0.14.1-971-gbbbb"),
        ReleaseOrder::Newer
    );
    assert_eq!(
        release_is_newer("v0.14.1-972-gaaaa", "v0.14.1-971-gbbbb+dirty"),
        ReleaseOrder::Newer
    );
    assert_eq!(
        release_is_newer(
            "v0.14.1-18446744073709551616-gabcd",
            "v0.14.1-18446744073709551615-gabcd"
        ),
        ReleaseOrder::Newer
    );
    assert_eq!(
        release_is_newer("v1.0.0", "v0.14.1-971-gbbbb"),
        ReleaseOrder::Newer
    );
    assert_eq!(
        release_is_newer("v1.0.0-1-gaaaa", "v1.0.0"),
        ReleaseOrder::Newer
    );
    assert_eq!(
        release_is_newer("v1.0.1", "v1.0.0-99-gaaaa"),
        ReleaseOrder::Newer
    );
    assert_eq!(release_is_newer("v1.0.0", "v1.0.0"), ReleaseOrder::Equal);
    assert_eq!(
        release_is_newer("v0.14.1-971-gbbbb", "v1.0.0"),
        ReleaseOrder::Older
    );
    assert_eq!(release_is_newer("v1.0.0", "v1.0.1"), ReleaseOrder::Older);
    for bad in [
        "v0.14.1-972-gaaaa-dirty",
        "v0.14.1-972-gaaaa+dirty",
        "v1.0.0-dirty",
        "v1.01.0",
        "v0.14.1-1420-invalid",
        "v0.14.1-01420-gabcd",
        "v0.14.1-1420-gabc",
        "garbage",
    ] {
        assert_eq!(
            release_is_newer(bad, "v0.14.1-971-gbbbb"),
            ReleaseOrder::Unknown,
            "{bad}"
        );
    }
    assert_eq!(
        release_is_newer("v0.14.1-972-gaaaa", "v0.14.1-invalid"),
        ReleaseOrder::Unknown
    );
}

#[test]
fn release_candidates_order_before_their_stable_release() {
    for (candidate, running, order) in [
        ("v1.0.2", "v1.0.3-rc.10", ReleaseOrder::Older),
        ("v1.0.3", "v1.0.3-rc.10", ReleaseOrder::Newer),
        ("v1.0.3-rc.10", "v1.0.3-2-gabcd", ReleaseOrder::Older),
        ("v1.0.3-rc.10", "v1.0.3-rc.9", ReleaseOrder::Newer),
        ("v1.0.3-rc.10", "v1.0.3-rc.10", ReleaseOrder::Equal),
        ("v1.0.3-rc.10-1-gabcd", "v1.0.3-rc.10", ReleaseOrder::Newer),
        ("v1.0.3-rc.10", "v1.0.3-rc.9+dirty", ReleaseOrder::Newer),
        ("v1.0.3-rc.010", "v1.0.3-rc.9", ReleaseOrder::Unknown),
        ("v1.0.3-rc.10-dirty", "v1.0.3-rc.9", ReleaseOrder::Unknown),
        ("v1.0.3-beta.1", "v1.0.3-rc.9", ReleaseOrder::Unknown),
    ] {
        assert_eq!(
            release_is_newer(candidate, running),
            order,
            "{candidate} vs {running}"
        );
    }
}

#[test]
fn find_newest_bundle_picks_latest_and_handles_ties_without_links() {
    let base = std::env::temp_dir().join(format!("ramshared-newest-bundle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("b-pkg/release")).unwrap();
    std::fs::create_dir_all(base.join("a-pkg/release")).unwrap();
    let _fix = Fixture(base.clone());

    use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
    let set_mtime = |path: &Path, secs: i64| {
        let ts = Timespec {
            tv_sec: secs,
            tv_nsec: 0,
        };
        utimensat(
            CWD,
            path,
            &Timestamps {
                last_access: ts,
                last_modification: ts,
            },
            AtFlags::empty(),
        )
        .unwrap();
    };
    set_mtime(&base.join("a-pkg/release"), 1_700_000_000);
    set_mtime(&base.join("b-pkg/release"), 1_700_000_100);
    assert_eq!(
        find_newest_bundle(&base).unwrap(),
        Some(base.join("b-pkg/release"))
    );

    // Equal timestamps tie-break lexicographically.
    set_mtime(&base.join("a-pkg/release"), 1_700_000_100);
    assert_eq!(
        find_newest_bundle(&base).unwrap(),
        Some(base.join("a-pkg/release"))
    );

    // Symlinked release directories are ignored.
    std::fs::create_dir_all(base.join("c-pkg")).unwrap();
    symlink(base.join("b-pkg/release"), base.join("c-pkg/release")).unwrap();
    std::fs::remove_dir_all(base.join("a-pkg")).unwrap();
    std::fs::remove_dir_all(base.join("b-pkg")).unwrap();
    assert_eq!(find_newest_bundle(&base).unwrap(), None);
}

#[test]
fn policy_and_enrolled_posture_parse_strictly() {
    let base = std::env::temp_dir().join(format!("ramshared-policy-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());
    let uid = geteuid().as_raw();

    let policy = base.join("auto-update.conf");
    std::fs::write(
        &policy,
        "# comment\nenabled=1\nlower_sink=/var/lib/sink\nbundle_root=/opt/pkgs\nrelease_keyring=/etc/key.gpg\n",
    )
    .unwrap();
    std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o644)).unwrap();
    let parsed = read_auto_update_policy(&policy, Some(uid))
        .unwrap()
        .unwrap();
    assert!(parsed.enabled);
    assert_eq!(parsed.lower_sink, Some(PathBuf::from("/var/lib/sink")));

    // Group-writable policy is rejected as untrusted.
    std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o664)).unwrap();
    assert!(read_auto_update_policy(&policy, Some(uid)).is_err());

    // Hard-linked policy is rejected, and mismatched expected owner is rejected cleanly
    // without comparing against geteuid() when an explicit expected_owner is supplied.
    std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&policy, b"enabled=1\n").unwrap();
    assert!(read_auto_update_policy(&policy, Some(uid.saturating_add(1))).is_err());
    let hardlink_policy = base.join("hardlink-policy.conf");
    std::fs::hard_link(&policy, &hardlink_policy).unwrap();
    assert!(read_auto_update_policy(&hardlink_policy, Some(uid)).is_err());
    let _ = std::fs::remove_file(&hardlink_policy);

    // Sticky world-writable immediate parent (e.g. /tmp/policy.conf) is rejected.
    let direct_tmp =
        std::env::temp_dir().join(format!("ramshared-direct-{}.conf", std::process::id()));
    std::fs::write(&direct_tmp, "enabled=1\n").unwrap();
    let _direct_res = read_auto_update_policy(&direct_tmp, Some(uid));
    let _ = std::fs::remove_file(&direct_tmp);
    assert!(_direct_res.is_err());

    // Duplicate or traversal policy is rejected.
    assert!(parse_auto_update_policy(b"enabled=1\nenabled=0\n").is_err());
    assert!(parse_auto_update_policy(b"lower_sink=/tmp/../bad\n").is_err());

    let approvals = base.join("approvals");
    std::fs::create_dir(&approvals).unwrap();
    std::fs::write(
        approvals.join("activate-v1.0.token"),
        b"activate:v1.0:vram=1024:posture=writeback\n",
    )
    .unwrap();
    assert_eq!(
        read_enrolled_posture(&approvals, "v1.0").unwrap(),
        Some(Posture::Writeback)
    );
    assert_eq!(read_enrolled_posture(&approvals, "v2.0").unwrap(), None);

    // Additional coverage: relative/symlink/oversized policy paths, swap posture, and sha256 lookup.
    assert!(read_auto_update_policy(Path::new("rel/policy.conf"), Some(uid)).is_err());
    let link_policy = base.join("link-policy.conf");
    symlink(&policy, &link_policy).unwrap();
    assert!(read_auto_update_policy(&link_policy, Some(uid)).is_err());

    std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&policy, vec![b'a'; MAX_POLICY_BYTES + 128]).unwrap();
    assert!(read_auto_update_policy(&policy, Some(uid)).is_err());

    assert!(parse_auto_update_policy(b"\xff\xfe\n").is_err());
    assert!(parse_auto_update_policy(b"enabled=1\x07\n").is_err());
    assert!(
        !parse_auto_update_policy(b"unknown_key=1\nlower_sink\n")
            .unwrap()
            .enabled
    );
    assert!(parse_auto_update_policy(b"bundle_root=relative/dir\n").is_err());
    assert!(parse_auto_update_policy(b"release_keyring=relative/key.gpg\n").is_err());

    std::fs::write(
        approvals.join("activate-v2.0.token"),
        b"activate:v2.0:vram=1024:posture=swap\n",
    )
    .unwrap();
    assert_eq!(
        read_enrolled_posture(&approvals, "v2.0").unwrap(),
        Some(Posture::Swap)
    );
    symlink(
        approvals.join("activate-v2.0.token"),
        approvals.join("activate-v3.0.token"),
    )
    .unwrap();
    assert_eq!(read_enrolled_posture(&approvals, "v3.0").unwrap(), None);
    assert!(read_enrolled_posture(&approvals, "../bad").is_err());
    assert_eq!(
        read_enrolled_posture(&base.join("no-approvals"), "v1.0").unwrap(),
        None
    );

    let sha_bundles = base.join("sha-bundles");
    std::fs::create_dir_all(sha_bundles.join("b1/release")).unwrap();
    std::fs::create_dir_all(sha_bundles.join("b2/release")).unwrap();
    let target_sum = crate::customer::install_fs::digest(b"m2\n");
    std::fs::write(sha_bundles.join("b1/release/SHA256SUMS"), b"m1\n").unwrap();
    std::fs::write(sha_bundles.join("b2/release/SHA256SUMS"), b"m2\n").unwrap();
    std::fs::write(sha_bundles.join("b2/release/RELEASE_VERSION"), b"v1.2.3\n").unwrap();
    assert_eq!(
        find_bundle_by_manifest_sha256(&sha_bundles, &target_sum),
        Some(sha_bundles.join("b2/release"))
    );
    assert_eq!(
        find_bundle_by_manifest_sha256(&sha_bundles, &"0".repeat(64)),
        None
    );
    assert_eq!(
        find_bundle_by_manifest_sha256(&base.join("missing-root"), &target_sum),
        None
    );
    assert_eq!(
        release_is_newer("bad", "v1.0-1-gaaaa"),
        ReleaseOrder::Unknown
    );
}
