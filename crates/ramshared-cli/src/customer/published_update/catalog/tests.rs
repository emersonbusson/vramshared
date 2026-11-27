// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/published_update/catalog/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use serde_json::{Value, json};

fn release(tag: &str) -> Value {
    let archive = format!("ramshared-linux-{tag}.signed.tar.gz");
    let assets = [
        archive.clone(),
        format!("{archive}.sha256"),
        format!("{archive}.sha256.asc"),
    ];
    let assets = assets.map(|name| json!({
        "browser_download_url": format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{name}"),
        "name": name,
        "state": "uploaded",
        "size": 200,
    }));
    json!({"tag_name": tag, "draft": false, "prerelease": false, "assets": assets})
}

fn read(rows: Vec<Value>) -> Vec<Release> {
    parse_page(&serde_json::to_vec(&rows).unwrap()).unwrap()
}

#[test]
fn signed_patch_discovery_preserves_local_series() {
    let mut no_checksum = release("v1.0.12");
    no_checksum["assets"].as_array_mut().unwrap().pop();
    let mut draft = release("v1.0.11");
    draft["draft"] = json!(true);
    let rows = read(vec![
        release("v1.0.3"),
        release("v1.1.1"),
        release("v1.0.10"),
        release("v2.0.0"),
        release("v1.0.13-rc.1"),
        no_checksum,
        draft,
    ]);
    assert_eq!(select_patch(&rows, "v1.0.2").unwrap().tag, "v1.0.10");
    assert!(select_patch(&rows, "v1.0.10").is_none());
    assert!(select_patch(&rows, "v1.0.2-rc.1").is_none());
    assert!(select_patch(&rows, "v01.0.2").is_none());
    assert!(select_patch(&rows, "v1.0.2-0-g1234").is_none());
}

#[test]
fn only_complete_unique_assets_on_the_fixed_release_are_eligible() {
    let patch = "v1.0.3";
    for (field, value) in [
        ("size", json!(BUNDLE_LIMIT + 1)),
        ("size", json!(0)),
        ("state", json!("new")),
        ("browser_download_url", json!("https://example.com/asset")),
    ] {
        let mut row = release(patch);
        row["assets"][0][field] = value;
        assert!(select_patch(&read(vec![row]), "v1.0.2").is_none());
    }
    let mut row = release(patch);
    let duplicate = row["assets"][0].clone();
    row["assets"].as_array_mut().unwrap().push(duplicate);
    assert!(select_patch(&read(vec![row]), "v1.0.2").is_none());
    let mut row = release(patch);
    row["prerelease"] = json!(true);
    assert!(select_patch(&read(vec![row]), "v1.0.2").is_none());
    assert!(parse_page(&[0; METADATA_LIMIT + 1]).is_none());
    assert!(parse_page(b"not json").is_none());
    assert!(
        parse_page(&serde_json::to_vec(&vec![release(patch); PAGE_SIZE + 1]).unwrap()).is_none()
    );
}
