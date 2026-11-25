// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

/// Nibbles the fixture pins, so a test compares against the same digest
/// the fixture declared instead of recomputing it from a different seed.
const CLI_NIBBLE: u8 = 10;
const DAEMON_NIBBLE: u8 = 11;
const COMMIT_NIBBLE: u8 = 12;
const OTHER_NIBBLE: u8 = 14;

/// 64 lowercase hex digits of one repeated nibble. Always a valid digest
/// shape, so a fixture can never fail a rule it is not exercising.
fn digest(nibble: u8) -> String {
    let digit = char::from(b"0123456789abcdef"[usize::from(nibble % 16)]);
    std::iter::repeat_n(digit, 64).collect()
}

/// 40 lowercase hex digits of one repeated nibble, for a commit identity.
fn hex(nibble: u8) -> String {
    let digit = char::from(b"0123456789abcdef"[usize::from(nibble % 16)]);
    std::iter::repeat_n(digit, 40).collect()
}

fn member(role: &str, kind: ManifestKind, sha256: Option<String>) -> ManifestMember {
    ManifestMember {
        role: role.to_string(),
        kind,
        mode: "0644".to_string(),
        source: format!("scripts/safety/{role}"),
        formats: ALL_FORMATS.to_vec(),
        sha256,
        reason: None,
    }
}

pub(crate) fn pinned_manifest() -> PayloadManifest {
    let cli = digest(CLI_NIBBLE);
    let daemon = digest(DAEMON_NIBBLE);
    let dropin = "docker.service.d/10-ramshared-control.conf";
    PayloadManifest {
        schema_version: PAYLOAD_MANIFEST_SCHEMA.to_string(),
        product: ManifestProduct {
            name: "ramshared".to_string(),
            version: "0.15.0".to_string(),
        },
        architecture: ManifestArchitecture::Amd64,
        source: ManifestSource {
            commit: hex(COMMIT_NIBBLE),
            branch: "main".to_string(),
            tree_state: ManifestTreeState::Clean,
        },
        build: ManifestBuild {
            cli_sha256: cli.clone(),
            daemon_sha256: daemon.clone(),
            source_date_epoch: Some(0),
        },
        contents: vec![
            member("ramshared", ManifestKind::Bin, Some(cli)),
            member("ramsharedd", ManifestKind::Bin, Some(daemon)),
            member(
                "ramshared-cascade.service",
                ManifestKind::Unit,
                Some(digest(1)),
            ),
            member(dropin, ManifestKind::Dropin, Some(digest(2))),
            member(
                "origin.conf.example",
                ManifestKind::ConfigExample,
                Some(digest(3)),
            ),
        ],
        dependencies: ManifestDependencies {
            runtime: vec!["bash".to_string()],
        },
        units: ManifestUnits {
            lifecycle: vec!["ramshared-cascade.service".to_string()],
            control_plane: vec!["ramshared-cascade.service".to_string()],
            dropins: vec![dropin.to_string()],
            excluded: Some(vec!["nbd-benchmark-cell.sh".to_string()]),
        },
        configuration: ManifestConfiguration {
            examples: vec!["origin.conf.example".to_string()],
            conffiles: Vec::new(),
            operator_state: vec!["/var/lib/ramshared/origin.conf".to_string()],
        },
    }
}

fn document(manifest: &PayloadManifest) -> Vec<u8> {
    serde_json::to_vec(manifest).unwrap()
}

fn rejection(manifest: &PayloadManifest) -> CustomerError {
    PayloadManifest::parse(&document(manifest)).expect_err("the document must be refused")
}

#[test]
fn manifest_refuses_ambiguous_and_escaping_roles() {
    let mut manifest = pinned_manifest();
    manifest.contents.push(member(
        "ramshared/package-lifecycle-hook.sh",
        ManifestKind::Script,
        Some(digest(3)),
    ));
    PayloadManifest::parse(&document(&manifest)).expect("declared nested role is valid");

    for role in [
        "safe/../escape",
        "safe/./alias",
        "safe//alias",
        "safe/",
        "safe/../../escape",
    ] {
        let mut bad = pinned_manifest();
        bad.contents
            .push(member(role, ManifestKind::Script, Some(digest(3))));
        assert_eq!(
            rejection(&bad).reason,
            CustomerReason::UnsupportedSurface,
            "role {role:?} must not name an ambiguous or escaping destination"
        );
    }
}

#[test]
fn manifest_refuses_unplaceable_or_colliding_bundle_members() {
    let mut scoped = pinned_manifest();
    let mut rule = member("65-ramshared.rules", ManifestKind::Rule, Some(digest(4)));
    rule.formats
        .retain(|format| *format != ManifestFormat::Bundle);
    rule.reason = Some("The sealed bundle has no udev rules destination".to_string());
    scoped.contents.push(rule);
    PayloadManifest::parse(&document(&scoped)).expect("non-bundle rule is valid");

    let mut unplaceable = pinned_manifest();
    unplaceable.contents.push(member(
        "65-ramshared.rules",
        ManifestKind::Rule,
        Some(digest(4)),
    ));
    assert_eq!(
        rejection(&unplaceable).reason,
        CustomerReason::UnsupportedSurface
    );

    let mut collision = pinned_manifest();
    collision.contents.push(member(
        "scripts/safety/origin.conf.example",
        ManifestKind::Generated,
        None,
    ));
    assert_eq!(
        rejection(&collision).reason,
        CustomerReason::UnsupportedSurface
    );

    let mut parent_collision = pinned_manifest();
    parent_collision
        .contents
        .push(member("bin", ManifestKind::Generated, None));
    assert_eq!(
        rejection(&parent_collision).reason,
        CustomerReason::UnsupportedSurface,
        "a file cannot also be the directory of another payload member"
    );
    let mut parent_first = pinned_manifest();
    parent_first
        .contents
        .insert(0, member("bin", ManifestKind::Generated, None));
    assert_eq!(
        rejection(&parent_first).reason,
        CustomerReason::UnsupportedSurface
    );
}

#[test]
fn bundle_destination_matches_supported_kind_layouts() {
    for (kind, expected) in [
        (ManifestKind::Generated, "sample"),
        (ManifestKind::Bin, "bin/sample"),
        (ManifestKind::Script, "scripts/safety/sample"),
        (ManifestKind::Library, "scripts/safety/sample"),
        (ManifestKind::Config, "scripts/safety/sample"),
        (ManifestKind::ConfigExample, "scripts/safety/sample"),
        (ManifestKind::Unit, "systemd/sample"),
        (ManifestKind::UnitTemplate, "systemd/sample"),
        (ManifestKind::Dropin, "systemd/sample"),
    ] {
        let item = member("sample", kind, None);
        assert_eq!(
            bundle_destination(&item).unwrap().as_deref(),
            Some(expected)
        );
    }
    for kind in [
        ManifestKind::Rule,
        ManifestKind::Tmpfiles,
        ManifestKind::Sysusers,
        ManifestKind::Doc,
    ] {
        let mut item = member("sample", kind, None);
        assert!(bundle_destination(&item).is_err());
        item.formats
            .retain(|format| *format != ManifestFormat::Bundle);
        assert_eq!(bundle_destination(&item).unwrap(), None);
    }
    assert!(
        bundle_destination(&member("safe/../escape", ManifestKind::Script, None)).is_err(),
        "the destination mapper must reject unsafe roles even when called directly"
    );
}

#[test]
fn manifest_and_bundle_destinations_match_real_builder() {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Command;

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let emit = r#"
        set -euo pipefail
        source "$1/scripts/package/package-payload-manifest.sh"
        ramshared_payload_emit_json "$1" 0.15.0 amd64 "$2" main clean "$3" "$4"
    "#;
    let output = Command::new("bash")
        .args(["-c", emit, "builder-fixture"])
        .arg(&root)
        .arg(hex(COMMIT_NIBBLE))
        .arg(digest(CLI_NIBBLE))
        .arg(digest(DAEMON_NIBBLE))
        .output()
        .expect("run bundle manifest builder");
    assert!(
        output.status.success(),
        "builder refused its declaration: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest = PayloadManifest::parse(&output.stdout).expect("parse actual builder output");

    let layout = r#"
        set -euo pipefail
        source "$1/scripts/package/package-payload-manifest.sh"
        while IFS='|' read -r kind role mode source formats reason; do
            [[ ,$formats, == *,bundle,* ]] || continue
            printf '%s|' "$role"
            ramshared_payload_bundle_destination "$kind" "$role"
        done < <(ramshared_payload_members)
    "#;
    let output = Command::new("bash")
        .args(["-c", layout, "builder-layout"])
        .arg(&root)
        .output()
        .expect("run bundle destination mapping");
    assert!(output.status.success(), "builder layout failed");
    let shell_layout = String::from_utf8(output.stdout).unwrap();
    let expected: BTreeMap<&str, &str> = shell_layout
        .lines()
        .map(|line| line.split_once('|').expect("role and destination"))
        .collect();
    let actual: BTreeMap<&str, String> = manifest
        .contents
        .iter()
        .filter_map(|item| {
            bundle_destination(item)
                .expect("builder member has a supported destination")
                .map(|path| (item.role.as_str(), path))
        })
        .collect();
    assert_eq!(actual.len(), expected.len());
    for (role, path) in actual {
        assert_eq!(expected.get(role).copied(), Some(path.as_str()), "{role}");
    }
}

/// Payload identity: a tampered member, a wrong architecture and a wrong
/// binary version are three distinct refusals, and none of them is a
/// repair. DT-1 requires architecture, binary build identity and hashes to
/// agree before selection.
#[test]
fn manifest_rejects_tampered_wrong_arch_and_wrong_binary_version() {
    let manifest = pinned_manifest();

    // A document that agrees with what was shipped parses and verifies.
    let identity = PayloadIdentity {
        architecture: ManifestArchitecture::Amd64,
        product_version: "0.15.0".to_string(),
        cli_sha256: digest(CLI_NIBBLE),
        daemon_sha256: digest(DAEMON_NIBBLE),
    };
    assert_eq!(
        PayloadManifest::parse(&document(&manifest)).unwrap(),
        manifest
    );
    manifest.verify_identity(&identity).expect(
        "a manifest whose architecture, version and binary digests agree with the payload verifies",
    );

    // Tampered: the shipped bytes are not the member the declaration names.
    manifest
        .verify_member_bytes("ramshared-cascade.service", b"not the staged unit")
        .expect_err("bytes that digest to something else must be refused");
    manifest
        .verify_member_bytes("absent-role", b"anything")
        .expect_err("a member the manifest does not declare must be refused");
    // A generated member has no digest, so there is nothing to verify.
    let mut generated = manifest.clone();
    let example = generated
        .contents
        .iter_mut()
        .find(|member| member.role == "origin.conf.example")
        .unwrap();
    example.kind = ManifestKind::Generated;
    example.sha256 = None;
    let error = generated
        .verify_member_bytes("origin.conf.example", b"whatever the builder wrote")
        .expect_err("a generated member has no digest to verify");
    assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
    // ... and the document itself is refused, because a pin that no longer
    // matches its own `bin` member describes a release that does not exist.
    generated.build.cli_sha256 = digest(1);
    assert!(
        rejection(&generated)
            .detail
            .contains("pinned build identity")
    );

    // Wrong architecture.
    let wrong_arch = PayloadIdentity {
        architecture: ManifestArchitecture::Arm64,
        ..identity.clone()
    };
    let error = manifest
        .verify_identity(&wrong_arch)
        .expect_err("a foreign architecture must be refused");
    assert_eq!(error.reason, CustomerReason::IdentityChanged);
    assert_eq!(error.exit_code(), 4);

    // Wrong binary version: the shipped binaries digest to something else.
    let wrong_binary = PayloadIdentity {
        cli_sha256: digest(OTHER_NIBBLE),
        ..identity.clone()
    };
    let error = manifest
        .verify_identity(&wrong_binary)
        .expect_err("a wrong binary build identity must be refused");
    assert_eq!(error.reason, CustomerReason::IdentityChanged);
    let wrong_daemon = PayloadIdentity {
        daemon_sha256: digest(OTHER_NIBBLE),
        ..identity.clone()
    };
    manifest
        .verify_identity(&wrong_daemon)
        .expect_err("a wrong daemon build identity must be refused");

    // Wrong product version label is also a refused identity, not a rename.
    let wrong_version = PayloadIdentity {
        product_version: "0.14.9".to_string(),
        ..identity
    };
    manifest
        .verify_identity(&wrong_version)
        .expect_err("a wrong product version must be refused");
}

/// The ordered semantic agreements each name their own cause. A schema
/// cannot express these; they are why a covered validator exists.
#[test]
fn manifest_semantics_refuse_scoped_silence_retired_units_and_half_a_control_plane() {
    // Unrecognised schema version is refused, never guessed.
    let mut bad = pinned_manifest();
    bad.schema_version = "ramshared-release-manifest/v2".to_string();
    assert!(rejection(&bad).detail.contains("schema_version"));

    // Unknown fields are refused, not ignored.
    let mut text = String::from_utf8(document(&pinned_manifest())).unwrap();
    text = text.replacen("{", "{\"smuggled\":true,", 1);
    let error = PayloadManifest::parse(text.as_bytes()).expect_err("unknown fields must refuse");
    assert_eq!(error.reason, CustomerReason::UnsupportedSurface);

    // A format-scoped member without a reason is an undeclared difference.
    let mut bad = pinned_manifest();
    bad.contents[2].formats = vec![ManifestFormat::Deb];
    assert!(rejection(&bad).detail.contains("reason"));

    // A shared member with a reason is also wrong: the reason is exactly
    // the declaration that the member is scoped.
    let mut bad = pinned_manifest();
    bad.contents[2].reason = Some("explaining a shared member".to_string());
    assert!(rejection(&bad).detail.contains("reason"));

    // A declared exclusion that is staged is the regression.
    let mut bad = pinned_manifest();
    bad.units.excluded = Some(vec!["ramshared-cascade.service".to_string()]);
    assert!(rejection(&bad).detail.contains("excluded"));

    // The retired lifecycle unit is a second activation path.
    let mut bad = pinned_manifest();
    bad.units.lifecycle.push(RETIRED_LIFECYCLE_UNIT.to_string());
    bad.units
        .control_plane
        .push(RETIRED_LIFECYCLE_UNIT.to_string());
    bad.contents.push(member(
        RETIRED_LIFECYCLE_UNIT,
        ManifestKind::Unit,
        Some(digest(b'z')),
    ));
    assert!(rejection(&bad).detail.contains(RETIRED_LIFECYCLE_UNIT));

    // A control-plane unit with no member ships half a control plane.
    let mut bad = pinned_manifest();
    bad.units
        .control_plane
        .push("ramshared-supervisor.service".to_string());
    assert!(rejection(&bad).detail.contains("ships no member"));

    // A drop-in with no member is the other half of that defect.
    let mut bad = pinned_manifest();
    bad.units
        .dropins
        .push("cron.service.d/10-ramshared-control.conf".to_string());
    assert!(rejection(&bad).detail.contains("ships no member"));

    // A name declared twice is a malformed set, not a stronger declaration.
    let mut bad = pinned_manifest();
    let dropin = bad.units.dropins[0].clone();
    bad.units.dropins.push(dropin.clone());
    assert!(rejection(&bad).detail.contains("twice"));
    let mut bad = pinned_manifest();
    let unit = bad.units.control_plane[0].clone();
    bad.units.control_plane.push(unit);
    assert!(rejection(&bad).detail.contains("twice"));

    // A conffile would let a purge delete operator state (AUD-41).
    let mut bad = pinned_manifest();
    bad.configuration.conffiles.push("origin.conf".to_string());
    assert!(rejection(&bad).detail.contains("conffiles"));

    // A non-generated member without a digest is not verifiable.
    let mut bad = pinned_manifest();
    bad.contents[2].sha256 = None;
    assert!(rejection(&bad).detail.contains("no digest"));

    // A generated member carrying a digest describes bytes that do not
    // exist at declaration time.
    let mut bad = pinned_manifest();
    bad.contents[2].kind = ManifestKind::Generated;
    bad.contents[2].sha256 = Some(digest(b'g'));
    assert!(rejection(&bad).detail.contains("generated"));

    // A pin that disagrees with its own `bin` member describes a release
    // that does not exist.
    let mut bad = pinned_manifest();
    bad.build.cli_sha256 = digest(b'q');
    assert!(rejection(&bad).detail.contains("pinned build identity"));

    // Duplicate roles are refused rather than last-one-wins.
    let mut bad = pinned_manifest();
    let clone = bad.contents[2].clone();
    bad.contents.push(clone);
    assert!(rejection(&bad).detail.contains("twice"));

    // Oversized input is refused at the bound.
    let error = PayloadManifest::parse(&vec![b' '; MAX_PAYLOAD_MANIFEST_BYTES + 1])
        .expect_err("an oversized manifest must be refused");
    assert_eq!(error.reason, CustomerReason::UnsupportedSurface);

    // Malformed JSON is refused, never partially accepted.
    PayloadManifest::parse(b"{").expect_err("malformed JSON must be refused");
}

/// Per-member shape agreements each name their own cause. These are the
/// refusals `check_member_shape` and `check_member_digest` own: one member
/// is malformed in exactly one way at a time, and the document-level rules
/// never get a chance to disagree about it.
#[test]
fn member_shape_refusals_each_name_their_own_cause() {
    // Role outside the declared alphabet.
    let mut bad = pinned_manifest();
    bad.contents[2].role = "not a role!".to_string();
    assert!(
        rejection(&bad)
            .detail
            .contains("outside the declared alphabet")
    );

    // Mode that is not an installed octal mode.
    let mut bad = pinned_manifest();
    bad.contents[2].mode = "0999".to_string();
    assert!(
        rejection(&bad)
            .detail
            .contains("not an installed octal mode")
    );
    let mut bad = pinned_manifest();
    bad.contents[2].mode = "777".to_string();
    assert!(
        rejection(&bad)
            .detail
            .contains("not an installed octal mode")
    );

    // A member with no source is not a member.
    let mut bad = pinned_manifest();
    bad.contents[2].source = String::new();
    assert!(rejection(&bad).detail.contains("declares no source"));

    // A member that ships in no format ships nowhere.
    let mut bad = pinned_manifest();
    bad.contents[2].formats = Vec::new();
    assert!(rejection(&bad).detail.contains("ships in no format"));

    // The same format twice is one format declared twice, not more coverage.
    let mut bad = pinned_manifest();
    bad.contents[2].formats = vec![ManifestFormat::Deb, ManifestFormat::Deb];
    assert!(rejection(&bad).detail.contains("declares format deb twice"));

    // A non-generated member's digest must be a lowercase sha256.
    let mut bad = pinned_manifest();
    bad.contents[2].sha256 = Some("not-a-digest".to_string());
    assert!(rejection(&bad).detail.contains("not a lowercase sha256"));
}

/// The size bound is real and a document that satisfies every rule is
/// accepted, so a refusal above is never "the validator always refuses".
#[test]
fn manifest_accepts_the_document_the_builders_emit() {
    let manifest = pinned_manifest();
    let bytes = document(&manifest);
    assert!(bytes.len() <= MAX_PAYLOAD_MANIFEST_BYTES);
    let parsed = PayloadManifest::parse(&bytes).unwrap();
    assert_eq!(parsed, manifest);
    assert_eq!(parsed.architecture.as_str(), "amd64");
    assert_eq!(ManifestArchitecture::Arm64.as_str(), "arm64");
    assert!(parsed.build.source_date_epoch.is_some());
}

/// Product-identity predicates are exact: a digest is lowercase hex, a
/// commit is full, and a mode is an installed octal mode.
#[test]
fn identity_predicates_refuse_near_misses() {
    assert!(is_lowercase_sha256(&digest(CLI_NIBBLE)));
    assert!(!is_lowercase_sha256(&digest(CLI_NIBBLE).to_uppercase()));
    assert!(!is_lowercase_sha256("abc"));
    assert!(!is_lowercase_sha256(&format!("{}0", digest(CLI_NIBBLE))));

    assert!(is_full_commit(&hex(COMMIT_NIBBLE)));
    assert!(!is_full_commit("HEAD"));
    assert!(!is_full_commit(&hex(COMMIT_NIBBLE).to_uppercase()));
    assert!(!is_full_commit(&format!("{}0", hex(COMMIT_NIBBLE))));

    assert!(is_product_version("0.15.0"));
    assert!(is_product_version("0.15.0-rc1"));
    assert!(!is_product_version(""));
    assert!(!is_product_version(".hidden"));
    assert!(!is_product_version("0.15.0/slash"));

    assert!(is_installed_mode("0644"));
    assert!(is_installed_mode("0755"));
    assert!(!is_installed_mode("644"));
    assert!(!is_installed_mode("0999"));
    assert!(!is_installed_mode("06444"));

    assert!(is_dropin_role("docker.service.d/10-ramshared-control.conf"));
    assert!(!is_dropin_role("docker.service.d/"));
    assert!(!is_dropin_role("/etc/systemd/system/x.conf"));
    assert!(!is_dropin_role("docker.socket.d/10.conf"));

    assert!(is_role("scripts/safety/nbd-lifecycle.sh"));
    assert!(!is_role(""));
    assert!(!is_role("../escape"));
    assert!(!is_role("-leading-dash"));

    assert!(is_branch("feat/ramshared-v0.15.0-readiness"));
    assert!(!is_branch(""));
}
