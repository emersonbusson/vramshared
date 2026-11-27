// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/published_update/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use std::io::{Read, Write};

#[test]
fn checksum_is_bound_to_one_archive_and_checks_exact_bytes() {
    let name = "ramshared-linux-v1.0.3.signed.tar.gz";
    let mut body = &b"sealed payload"[..];
    let hash = sha256_reader(&mut body).unwrap();
    let text = format!("{hash}  {name}\n");
    assert_eq!(
        checksum_for(text.as_bytes(), name).as_deref(),
        Some(hash.as_str())
    );
    assert!(checksum_for(text.as_bytes(), "another.tar.gz").is_none());
    assert!(checksum_for(format!("{text}{text}").as_bytes(), name).is_none());
    assert!(checksum_for(format!("bad  {name}\n").as_bytes(), name).is_none());

    let mut archive = tempfile::tempfile().unwrap();
    archive.write_all(b"sealed payload").unwrap();
    archive.rewind().unwrap();
    authenticate_outer(&mut archive, &hash).unwrap();
    let mut bytes = Vec::new();
    archive.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"sealed payload");
    archive.rewind().unwrap();
    assert!(authenticate_outer(&mut archive, &"a".repeat(64)).is_err());
}

#[test]
#[ignore = "requires root, the enrolled release keyring, and network access"]
fn live_signed_public_release_is_retained() {
    use std::os::unix::fs::PermissionsExt;

    assert_eq!(rustix::process::geteuid().as_raw(), 0);
    let root = tempfile::tempdir().unwrap();
    let policy = root.path().join("auto-update.conf");
    let keyring = Path::new("/etc/ramshared/release-keyring.gpg");
    std::fs::write(
        &policy,
        format!(
            "enabled=1\nsource=github\nrelease_keyring={}\n",
            keyring.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();

    let release = http::by_tag("v1.0.3").expect("signed stable release");
    let bundle = retain(&release, root.path(), &policy, keyring, None, false)
        .expect("retained public release");
    let manifest = bundle_manifest(&bundle, "v1.0.3", &policy).expect("verified bundle manifest");
    assert_eq!(
        retain(
            &release,
            root.path(),
            &policy,
            keyring,
            Some(&manifest),
            false
        )
        .unwrap(),
        bundle
    );
    assert!(
        retain(
            &release,
            root.path(),
            &policy,
            keyring,
            Some(&"f".repeat(64)),
            false,
        )
        .is_err()
    );
    assert!(bundle_manifest(&bundle, "v1.0.4", &policy).is_none());
    println!(
        "published release {} retained with manifest {manifest}",
        release.tag
    );
}
