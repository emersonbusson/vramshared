// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/tree_custody/tests.rs
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::{PermissionsExt, symlink};

use rustix::process::{Resource, getrlimit};

use super::*;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn digest(bytes: &[u8]) -> String {
    let mut r = bytes;
    crate::installed_image::sha256_reader(&mut r).unwrap()
}

fn prior_fixture(name: &str) -> (Fixture, File, Vec<u8>) {
    let root = std::env::temp_dir().join(format!(
        "ramshared-tree-custody-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(root.join("scripts/safety")).unwrap();
    let bin = b"prior-binary".to_vec();
    let cfg = b"cfg\n".to_vec();
    std::fs::write(root.join("bin/ramshared"), &bin).unwrap();
    std::fs::write(root.join("scripts/safety/cascade.conf"), &cfg).unwrap();
    let manifest = format!(
        "{}  ./bin/ramshared\n{}  ./scripts/safety/cascade.conf\n",
        digest(&bin),
        digest(&cfg)
    )
    .into_bytes();
    std::fs::write(root.join("INPUT_BUNDLE_SHA256SUMS"), &manifest).unwrap();
    std::fs::write(root.join("INSTALL_PROVENANCE.json"), b"{}\n").unwrap();
    std::fs::write(root.join("SHA256SUMS"), b"manifest\n").unwrap();
    std::fs::write(
        root.join("INSTALLED_MANIFEST_SHA256"),
        format!("{}\n", digest(b"manifest\n")),
    )
    .unwrap();
    let dir = open_directory_no_follow(&root, false).unwrap().unwrap();
    (Fixture(root), dir, manifest)
}

#[test]
fn descriptor_budget_enforces_headroom_and_bounds() {
    let live = DescriptorBudget::acquire().unwrap();
    assert!(live.remaining() <= MAX_HELD_TREE_FDS);
    let mut exact = DescriptorBudget::with_cap(5);
    assert!(exact.reserve(5).is_ok());
    assert_eq!(exact.remaining(), 0);
    assert!(exact.reserve(1).is_err());
}

#[test]
fn previous_custody_detects_in_place_byte_mode_extra_and_receipt_tampering() {
    let (fixture, dir, manifest) = prior_fixture("tamper");
    let mut budget = DescriptorBudget::with_cap(32);
    let custody =
        TreeCustody::capture_previous(&dir, &manifest, (4096, 65536), None, &mut budget).unwrap();
    assert_eq!(custody.held_fd_count(), 10);
    assert!(custody.revalidate().is_ok());
    assert!(custody.member_file("bin/ramshared").is_some());
    assert_eq!(
        custody.expected_sha256("bin/ramshared"),
        Some(digest(b"prior-binary").as_str())
    );

    let mut too_small = DescriptorBudget::with_cap(9);
    assert!(
        TreeCustody::capture_previous(&dir, &manifest, (4096, 65536), None, &mut too_small)
            .is_err()
    );

    // In-place content mutation with identical length on a held receipt or member.
    std::fs::write(fixture.0.join("INSTALL_PROVENANCE.json"), b"[]\n").unwrap();
    assert!(custody.revalidate().is_err());

    std::fs::write(fixture.0.join("INSTALL_PROVENANCE.json"), b"{}\n").unwrap();
    let custody2 = TreeCustody::capture_previous(
        &dir,
        &manifest,
        (4096, 65536),
        None,
        &mut DescriptorBudget::with_cap(32),
    )
    .unwrap();
    std::fs::write(fixture.0.join("bin/ramshared"), b"tampered-bin").unwrap();
    assert!(custody2.revalidate().is_err());

    // Chmod in place without changing file bytes.
    let (f_mode, d_mode, m_mode) = prior_fixture("chmod");
    let c_mode = TreeCustody::capture_previous(
        &d_mode,
        &m_mode,
        (4096, 65536),
        None,
        &mut DescriptorBudget::with_cap(32),
    )
    .unwrap();
    std::fs::set_permissions(
        f_mode.0.join("scripts/safety/cascade.conf"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(c_mode.revalidate().is_err());

    // Extra symlink dropped inside subdir after capture.
    let (f_extra, d_extra, m_extra) = prior_fixture("extra");
    let c_extra = TreeCustody::capture_previous(
        &d_extra,
        &m_extra,
        (4096, 65536),
        None,
        &mut DescriptorBudget::with_cap(32),
    )
    .unwrap();
    symlink("target", f_extra.0.join("bin/rogue")).unwrap();
    assert!(c_extra.revalidate().is_err());
}

#[test]
#[ignore = "subprocess fixture invoked by descriptor_budget_refuses_exhausted_nofile"]
fn descriptor_budget_low_nofile_child() {
    let Ok(flag) = std::env::var("RAMSHARED_CUSTODY_NOFILE_CHILD") else {
        return;
    };
    if flag != "1" {
        return;
    }
    let (_f, dir, manifest) = prior_fixture("nofile-child");
    let cur = getrlimit(Resource::Nofile);
    let open = open_fd_count().unwrap();
    let ceiling = u64::try_from(open + RESERVED_HEADROOM_FDS + 2).unwrap();
    rustix::process::setrlimit(
        Resource::Nofile,
        rustix::process::Rlimit {
            current: Some(ceiling),
            maximum: cur.maximum,
        },
    )
    .unwrap();
    let mut budget = DescriptorBudget::acquire().unwrap();
    assert!(budget.remaining() <= 2);
    let res = TreeCustody::capture_previous(&dir, &manifest, (4096, 65536), None, &mut budget);
    assert!(res.is_err());
    std::process::exit(0);
}

#[test]
fn descriptor_budget_refuses_exhausted_nofile() {
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "customer::package_selector::tree_custody::tests::descriptor_budget_low_nofile_child",
        ])
        .env("RAMSHARED_CUSTODY_NOFILE_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
