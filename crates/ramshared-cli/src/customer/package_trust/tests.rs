// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_trust/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[test]
fn signature_status_requires_one_good_unexpired_unrevoked_signature() {
    let good = b"[GNUPG:] NEWSIG\n[GNUPG:] KEY_CONSIDERED A 0\n[GNUPG:] SIG_ID x\n[GNUPG:] GOODSIG A Test\n[GNUPG:] VALIDSIG A 0\n";
    assert!(valid_signature_status(good));
    for bad in [
        &b""[..],
        &b"[GNUPG:] NEWSIG\n[GNUPG:] EXPKEYSIG A Test\n[GNUPG:] VALIDSIG A 0\n"[..],
        &b"[GNUPG:] NEWSIG\n[GNUPG:] REVKEYSIG A Test\n[GNUPG:] VALIDSIG A 0\n"[..],
        &b"[GNUPG:] NEWSIG\n[GNUPG:] GOODSIG A Test\n[GNUPG:] VALIDSIG A 0"[..],
        &b"[GNUPG:] NEWSIG\n[GNUPG:] GOODSIG A Test\n[GNUPG:] VALIDSIG A 0\n[GNUPG:] NEWSIG\n"[..],
        &b"[GNUPG:] NEWSIG\n[GNUPG:] GOODSIG A Test\n[GNUPG:] VALIDSIG A 0\n[GNUPG:] UNKNOWN\n"[..],
    ] {
        assert!(!valid_signature_status(bad));
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ramshared-trust-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
    fn owner(&self) -> u32 {
        std::fs::metadata(&self.0).unwrap().uid()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn trusted_input_rejects_mutation_replacement_and_links() {
    let fixture = Fixture::new();
    let path = fixture.0.join("manifest");
    std::fs::write(&path, b"original").unwrap();
    let mut input = TrustedFile::open_for_owner(&path, 64, fixture.owner()).unwrap();
    assert!(input.revalidate().is_some());
    std::fs::write(&path, b"modified").unwrap();
    assert!(input.revalidate().is_none());
    std::fs::write(&path, b"original").unwrap();
    let saved = fixture.0.join("saved");
    std::fs::rename(&path, &saved).unwrap();
    std::fs::write(&path, b"original").unwrap();
    assert!(input.revalidate().is_none());
    let link = fixture.0.join("link");
    symlink(&path, &link).unwrap();
    assert!(TrustedFile::open_for_owner(&link, 64, fixture.owner()).is_none());
    let alias = fixture.0.join("alias");
    std::fs::hard_link(&path, &alias).unwrap();
    assert!(TrustedFile::open_for_owner(&path, 64, fixture.owner()).is_none());
}

#[test]
fn trusted_input_rejects_writable_ancestors_and_size_overflow() {
    let fixture = Fixture::new();
    let path = fixture.0.join("manifest");
    std::fs::write(&path, b"content").unwrap();
    assert!(TrustedFile::open_for_owner(&path, 6, fixture.owner()).is_none());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(TrustedFile::open_for_owner(&path, 64, fixture.owner()).is_none());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(TrustedFile::open_for_owner(&path, 64, fixture.owner()).is_none());
    std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        TrustedFile::open_for_owner(&fixture.0.join("../elsewhere"), 64, fixture.owner()).is_none()
    );
}

#[test]
fn trusted_input_rejects_replaced_parent_even_with_same_file_inode() {
    let fixture = Fixture::new();
    let parent = fixture.0.join("parent");
    let old_parent = fixture.0.join("old-parent");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("manifest");
    std::fs::write(&path, b"content").unwrap();
    let mut input = TrustedFile::open_for_owner(&path, 64, fixture.owner()).unwrap();
    std::fs::rename(&parent, &old_parent).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::rename(old_parent.join("manifest"), &path).unwrap();
    assert!(input.revalidate().is_none());
    assert_eq!(std::fs::read(&path).unwrap(), b"content");
}

#[test]
fn trusted_input_refuses_redirected_parent_and_excessive_path_depth() {
    let fixture = Fixture::new();
    let actual = fixture.0.join("actual");
    std::fs::create_dir(&actual).unwrap();
    std::fs::write(actual.join("manifest"), b"content").unwrap();
    let redirect = fixture.0.join("redirect");
    symlink(&actual, &redirect).unwrap();
    assert!(TrustedFile::open_for_owner(&redirect.join("manifest"), 64, fixture.owner()).is_none());
    let long = fixture.0.join(
        (0..MAX_TRUST_PATH_DEPTH)
            .map(|_| "a")
            .collect::<Vec<_>>()
            .join("/"),
    );
    assert!(TrustedFile::open_for_owner(&long, 64, fixture.owner()).is_none());
    assert!(TrustedFile::open_for_owner(&actual.join("manifest"), 0, fixture.owner()).is_none());
}

#[test]
fn trusted_input_refuses_parent_mode_change() {
    let fixture = Fixture::new();
    let parent = fixture.0.join("parent");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("manifest");
    std::fs::write(&path, b"content").unwrap();
    let mut input = TrustedFile::open_for_owner(&path, 64, fixture.owner()).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(input.revalidate().is_none());
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn trusted_input_refuses_parent_replacement_after_content_check() {
    let fixture = Fixture::new();
    let parent = fixture.0.join("parent");
    let old_parent = fixture.0.join("old-parent");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("manifest");
    std::fs::write(&path, b"content").unwrap();
    let mut input = TrustedFile::open_for_owner(&path, 64, fixture.owner()).unwrap();
    assert!(
        input
            .revalidate_with_hook(|| {
                std::fs::rename(&parent, &old_parent).unwrap();
                std::fs::create_dir(&parent).unwrap();
                std::fs::rename(old_parent.join("manifest"), &path).unwrap();
            })
            .is_none()
    );
    assert_eq!(std::fs::read(path).unwrap(), b"content");
}

#[test]
fn trust_snapshot_preserves_bytes_and_refuses_writes() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let fixture = Fixture::new();
    let path = fixture.0.join("manifest");
    std::fs::write(&path, b"signed content").unwrap();
    let mut input = TrustedFile::open_for_owner(&path, 64, fixture.owner()).unwrap();
    let mut snapshot = input.snapshot().unwrap();
    assert!(snapshot.write_all(b"tamper").is_err());
    assert!(snapshot.set_len(0).is_err());
    assert!(snapshot.set_len(64).is_err());
    std::fs::write(&path, b"new content").unwrap();
    snapshot.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    snapshot.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"signed content");
    assert!(input.snapshot().is_none());
}

#[test]
fn trust_snapshot_refuses_oversized_metadata() {
    let fixture = Fixture::new();
    let path = fixture.0.join("manifest");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_TRUST_SNAPSHOT_BYTES + 1).unwrap();
    let mut input =
        TrustedFile::open_for_owner(&path, MAX_TRUST_SNAPSHOT_BYTES + 1, fixture.owner()).unwrap();
    assert!(input.snapshot().is_none());
}

#[test]
fn policy_keyring_refuses_ambiguous_entries() {
    assert_eq!(
        keyring_from_policy(b"enabled=1\nrelease_keyring=/etc/ramshared/keyring.gpg\n"),
        Some(PathBuf::from("/etc/ramshared/keyring.gpg"))
    );
    for input in [
        "enabled=1\n",
        "release_keyring=relative\n",
        "release_keyring=/first\nrelease_keyring=/second\n",
        "release_keyring=/first\nrelease_keyring=\n",
        "release_keyring=/first\0bad\n",
        "release_keyring=/a/../bad\n",
        "release_keyring =/bad\n",
    ] {
        assert!(keyring_from_policy(input.as_bytes()).is_none(), "{input:?}");
    }
    assert!(keyring_from_policy(&[0xff]).is_none());
}

#[test]
fn signature_revalidation_refuses_busy_custody_without_waiting() {
    use std::sync::mpsc;
    use std::time::Duration;
    let fixture = Fixture::new();
    let policy = fixture.0.join("policy");
    let keyring = fixture.0.join("keyring");
    std::fs::write(&policy, b"policy").unwrap();
    std::fs::write(&keyring, b"keyring").unwrap();
    let authority = VerifiedSignature {
        manifest_sha256: "a".repeat(64),
        custody: SignatureCustody::Held(Box::new(Mutex::new(SignatureInputs {
            policy: Some(TrustedFile::open_for_owner(&policy, 64, fixture.owner()).unwrap()),
            keyring: TrustedFile::open_for_owner(&keyring, 64, fixture.owner()).unwrap(),
            verification: VerificationSnapshots {
                verifier: File::open(&policy).unwrap(),
                keyring: File::open(&keyring).unwrap(),
                signature: File::open(&policy).unwrap(),
                manifest: File::open(&policy).unwrap(),
            },
        }))),
    };
    let SignatureCustody::Held(inputs) = &authority.custody else {
        panic!("held custody required")
    };
    let guard = inputs.lock().unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            tx.send((
                authority.revalidate().is_none(),
                authority.trust_reference().is_none(),
            ))
            .unwrap();
        });
        let observed = rx.recv_timeout(Duration::from_secs(1));
        drop(guard);
        assert_eq!(observed.unwrap(), (true, true));
    });
}

#[test]
fn sealed_verifier_refuses_failure_and_reaps_timeout() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};
    let fixture = Fixture::new();
    let input_path = fixture.0.join("input");
    std::fs::write(&input_path, b"input").unwrap();
    let mut input = TrustedFile::open_for_owner(&input_path, 64, fixture.owner()).unwrap();
    let snapshot = input.snapshot().unwrap();
    let failing_path = fixture.0.join("failing");
    std::fs::write(&failing_path, b"#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&failing_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut failing =
        TrustedFile::open_for_owner(&failing_path, MAX_TRUST_SNAPSHOT_BYTES, fixture.owner())
            .unwrap();
    let failing = failing.snapshot().unwrap();
    assert!(
        verify_sealed_gpgv_with(
            &failing,
            &snapshot,
            &snapshot,
            &snapshot,
            Duration::from_secs(2),
            |_| {}
        )
        .is_none()
    );

    let script_path = fixture.0.join("stalled");
    std::fs::write(&script_path, b"#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut script = TrustedFile::open_for_owner(&script_path, 128, fixture.owner()).unwrap();
    let script = script.snapshot().unwrap();
    let child = AtomicU32::new(0);
    let started = Instant::now();
    assert!(
        verify_sealed_gpgv_with(
            &script,
            &snapshot,
            &snapshot,
            &snapshot,
            Duration::from_millis(150),
            |pid| child.store(pid, Ordering::SeqCst)
        )
        .is_none()
    );
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert!(started.elapsed() < Duration::from_secs(5));
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.load(Ordering::SeqCst)).unwrap())
        .unwrap();
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
}

fn exercise_signed_preparation(
    root: &Path,
    bundle: &crate::customer::package_source::DeclaredBundle,
    signature: &VerifiedSignature,
    policy: &Path,
    previous_signature: &VerifiedSignature,
    previous_policy: &Path,
) {
    use crate::customer::CustomerState;
    use crate::customer::package::ManifestArchitecture;
    use crate::customer::package_admission::{LifecycleOwnership, LifecycleSnapshot};
    use crate::customer::package_prepare::{
        PreparePaths, prepare_admitted, prepare_admitted_with_hooks, resume,
    };
    use crate::customer::package_transition::{PackageObservation, begin_verified_package_held};
    use crate::customer::plan::ReleaseIdentity;
    use crate::customer::transition::{
        OperationOwner, PackageAction, PackagePending, TransitionPaths, acquire_transition,
        read_pending,
    };

    let stage = root.join("signed-stage");
    let parent = root.join("signed-release-parent");
    std::fs::create_dir(&stage).unwrap();
    std::fs::create_dir(&parent).unwrap();
    let meta = std::fs::metadata(&parent).unwrap();
    let paths = TransitionPaths::under(&root.join("signed-transaction"));
    let pending = PackagePending {
        target_release: Some("v1".into()),
        action: PackageAction::Upgrade,
        old_hash: previous_signature.manifest_sha256().to_owned(),
        new_hash: signature.manifest_sha256().to_owned(),
        issued_unix: 1,
    };
    let observation = PackageObservation {
        snapshot: LifecycleSnapshot {
            state: CustomerState::InstalledOff,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: true,
        },
        release: ReleaseIdentity::new("0.15.0"),
        old_hash: pending.old_hash.clone(),
        new_hash: pending.new_hash.clone(),
    };
    let streamed = bundle
        .verify_streamed_for(u64::MAX, u64::MAX, ManifestArchitecture::Amd64)
        .unwrap();
    let original_policy = std::fs::read(policy).unwrap();
    assert_eq!(
        streamed.authenticated_release_identity(signature, &mut || None),
        Some("v1")
    );
    let other_signature = VerifiedSignature::fixture("f".repeat(64));
    assert!(
        streamed
            .authenticated_release_identity(&other_signature, &mut || None)
            .is_none()
    );
    let identity_path = root.join("bundle/RELEASE_VERSION");
    let original_identity = std::fs::read(&identity_path).unwrap();
    std::fs::write(&identity_path, b"v2\n").unwrap();
    assert!(
        streamed
            .authenticated_release_identity(signature, &mut || None)
            .is_none()
    );
    std::fs::write(&identity_path, &original_identity).unwrap();
    std::fs::write(policy, b"release_keyring=/changed\n").unwrap();
    assert!(
        streamed
            .authenticated_release_identity(signature, &mut || None)
            .is_none()
    );
    std::fs::write(policy, &original_policy).unwrap();
    let mut wrong_target = pending.clone();
    wrong_target.target_release = Some("v2".into());
    assert!(
        begin_verified_package_held(
            &paths,
            &wrong_target,
            Some(&streamed),
            Some(signature),
            &mut || Ok(observation.clone()),
            &mut || None,
        )
        .is_err()
    );
    assert!(read_pending(&paths).unwrap().is_none());
    let mut missing_target = pending.clone();
    missing_target.target_release = None;
    assert!(
        begin_verified_package_held(
            &paths,
            &missing_target,
            Some(&streamed),
            Some(signature),
            &mut || panic!("missing target must be refused before observation"),
            &mut || None,
        )
        .is_err()
    );
    assert!(read_pending(&paths).unwrap().is_none());
    let mut observations = 0;
    assert!(
        begin_verified_package_held(
            &paths,
            &pending,
            Some(&streamed),
            Some(signature),
            &mut || {
                observations += 1;
                if observations == 2 {
                    std::fs::write(policy, b"release_keyring=/changed\n").unwrap();
                }
                Ok(observation.clone())
            },
            &mut || None,
        )
        .is_err()
    );
    assert!(read_pending(&paths).unwrap().is_none());
    std::fs::write(policy, &original_policy).unwrap();
    let admitted = begin_verified_package_held(
        &paths,
        &pending,
        Some(&streamed),
        Some(signature),
        &mut || Ok(observation.clone()),
        &mut || None,
    )
    .unwrap();
    assert_eq!(admitted.candidate_release(), Some("v1"));
    let request = PreparePaths {
        stage: &stage,
        parent: &parent,
        parent_identity: (meta.dev(), meta.ino()),
        name: "release",
        reserve_bytes: 0,
    };
    assert_eq!(read_pending(&paths).unwrap(), Some(pending.clone()));
    assert!(resume(admitted.guard(), &wrong_target, bundle, signature, &request).is_err());
    assert_eq!(read_pending(&paths).unwrap(), Some(pending.clone()));
    let legacy_paths = TransitionPaths::under(&root.join("legacy-target-transaction"));
    let legacy_guard = acquire_transition(OperationOwner::Package, &legacy_paths).unwrap();
    crate::customer::transition::enroll_pending(&legacy_guard, &missing_target).unwrap();
    assert!(resume(&legacy_guard, &missing_target, bundle, signature, &request).is_err());
    assert_eq!(read_pending(&legacy_paths).unwrap(), Some(missing_target));
    drop(legacy_guard);
    std::fs::write(policy, b"release_keyring=/changed\n").unwrap();
    assert!(prepare_admitted(&admitted, &pending, bundle, signature, &request).is_err());
    assert!(std::fs::read_dir(&stage).unwrap().next().is_none());
    std::fs::write(policy, &original_policy).unwrap();
    assert!(
        prepare_admitted_with_hooks(
            &admitted,
            &pending,
            bundle,
            signature,
            &request,
            || std::fs::write(policy, b"release_keyring=/changed\n").unwrap(),
            || {},
        )
        .is_err()
    );
    assert!(!parent.join("release").exists());
    std::fs::write(policy, &original_policy).unwrap();
    let release = resume(admitted.guard(), &pending, bundle, signature, &request).unwrap();
    assert_eq!(release.members.len(), bundle.stage_roles().unwrap().len());
    assert_eq!(
        std::fs::read(
            release
                .tree
                .path()
                .join("scripts/safety/origin.conf.example")
        )
        .unwrap(),
        b"origin.conf.example"
    );
    drop(release);
    drop(admitted);
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    let reopened = resume(&guard, &pending, bundle, signature, &request).unwrap();
    assert_eq!(reopened.members.len(), bundle.stage_roles().unwrap().len());
    drop(reopened);
    drop(guard);

    let stage_sel = root.join("signed-stage-selector");
    let parent_sel = root.join("signed-parent-selector");
    std::fs::create_dir(&stage_sel).unwrap();
    std::fs::create_dir(&parent_sel).unwrap();
    let meta_sel = std::fs::metadata(&parent_sel).unwrap();
    let paths_sel = TransitionPaths::under(&root.join("signed-transaction-selector"));
    let admitted_sel = begin_verified_package_held(
        &paths_sel,
        &pending,
        Some(&streamed),
        Some(signature),
        &mut || Ok(observation.clone()),
        &mut || None,
    )
    .unwrap();
    let request_sel = PreparePaths {
        stage: &stage_sel,
        parent: &parent_sel,
        parent_identity: (meta_sel.dev(), meta_sel.ino()),
        name: "release",
        reserve_bytes: 0,
    };
    let release_sel =
        prepare_admitted(&admitted_sel, &pending, bundle, signature, &request_sel).unwrap();
    selector_real_signature::exercise(selector_real_signature::Scenario {
        root,
        parent: &parent_sel,
        transaction: &admitted_sel,
        pending: &pending,
        bundle,
        release: &release_sel,
        signature,
        previous_signature,
        previous_policy,
    });
    drop(release_sel);
    drop(admitted_sel);

    let stage2 = root.join("signed-stage-after-tree");
    let parent2 = root.join("signed-parent-after-tree");
    std::fs::create_dir(&stage2).unwrap();
    std::fs::create_dir(&parent2).unwrap();
    let meta2 = std::fs::metadata(&parent2).unwrap();
    let paths2 = TransitionPaths::under(&root.join("signed-transaction-after-tree"));
    let admitted2 = begin_verified_package_held(
        &paths2,
        &pending,
        Some(&streamed),
        Some(signature),
        &mut || Ok(observation.clone()),
        &mut || None,
    )
    .unwrap();
    let request2 = PreparePaths {
        stage: &stage2,
        parent: &parent2,
        parent_identity: (meta2.dev(), meta2.ino()),
        name: "release",
        reserve_bytes: 0,
    };
    assert!(
        prepare_admitted_with_hooks(
            &admitted2,
            &pending,
            bundle,
            signature,
            &request2,
            || {},
            || std::fs::write(policy, b"release_keyring=/changed\n").unwrap(),
        )
        .is_err()
    );
    assert!(parent2.join("release").is_dir());
    assert!(!parent2.join("release/bin/ramshared").exists());
    std::fs::write(policy, &original_policy).unwrap();
    let tree = crate::customer::package_tree::resume_tree(
        admitted2.guard(),
        &pending,
        bundle,
        &parent2,
        (meta2.dev(), meta2.ino()),
        "release",
        0,
    )
    .unwrap();
    let mut published = 0;
    assert!(
        crate::customer::package_publish::inventory::publish_owned_all_with_hook(
            admitted2.guard(),
            &pending,
            bundle,
            &tree,
            signature,
            0,
            &mut |count| {
                published = count;
                if count == 1 {
                    std::fs::write(policy, b"release_keyring=/changed\n").unwrap();
                }
            },
        )
        .is_err()
    );
    assert_eq!(published, 1);
    assert!(crate::customer::package_publish::publication_journal_present(&paths2).unwrap());
    std::fs::write(policy, &original_policy).unwrap();
    let recovered = resume(admitted2.guard(), &pending, bundle, signature, &request2).unwrap();
    assert_eq!(recovered.members.len(), bundle.stage_roles().unwrap().len());

    let stage3 = root.join("signed-stage-before-link");
    let parent3 = root.join("signed-parent-before-link");
    std::fs::create_dir(&stage3).unwrap();
    std::fs::create_dir(&parent3).unwrap();
    let meta3 = std::fs::metadata(&parent3).unwrap();
    let paths3 = TransitionPaths::under(&root.join("signed-transaction-before-link"));
    let admitted3 = begin_verified_package_held(
        &paths3,
        &pending,
        Some(&streamed),
        Some(signature),
        &mut || Ok(observation.clone()),
        &mut || None,
    )
    .unwrap();
    let request3 = PreparePaths {
        stage: &stage3,
        parent: &parent3,
        parent_identity: (meta3.dev(), meta3.ino()),
        name: "release",
        reserve_bytes: 0,
    };
    assert!(
        prepare_admitted_with_hooks(
            &admitted3,
            &pending,
            bundle,
            signature,
            &request3,
            || {},
            || std::fs::write(policy, b"release_keyring=/changed\n").unwrap(),
        )
        .is_err()
    );
    std::fs::write(policy, &original_policy).unwrap();
    let tree3 = crate::customer::package_tree::resume_tree(
        admitted3.guard(),
        &pending,
        bundle,
        &parent3,
        (meta3.dev(), meta3.ino()),
        "release",
        0,
    )
    .unwrap();
    let role = bundle.stage_roles().unwrap().first().unwrap().clone();
    let destination = bundle.publication_member(&role).unwrap().0;
    assert!(
        crate::customer::package_publish::publish_signed_owned_with_before_link(
            admitted3.guard(),
            &pending,
            bundle,
            &tree3,
            &role,
            signature,
            || {
                std::fs::write(policy, b"release_keyring=/changed\n").unwrap();
                Ok(())
            },
        )
        .is_err()
    );
    assert!(!tree3.path().join(destination).exists());
    assert!(crate::customer::package_publish::publication_journal_present(&paths3).unwrap());
    std::fs::write(policy, &original_policy).unwrap();
    let absent = crate::customer::package_publish::inventory::resume_owned_all(
        admitted3.guard(),
        &pending,
        bundle,
        &tree3,
        signature,
        0,
    )
    .unwrap_err();
    assert!(absent.detail.contains("effect may have occurred"));
    assert!(crate::customer::package_publish::publication_journal_present(&paths3).unwrap());

    let stage4 = root.join("signed-stage-after-link");
    let parent4 = root.join("signed-parent-after-link");
    std::fs::create_dir(&stage4).unwrap();
    std::fs::create_dir(&parent4).unwrap();
    let meta4 = std::fs::metadata(&parent4).unwrap();
    let paths4 = TransitionPaths::under(&root.join("signed-transaction-after-link"));
    let admitted4 = begin_verified_package_held(
        &paths4,
        &pending,
        Some(&streamed),
        Some(signature),
        &mut || Ok(observation.clone()),
        &mut || None,
    )
    .unwrap();
    let request4 = PreparePaths {
        stage: &stage4,
        parent: &parent4,
        parent_identity: (meta4.dev(), meta4.ino()),
        name: "release",
        reserve_bytes: 0,
    };
    assert!(
        prepare_admitted_with_hooks(
            &admitted4,
            &pending,
            bundle,
            signature,
            &request4,
            || {},
            || std::fs::write(policy, b"release_keyring=/changed\n").unwrap(),
        )
        .is_err()
    );
    std::fs::write(policy, &original_policy).unwrap();
    let tree4 = crate::customer::package_tree::resume_tree(
        admitted4.guard(),
        &pending,
        bundle,
        &parent4,
        (meta4.dev(), meta4.ino()),
        "release",
        0,
    )
    .unwrap();
    let role4 = bundle.stage_roles().unwrap().first().unwrap().clone();
    let destination4 = bundle.publication_member(&role4).unwrap().0;
    assert!(
        crate::customer::package_publish::publish_signed_owned_with_after_link(
            admitted4.guard(),
            &pending,
            bundle,
            &tree4,
            &role4,
            signature,
            || {
                std::fs::write(policy, b"release_keyring=/changed\n").unwrap();
                Ok(())
            },
        )
        .is_err()
    );
    assert!(tree4.path().join(destination4).is_file());
    assert!(crate::customer::package_publish::publication_journal_present(&paths4).unwrap());
    let journal4 =
        std::fs::read_to_string(paths4.transactions_root().join("package-publish.json")).unwrap();
    let last: serde_json::Value = serde_json::from_str(journal4.lines().last().unwrap()).unwrap();
    assert_eq!(last["phase"], "prepared");
    std::fs::write(policy, &original_policy).unwrap();
    let recovered4 = resume(admitted4.guard(), &pending, bundle, signature, &request4).unwrap();
    assert_eq!(
        recovered4.members.len(),
        bundle.stage_roles().unwrap().len()
    );
}

fn make_declared_bundle(bundle: &Path) {
    use crate::customer::package::{
        ManifestFormat, ManifestKind, ManifestMember, tests::pinned_manifest,
    };
    use sha2::Digest;
    let digest = |bytes: &[u8]| -> String {
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    let mut elf = vec![0u8; 64];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&3u16.to_le_bytes());
    elf[18..20].copy_from_slice(&62u16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    let mut payload = pinned_manifest();
    let mut cascade = payload
        .contents
        .iter()
        .find(|member| member.role == "origin.conf.example")
        .unwrap()
        .clone();
    cascade.role = "cascade.conf.example".into();
    cascade.source = "scripts/safety/cascade.conf.example".into();
    payload.configuration.examples.push(cascade.role.clone());
    payload.contents.push(cascade);
    let mut checksums = Vec::new();
    for member in &mut payload.contents {
        let bytes = if member.kind == ManifestKind::Bin {
            member.mode = "0755".into();
            elf.clone()
        } else if member.role == "cascade.conf.example" {
            selector_real_signature::TEMPLATE.as_bytes().to_vec()
        } else {
            member.role.as_bytes().to_vec()
        };
        let path = crate::customer::package_member::bundle_destination(member)
            .unwrap()
            .unwrap();
        let target = bundle.join(&path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, &bytes).unwrap();
        member.sha256 = Some(digest(&bytes));
        checksums.push(format!("{}  ./{path}", digest(&bytes)));
    }
    payload.build.cli_sha256 = digest(&elf);
    payload.build.daemon_sha256 = digest(&elf);
    for (role, value) in [
        ("RELEASE_VERSION", "v1".to_string()),
        ("SOURCE_COMMIT", payload.source.commit.clone()),
        ("SOURCE_BRANCH", payload.source.branch.clone()),
        ("SOURCE_TREE_STATE", "clean".to_string()),
    ] {
        let bytes = format!("{value}\n");
        std::fs::write(bundle.join(role), &bytes).unwrap();
        checksums.push(format!("{}  ./{role}", digest(bytes.as_bytes())));
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
    std::fs::write(bundle.join("release-manifest.json"), &document).unwrap();
    checksums.push(format!("{}  ./release-manifest.json", digest(&document)));
    std::fs::write(
        bundle.join("SHA256SUMS"),
        format!("{}\n", checksums.join("\n")),
    )
    .unwrap();
}

#[path = "tests/openpgp.rs"]
mod openpgp;
mod selector_real_signature;

#[test]
#[ignore = "requires gpg, gpgconf and gpgv on the test host"]
fn trust_snapshots_verify_real_openpgp() {
    openpgp::run();
}

#[test]
#[ignore = "requires gpg, gpgconf, gpgv and a real expiration interval"]
fn trust_revalidation_refuses_key_expiring_during_transaction() {
    openpgp::expiry_during_revalidation();
}
