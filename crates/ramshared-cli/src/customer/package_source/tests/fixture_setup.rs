// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/fixture_setup.rs

struct CompleteBundleFixture {
    root: std::path::PathBuf,
    payload: crate::customer::package::PayloadManifest,
    lines: Vec<String>,
    checksums: String,
    bundle: DeclaredBundle,
}

fn complete_bundle_fixture() -> CompleteBundleFixture {
    use crate::customer::package::{
        ManifestFormat, ManifestKind, ManifestMember, tests::pinned_manifest,
    };
    let root =
        std::env::temp_dir().join(format!("ramshared-complete-source-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let mut payload = pinned_manifest();
    for member in &mut payload.contents {
        let content = if member.kind == ManifestKind::Bin {
            member.mode = "0755".into();
            fixture_elf(62)
        } else {
            member.role.as_bytes().to_vec()
        };
        member.sha256 = Some(digest(&content).unwrap());
        let path = super::super::package_member::bundle_destination(member)
            .unwrap()
            .unwrap();
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, &content).unwrap();
    }
    payload.build.cli_sha256 = digest(&fixture_elf(62)).unwrap();
    payload.build.daemon_sha256 = digest(&fixture_elf(62)).unwrap();
    let identity = [
        ("RELEASE_VERSION", "v1".to_string()),
        ("SOURCE_COMMIT", payload.source.commit.clone()),
        ("SOURCE_BRANCH", payload.source.branch.clone()),
        ("SOURCE_TREE_STATE", "clean".to_string()),
    ];
    for (role, value) in &identity {
        std::fs::write(root.join(role), format!("{value}\n")).unwrap();
    }
    for role in [
        "SHA256SUMS",
        "release-manifest.json",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_BRANCH",
        "SOURCE_TREE_STATE",
    ] {
        payload.contents.push(ManifestMember {
            role: role.into(),
            kind: ManifestKind::Generated,
            mode: "0644".into(),
            source: role.into(),
            formats: vec![ManifestFormat::Bundle],
            sha256: None,
            reason: Some("Generated for bundle".into()),
        });
    }
    let document = serde_json::to_vec(&payload).unwrap();
    std::fs::write(root.join("release-manifest.json"), &document).unwrap();
    let mut lines = vec![format!(
        "{}  ./release-manifest.json",
        digest(&document).unwrap()
    )];
    for member in &payload.contents {
        if !member.kind.is_generated() {
            let path = super::super::package_member::bundle_destination(member)
                .unwrap()
                .unwrap();
            lines.push(format!("{}  ./{path}", member.sha256.as_ref().unwrap()));
        }
    }
    for (role, value) in &identity {
        lines.push(format!(
            "{}  ./{role}",
            digest(format!("{value}\n").as_bytes()).unwrap()
        ));
    }
    let checksums = format!("{}\n", lines.join("\n"));
    std::fs::write(root.join("SHA256SUMS"), &checksums).unwrap();
    let bundle = DeclaredBundle::open(&root, &digest(checksums.as_bytes()).unwrap()).unwrap();
    CompleteBundleFixture {
        root,
        payload,
        lines,
        checksums,
        bundle,
    }
}
