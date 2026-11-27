// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/published_update/archive/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use flate2::{Compression, write::GzEncoder};

const ROOT: &str = "ramshared-linux-v1.0.3";

fn archive(entries: &[(&str, tar::EntryType, &[u8])]) -> File {
    let file = tempfile::tempfile().unwrap();
    let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::fast()));
    for (name, ty, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*ty);
        header.set_mode(0o7777);
        header.set_size(data.len() as u64);
        if ty.is_symlink() {
            header.set_link_name("RELEASE_VERSION").unwrap();
        }
        tar.append_data(&mut header, name, *data).unwrap();
    }
    let mut file = tar.into_inner().unwrap().finish().unwrap();
    use std::io::Seek;
    file.rewind().unwrap();
    file
}

#[test]
fn packaged_files_are_copied_under_the_exact_release_without_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let stage = tempfile::tempdir().unwrap();
    let first = format!("{ROOT}/release/RELEASE_VERSION");
    let second = format!("{ROOT}/release/bin/ramshared");
    let bundle = archive(&[
        (&first, tar::EntryType::Regular, b"v1.0.3\n"),
        (&second, tar::EntryType::Regular, b"ELF"),
    ]);
    extract(bundle, stage.path(), ROOT).unwrap();
    let bin = stage.path().join(second);
    assert_eq!(
        std::fs::read(stage.path().join(first)).unwrap(),
        b"v1.0.3\n"
    );
    assert_eq!(
        std::fs::metadata(bin).unwrap().permissions().mode() & 0o7777,
        0o600
    );
}

#[test]
fn extraction_refuses_wrong_root_duplicates_and_non_regular_members() {
    let first = format!("{ROOT}/release/RELEASE_VERSION");
    for entries in [
        vec![(
            "different/release/RELEASE_VERSION",
            tar::EntryType::Regular,
            b"" as &[u8],
        )],
        vec![(&first, tar::EntryType::Symlink, b"" as &[u8])],
        vec![(&first, tar::EntryType::Regular, b"" as &[u8]); 2],
    ] {
        let stage = tempfile::tempdir().unwrap();
        assert!(extract(archive(&entries), stage.path(), ROOT).is_err());
        assert!(!stage.path().join("different").exists());
    }
}
