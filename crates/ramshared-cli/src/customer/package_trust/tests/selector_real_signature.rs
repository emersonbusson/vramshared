// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_trust/tests/selector_real_signature.rs
//! Real prior-signature custody across selector intent persistence.

use super::*;
use crate::customer::package_prepare::PreparedRelease;
use crate::customer::package_source::DeclaredBundle;
use crate::customer::package_transition::AdmittedPackage;
use crate::customer::transition::PackagePending;
use sha2::Digest;

pub(super) const TEMPLATE: &str = "# generic\nNBD_LOWER_SINK=\nNBD_LOWER_SINK_TYPE=directory\nNBD_LOWER_SINK_IDENTITY_SHA256=\nNBD_LOWER_SINK_FS_BLOCK_BYTES=\nNBD_LOWER_SINK_BINDING=unbound\n";

pub(super) struct Scenario<'a, 'bundle> {
    pub root: &'a Path,
    pub parent: &'a Path,
    pub transaction: &'a AdmittedPackage<'bundle>,
    pub pending: &'a PackagePending,
    pub bundle: &'a DeclaredBundle,
    pub release: &'a PreparedRelease,
    pub signature: &'a VerifiedSignature,
    pub previous_signature: &'a VerifiedSignature,
    pub previous_policy: &'a Path,
}

fn digest(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn prior_release(root: &Path, input: &[u8]) {
    std::fs::create_dir_all(root.join("bin")).unwrap();
    for name in [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
    ] {
        std::fs::write(root.join(name), b"fixture").unwrap();
    }
    std::fs::write(root.join("INPUT_BUNDLE_SHA256SUMS"), input).unwrap();
    let provenance = serde_json::json!({
        "schema_version": crate::installed_image::INSTALLED_PROVENANCE_V2,
        "input_bundle_manifest_sha256": digest(input),
    });
    std::fs::write(root.join("INSTALL_PROVENANCE.json"), provenance.to_string()).unwrap();
    let names = [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "INPUT_BUNDLE_SHA256SUMS",
        "INSTALL_PROVENANCE.json",
    ];
    let manifest = names
        .iter()
        .map(|name| {
            format!(
                "{}  ./{name}\n",
                digest(&std::fs::read(root.join(name)).unwrap())
            )
        })
        .collect::<String>();
    std::fs::write(root.join("SHA256SUMS"), &manifest).unwrap();
    std::fs::write(
        root.join("INSTALLED_MANIFEST_SHA256"),
        format!("{}\n", digest(manifest.as_bytes())),
    )
    .unwrap();
}

fn matching_prior(root: &Path, bundle: &Path, input: &[u8]) {
    std::fs::remove_dir_all(root).unwrap();
    std::fs::create_dir(root).unwrap();
    let original = std::str::from_utf8(input).unwrap();
    for line in original.lines() {
        let (_, name) = line.split_once("  ./").unwrap();
        let target = root.join(name);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(bundle.join(name), target).unwrap();
    }
    std::fs::write(root.join("INPUT_BUNDLE_SHA256SUMS"), input).unwrap();
    let provenance = serde_json::json!({
        "schema_version": crate::installed_image::INSTALLED_PROVENANCE_V2,
        "input_bundle_manifest_sha256": digest(input),
    })
    .to_string();
    std::fs::write(root.join("INSTALL_PROVENANCE.json"), &provenance).unwrap();
    let manifest = format!(
        "{original}{}  ./INPUT_BUNDLE_SHA256SUMS\n{}  ./INSTALL_PROVENANCE.json\n",
        digest(input),
        digest(provenance.as_bytes())
    );
    std::fs::write(root.join("SHA256SUMS"), &manifest).unwrap();
    std::fs::write(
        root.join("INSTALLED_MANIFEST_SHA256"),
        format!("{}\n", digest(manifest.as_bytes())),
    )
    .unwrap();
}

fn bind_prior(
    root: &Path,
    input: &[u8],
    binding: &crate::customer::package_selector::derived_config::Binding<'_>,
) {
    let config = crate::customer::package_selector::derived_config::reconstruct(
        TEMPLATE.as_bytes(),
        binding,
        4096,
    )
    .unwrap();
    std::fs::write(root.join("scripts/safety/cascade.conf.example"), &config).unwrap();
    let provenance = serde_json::json!({
        "schema_version": crate::installed_image::INSTALLED_PROVENANCE_V2,
        "input_bundle_manifest_sha256": digest(input),
        "lower_sink": {
            "canonical_path": binding.path,
            "identity_sha256": binding.identity,
            "filesystem_block_bytes": binding.block_bytes,
        },
    })
    .to_string();
    std::fs::write(root.join("INSTALL_PROVENANCE.json"), &provenance).unwrap();
    let mut manifest = String::new();
    for line in std::str::from_utf8(input).unwrap().lines() {
        let (_, name) = line.split_once("  ./").unwrap();
        let hash = if name == "scripts/safety/cascade.conf.example" {
            digest(&config)
        } else {
            line[..64].to_owned()
        };
        manifest.push_str(&format!("{hash}  ./{name}\n"));
    }
    manifest.push_str(&format!(
        "{}  ./INPUT_BUNDLE_SHA256SUMS\n{}  ./INSTALL_PROVENANCE.json\n",
        digest(input),
        digest(provenance.as_bytes())
    ));
    std::fs::write(root.join("SHA256SUMS"), &manifest).unwrap();
    std::fs::write(
        root.join("INSTALLED_MANIFEST_SHA256"),
        format!("{}\n", digest(manifest.as_bytes())),
    )
    .unwrap();
}

pub(super) fn exercise(s: Scenario<'_, '_>) {
    let evidence = crate::customer::package_selector::PreviousEvidence {
        signature: s.previous_signature,
        transformation: None,
    };
    let input = std::fs::read(s.root.join("bundle/SHA256SUMS")).unwrap();
    assert_eq!(digest(&input), s.pending.old_hash);
    prior_release(&s.parent.join("prior-release"), &input);
    let selector = s.parent.join("current");
    symlink("prior-release", &selector).unwrap();
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            s.transaction,
            s.pending,
            s.bundle,
            s.release,
            s.signature,
            Some(&evidence),
            &selector,
        )
        .is_err(),
        "signed input must not authorize different installed binaries"
    );
    matching_prior(
        &s.parent.join("prior-release"),
        &s.root.join("bundle"),
        &input,
    );
    let policy = std::fs::read(s.previous_policy).unwrap();
    let changed = std::cell::Cell::new(false);
    assert!(
        crate::customer::package_selector::record_prepared_intent_with_hook(
            s.transaction,
            s.pending,
            s.bundle,
            s.release,
            (s.signature, Some(&evidence)),
            &selector,
            || {
                std::fs::write(s.previous_policy, b"release_keyring=/changed\n").unwrap();
                changed.set(true);
            },
        )
        .is_err()
    );
    assert!(changed.get());
    assert!(s.signature.revalidate().is_some());
    assert!(s.previous_signature.revalidate().is_none());
    let journal = s
        .transaction
        .guard()
        .paths()
        .transactions_root()
        .join("package-selector.json");
    assert!(journal.is_file());
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        Path::new("prior-release")
    );
    std::fs::write(s.previous_policy, &policy).unwrap();
    std::fs::remove_file(&journal).unwrap();
    let custody = crate::customer::package_selector::record_prepared_intent(
        s.transaction,
        s.pending,
        s.bundle,
        s.release,
        s.signature,
        Some(&evidence),
        &selector,
    )
    .unwrap();
    drop(custody);
    std::fs::remove_file(&journal).unwrap();
    let extra = s.parent.join("prior-release/unsigned-extra");
    std::fs::write(&extra, b"unexpected").unwrap();
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            s.transaction,
            s.pending,
            s.bundle,
            s.release,
            s.signature,
            Some(&evidence),
            &selector,
        )
        .is_err()
    );
    assert!(!journal.exists());
    std::fs::remove_file(extra).unwrap();
    let extra = s.parent.join("prior-release/unsigned-after-sync");
    let injected = std::cell::Cell::new(false);
    assert!(
        crate::customer::package_selector::record_prepared_intent_with_hook(
            s.transaction,
            s.pending,
            s.bundle,
            s.release,
            (s.signature, Some(&evidence)),
            &selector,
            || {
                std::fs::write(&extra, b"unexpected").unwrap();
                injected.set(true);
            },
        )
        .is_err()
    );
    assert!(injected.get());
    assert!(journal.is_file());
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        Path::new("prior-release")
    );
    std::fs::remove_file(extra).unwrap();
    std::fs::remove_file(&journal).unwrap();

    let sink_path = s.root.join("prior-sink");
    std::fs::create_dir(&sink_path).unwrap();
    let sink = crate::customer::package_selector::SinkWitness::observe(sink_path.to_str().unwrap())
        .unwrap();
    let template =
        std::fs::read(s.root.join("bundle/scripts/safety/cascade.conf.example")).unwrap();
    bind_prior(&s.parent.join("prior-release"), &input, &sink.binding());
    let transformed = crate::customer::package_selector::PreviousEvidence {
        signature: s.previous_signature,
        transformation: Some(crate::customer::package_selector::PreviousTransformation {
            role: "cascade.conf.example",
            template: &template,
            sink: &sink,
        }),
    };
    let custody = crate::customer::package_selector::record_prepared_intent(
        s.transaction,
        s.pending,
        s.bundle,
        s.release,
        s.signature,
        Some(&transformed),
        &selector,
    )
    .unwrap();
    drop(custody);
    std::fs::remove_file(&journal).unwrap();
    let mut wrong_template = template.clone();
    wrong_template.push(b'x');
    for (role, candidate) in [
        ("cascade.conf.example", wrong_template.as_slice()),
        ("origin.conf.example", template.as_slice()),
    ] {
        let invalid = crate::customer::package_selector::PreviousEvidence {
            signature: s.previous_signature,
            transformation: Some(crate::customer::package_selector::PreviousTransformation {
                role,
                template: candidate,
                sink: &sink,
            }),
        };
        assert!(
            crate::customer::package_selector::record_prepared_intent(
                s.transaction,
                s.pending,
                s.bundle,
                s.release,
                s.signature,
                Some(&invalid),
                &selector,
            )
            .is_err()
        );
        assert!(!journal.exists());
    }
    let oversized = vec![b'x'; crate::release_identity::MAX_SEALED_TEXT_BYTES + 1];
    let oversized_evidence = crate::customer::package_selector::PreviousEvidence {
        signature: s.previous_signature,
        transformation: Some(crate::customer::package_selector::PreviousTransformation {
            role: "cascade.conf.example",
            template: &oversized,
            sink: &sink,
        }),
    };
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            s.transaction,
            s.pending,
            s.bundle,
            s.release,
            s.signature,
            Some(&oversized_evidence),
            &selector,
        )
        .is_err()
    );
    assert!(!journal.exists());
    let declaration_path = s.parent.join("prior-release/release-manifest.json");
    let declaration = std::fs::read(&declaration_path).unwrap();
    std::fs::write(&declaration_path, b"tampered").unwrap();
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            s.transaction,
            s.pending,
            s.bundle,
            s.release,
            s.signature,
            Some(&transformed),
            &selector,
        )
        .is_err()
    );
    assert!(!journal.exists());
    std::fs::write(&declaration_path, declaration).unwrap();
    let mode = std::fs::metadata(&sink_path).unwrap().mode() & 0o7777;
    let changed = std::cell::Cell::new(false);
    assert!(
        crate::customer::package_selector::record_prepared_intent_with_hook(
            s.transaction,
            s.pending,
            s.bundle,
            s.release,
            (s.signature, Some(&transformed)),
            &selector,
            || {
                std::fs::set_permissions(&sink_path, std::fs::Permissions::from_mode(mode ^ 0o020))
                    .unwrap();
                changed.set(true);
            },
        )
        .is_err()
    );
    assert!(changed.get());
    assert!(journal.is_file());
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        Path::new("prior-release")
    );
    std::fs::set_permissions(&sink_path, std::fs::Permissions::from_mode(mode)).unwrap();
    std::fs::remove_file(&journal).unwrap();
    let other_path = s.root.join("other-prior-sink");
    std::fs::create_dir(&other_path).unwrap();
    let other =
        crate::customer::package_selector::SinkWitness::observe(other_path.to_str().unwrap())
            .unwrap();
    bind_prior(&s.parent.join("prior-release"), &input, &other.binding());
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            s.transaction,
            s.pending,
            s.bundle,
            s.release,
            s.signature,
            Some(&transformed),
            &selector,
        )
        .is_err()
    );
    assert!(!journal.exists());
    bind_prior(&s.parent.join("prior-release"), &input, &sink.binding());
    std::fs::write(
        s.parent
            .join("prior-release/scripts/safety/cascade.conf.example"),
        b"tampered",
    )
    .unwrap();
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            s.transaction,
            s.pending,
            s.bundle,
            s.release,
            s.signature,
            Some(&transformed),
            &selector,
        )
        .is_err()
    );

    // Restore prior release and drive the complete native sealing + atomic selector effect.
    bind_prior(&s.parent.join("prior-release"), &input, &sink.binding());
    assert!(!journal.exists());
    let custody = crate::customer::package_selector::record_prepared_intent(
        s.transaction,
        s.pending,
        s.bundle,
        s.release,
        s.signature,
        Some(&transformed),
        &selector,
    )
    .unwrap();
    assert!(
        crate::customer::package_selector::finalize_and_seal_release(
            &custody,
            &sink,
            "not-a-timestamp",
        )
        .is_err()
    );
    let sealed_hook_check = crate::customer::package_selector::finalize_and_seal_release_with_hook(
        &custody,
        &sink,
        "not-a-timestamp",
        || Ok(()),
    );
    assert!(sealed_hook_check.is_err());

    let other_seal_sink_path = s.root.join("other-seal-sink");
    std::fs::create_dir(&other_seal_sink_path).unwrap();
    let other_seal_sink = crate::customer::package_selector::SinkWitness::observe(
        other_seal_sink_path.to_str().unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(
        &other_seal_sink_path,
        std::fs::Permissions::from_mode(mode ^ 0o020),
    )
    .unwrap();
    assert!(
        crate::customer::package_selector::finalize_and_seal_release(
            &custody,
            &other_seal_sink,
            "2026-11-22T07:20:00Z",
        )
        .is_err()
    );

    let sealed = crate::customer::package_selector::finalize_and_seal_release(
        &custody,
        &sink,
        "2026-11-22T07:20:00Z",
    )
    .unwrap();
    assert!(sealed.revalidate().is_ok());

    let tx_paths = s.transaction.guard().paths();
    let effect_journal = tx_paths
        .transactions_root()
        .join("package-selector-effect.json");
    let mut noop1 = || Ok(());
    let mut noop2 = || Ok(());
    let mut abort_before = || {
        Err(crate::customer::transition::recovery_refusal(
            "injected before rename",
        ))
    };
    let mut hooks_before = crate::customer::package_selector::SelectorEffectHooks {
        after_stage: &mut noop1,
        before_rename: &mut abort_before,
        after_rename: &mut noop2,
    };
    assert!(
        crate::customer::package_selector::publish_sealed_selector_with_hooks(
            &custody,
            &sealed,
            &mut hooks_before,
        )
        .is_err()
    );
    assert_eq!(
        crate::customer::package_selector::inspect_effect_journal(tx_paths)
            .unwrap()
            .unwrap()
            .phase,
        crate::customer::package_selector::SelectorEffectPhase::RolledBack
    );
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        Path::new("prior-release")
    );
    std::fs::remove_file(&effect_journal).unwrap();

    let pre_staging = s
        .parent
        .join(format!(".current.{}.staging", std::process::id()));
    std::fs::write(&pre_staging, b"collision").unwrap();
    assert!(crate::customer::package_selector::publish_sealed_selector(&custody, &sealed).is_err());
    std::fs::remove_file(&pre_staging).unwrap();

    // Tampered staging link after stage triggers staging identity failure AND rollback refusal.
    let mut tamper_stage = || {
        let replacement = s.parent.join("tampered-stage-src");
        symlink("tampered-stage", &replacement).unwrap();
        std::fs::rename(&replacement, &pre_staging).unwrap();
        Ok(())
    };
    let mut noop3 = || Ok(());
    let mut noop4 = || Ok(());
    let mut hooks_stage_tamper = crate::customer::package_selector::SelectorEffectHooks {
        after_stage: &mut tamper_stage,
        before_rename: &mut noop3,
        after_rename: &mut noop4,
    };
    assert!(
        crate::customer::package_selector::publish_sealed_selector_with_hooks(
            &custody,
            &sealed,
            &mut hooks_stage_tamper,
        )
        .is_err()
    );
    std::fs::remove_file(&pre_staging).unwrap();
    std::fs::remove_file(&effect_journal).unwrap();

    let verified =
        crate::customer::package_selector::publish_sealed_selector(&custody, &sealed).unwrap();
    assert_eq!(
        verified.phase,
        crate::customer::package_selector::SelectorEffectPhase::Verified
    );
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        s.release.tree.path().strip_prefix(s.parent).unwrap()
    );
    assert!(crate::installed_image::verify_release_manifest(&selector).is_some());
    assert_eq!(
        crate::installed_image::observe_installed_input_bundle_digest(&selector).as_deref(),
        Some(s.pending.new_hash.as_str())
    );
    assert!(crate::customer::package_selector::publish_sealed_selector(&custody, &sealed).is_err());
    assert_eq!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Complete,
            Some((&sealed, s.signature)),
            None,
        )
        .unwrap(),
        crate::customer::package_selector::SelectorRecoveryOutcome::Verified
    );

    let mut pub_rec = crate::customer::package_selector::inspect_effect_journal(tx_paths)
        .unwrap()
        .unwrap();
    pub_rec.phase = crate::customer::package_selector::SelectorEffectPhase::SelectorPublished;
    std::fs::write(
        &effect_journal,
        format!("{}\n", serde_json::to_string(&pub_rec).unwrap()),
    )
    .unwrap();
    assert_eq!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Complete,
            Some((&sealed, s.signature)),
            None,
        )
        .unwrap(),
        crate::customer::package_selector::SelectorRecoveryOutcome::Verified
    );

    // Reconcile rollback cleanly restores `prior-release` even after Verified publication.
    let rb = crate::customer::package_selector::reconcile_selector(
        s.transaction.guard(),
        s.pending,
        crate::customer::package_selector::SelectorRecoveryAction::Rollback,
        None,
        custody.previous_custody(),
    )
    .unwrap();
    assert_eq!(
        rb,
        crate::customer::package_selector::SelectorRecoveryOutcome::RolledBack
    );
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        Path::new("prior-release")
    );
    assert!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Complete,
            Some((&sealed, s.signature)),
            None,
        )
        .is_err()
    );

    // Reconcile Complete from IntentBound (no staging symlink yet) transitions cleanly to Verified.
    let mut ib_rec = crate::customer::package_selector::inspect_effect_journal(tx_paths)
        .unwrap()
        .unwrap();
    let cur_meta = std::fs::symlink_metadata(&selector).unwrap();
    if let Some(prev) = ib_rec.previous.as_mut() {
        prev.entry_identity = (cur_meta.dev(), cur_meta.ino());
    }
    ib_rec.phase = crate::customer::package_selector::SelectorEffectPhase::IntentBound;
    ib_rec.staging_identity = None;
    std::fs::write(
        &effect_journal,
        format!("{}\n", serde_json::to_string(&ib_rec).unwrap()),
    )
    .unwrap();
    assert_eq!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Complete,
            Some((&sealed, s.signature)),
            None,
        )
        .unwrap(),
        crate::customer::package_selector::SelectorRecoveryOutcome::Verified
    );

    // Mismatched signature, record input hash, and live selector inode fail Complete guards.
    let cur_verified = crate::customer::package_selector::inspect_effect_journal(tx_paths)
        .unwrap()
        .unwrap();
    let mut bad_sig_rec = cur_verified.clone();
    bad_sig_rec.input_manifest_sha256 = "0".repeat(64);
    std::fs::write(
        &effect_journal,
        format!("{}\n", serde_json::to_string(&bad_sig_rec).unwrap()),
    )
    .unwrap();
    assert!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Complete,
            Some((&sealed, s.signature)),
            None,
        )
        .is_err()
    );
    let mut bad_hash_rec = cur_verified.clone();
    bad_hash_rec.sealed_manifest_sha256 = "0".repeat(64);
    std::fs::write(
        &effect_journal,
        format!("{}\n", serde_json::to_string(&bad_hash_rec).unwrap()),
    )
    .unwrap();
    assert!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Complete,
            Some((&sealed, s.signature)),
            None,
        )
        .is_err()
    );
    std::fs::write(
        &effect_journal,
        format!("{}\n", serde_json::to_string(&cur_verified).unwrap()),
    )
    .unwrap();
    assert_eq!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Rollback,
            None,
            custody.previous_custody(),
        )
        .unwrap(),
        crate::customer::package_selector::SelectorRecoveryOutcome::RolledBack
    );

    // SelectorStaged -> Complete with existing staging symlink, and mismatching staging_identity.
    let mut staged_rec = cur_verified;
    staged_rec.phase = crate::customer::package_selector::SelectorEffectPhase::SelectorStaged;
    let staged_target = s.release.tree.path().strip_prefix(s.parent).unwrap();
    let staged_path = s.parent.join(&staged_rec.staging_name);
    let rb_cur_meta = std::fs::symlink_metadata(&selector).unwrap();
    if let Some(prev) = staged_rec.previous.as_mut() {
        prev.entry_identity = (rb_cur_meta.dev(), rb_cur_meta.ino());
    }
    symlink(staged_target, &staged_path).unwrap();
    let staged_meta = std::fs::symlink_metadata(&staged_path).unwrap();
    staged_rec.staging_identity = Some((999, 999));
    std::fs::write(
        &effect_journal,
        format!("{}\n", serde_json::to_string(&staged_rec).unwrap()),
    )
    .unwrap();
    assert!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Complete,
            Some((&sealed, s.signature)),
            None,
        )
        .is_err()
    );
    staged_rec.staging_identity = Some((staged_meta.dev(), staged_meta.ino()));
    std::fs::write(
        &effect_journal,
        format!("{}\n", serde_json::to_string(&staged_rec).unwrap()),
    )
    .unwrap();
    assert_eq!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Complete,
            Some((&sealed, s.signature)),
            None,
        )
        .unwrap(),
        crate::customer::package_selector::SelectorRecoveryOutcome::Verified
    );

    assert_eq!(
        crate::customer::package_selector::reconcile_selector(
            s.transaction.guard(),
            s.pending,
            crate::customer::package_selector::SelectorRecoveryAction::Rollback,
            None,
            custody.previous_custody(),
        )
        .unwrap(),
        crate::customer::package_selector::SelectorRecoveryOutcome::RolledBack
    );
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        Path::new("prior-release")
    );
    crate::customer::package_selector::remove_owned_unselected_release(
        s.release.tree.path(),
        s.release.tree.identity(),
        &selector,
    )
    .unwrap();
    assert!(!s.release.tree.path().exists());
}
