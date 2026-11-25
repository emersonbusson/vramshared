// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_trust/tests/openpgp.rs
//! Real OpenPGP custody and key-state fixture.

use super::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run_command(command: &mut Command) -> bool {
    command.stdin(Stdio::null());
    match ramshared_exec::run_capture_command(
        command,
        "trust-fixture",
        Duration::from_secs(15),
        4096,
        |_| {},
    ) {
        Ok(output) => output.status.success(),
        Err(ramshared_exec::ProcessSpawnError::NonZeroExit { .. }) => false,
        Err(error) => panic!("fixture subprocess failed: {error}"),
    }
}

struct Agent(PathBuf);
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = ramshared_exec::run_capture_command(
            Command::new("gpgconf")
                .arg("--homedir")
                .arg(&self.0)
                .args(["--kill", "gpg-agent"]),
            "trust-fixture-agent-cleanup",
            Duration::from_secs(5),
            4096,
            |_| {},
        );
    }
}

pub(super) fn expiry_during_revalidation() {
    let fixture = Fixture::new();
    let home = fixture.0.join("short-lived-gnupg");
    let bundle = fixture.0.join("short-lived-bundle");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(&bundle).unwrap();
    let _agent = Agent(home.clone());
    let manifest = bundle.join("SHA256SUMS");
    let signature = bundle.join("SHA256SUMS.asc");
    let keyring = fixture.0.join("short-lived-keyring");
    let policy = fixture.0.join("short-lived-policy");
    std::fs::write(&manifest, b"test manifest\n").unwrap();
    assert!(run_command(
        Command::new("gpg").arg("--homedir").arg(&home).args([
            "--batch",
            "--pinentry-mode",
            "loopback",
            "--passphrase",
            "",
            "--quick-generate-key",
            "RamShared short-lived fixture",
            "ed25519",
            "sign",
            "seconds=20",
        ])
    ));
    let after_generation = Instant::now();
    assert!(run_command(
        Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .args(["--batch", "--output"])
            .arg(&keyring)
            .arg("--export")
    ));
    assert!(run_command(
        Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .args([
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--output"
            ])
            .arg(&signature)
            .arg("--detach-sign")
            .arg(&manifest)
    ));
    std::fs::write(&policy, format!("release_keyring={}\n", keyring.display())).unwrap();
    let authority = verify_bundle_from_policy(&bundle, &policy, fixture.owner()).unwrap();
    assert!(authority.revalidate().is_some());
    let alternate_policy = fixture.0.join("alternate-policy");
    std::fs::write(
        &alternate_policy,
        format!(
            "# alternate authority\nenabled=1\nrelease_keyring={}\n",
            keyring.display()
        ),
    )
    .unwrap();
    let alternate = verify_bundle_from_policy(&bundle, &alternate_policy, fixture.owner()).unwrap();
    assert_eq!(authority.manifest_sha256(), alternate.manifest_sha256());
    assert_ne!(authority.trust_reference(), alternate.trust_reference());
    std::thread::sleep(Duration::from_secs(22).saturating_sub(after_generation.elapsed()));
    assert!(authority.revalidate().is_none());
}

pub(super) fn run() {
    let fixture = Fixture::new();
    let home = fixture.0.join("gnupg");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _agent = Agent(home.clone());
    let bundle = fixture.0.join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    let manifest = bundle.join("SHA256SUMS");
    let signature = bundle.join("SHA256SUMS.asc");
    let keyring = fixture.0.join("keyring");
    make_declared_bundle(&bundle);
    assert!(run_command(
        Command::new("gpg").arg("--homedir").arg(&home).args([
            "--batch",
            "--pinentry-mode",
            "loopback",
            "--passphrase",
            "",
            "--quick-generate-key",
            "RamShared disposable test key",
            "ed25519",
            "sign",
            "0",
        ])
    ));
    assert!(run_command(
        Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .args(["--batch", "--output"])
            .arg(&keyring)
            .arg("--export")
    ));
    assert!(run_command(
        Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .args([
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--output",
            ])
            .arg(&signature)
            .arg("--detach-sign")
            .arg(&manifest)
    ));
    let snapshot = |path: &Path| {
        TrustedFile::open_for_owner(path, MAX_TRUST_SNAPSHOT_BYTES, fixture.owner())
            .unwrap()
            .snapshot()
            .unwrap()
    };
    let manifest_snapshot = snapshot(&manifest);
    let signature_snapshot = snapshot(&signature);
    let keyring_snapshot = snapshot(&keyring);
    let mut verifier =
        TrustedFile::open(Path::new("/usr/bin/gpgv"), MAX_TRUST_SNAPSHOT_BYTES).unwrap();
    let verifier_snapshot = verifier.snapshot().unwrap();
    let expired_home = fixture.0.join("expired-gnupg");
    std::fs::create_dir(&expired_home).unwrap();
    std::fs::set_permissions(&expired_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _expired_agent = Agent(expired_home.clone());
    let expired_key = fixture.0.join("expired-keyring");
    let expired_signature = fixture.0.join("expired-signature");
    assert!(run_command(
        Command::new("gpg")
            .arg("--homedir")
            .arg(&expired_home)
            .args([
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--faked-system-time",
                "1577836800",
                "--quick-generate-key",
                "RamShared expired test key",
                "ed25519",
                "sign",
                "1d",
            ])
    ));
    assert!(run_command(
        Command::new("gpg")
            .arg("--homedir")
            .arg(&expired_home)
            .args(["--batch", "--output"])
            .arg(&expired_key)
            .arg("--export")
    ));
    assert!(run_command(
        Command::new("gpg")
            .arg("--homedir")
            .arg(&expired_home)
            .args([
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--faked-system-time",
                "1577836800",
                "--output",
            ])
            .arg(&expired_signature)
            .arg("--detach-sign")
            .arg(&manifest)
    ));
    assert!(
        verify_sealed_gpgv(
            &verifier_snapshot,
            &snapshot(&expired_key),
            &snapshot(&expired_signature),
            &manifest_snapshot,
        )
        .is_none(),
        "an expired signing key must never authorize a package"
    );
    let revocation = std::fs::read_dir(home.join("openpgp-revocs.d"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let certificate = std::fs::read_to_string(revocation).unwrap().replace(
        ":-----BEGIN PGP PUBLIC KEY BLOCK-----",
        "-----BEGIN PGP PUBLIC KEY BLOCK-----",
    );
    let certificate_path = fixture.0.join("revocation-certificate");
    std::fs::write(&certificate_path, certificate).unwrap();
    assert!(run_command(
        Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .arg("--batch")
            .arg("--import")
            .arg(&certificate_path)
    ));
    let revoked_key = fixture.0.join("revoked-keyring");
    assert!(run_command(
        Command::new("gpg")
            .arg("--homedir")
            .arg(&home)
            .args(["--batch", "--output"])
            .arg(&revoked_key)
            .arg("--export")
    ));
    assert!(
        verify_sealed_gpgv(
            &verifier_snapshot,
            &snapshot(&revoked_key),
            &signature_snapshot,
            &manifest_snapshot,
        )
        .is_none(),
        "a revoked signing key must never authorize a package"
    );
    assert!(
        verify_sealed_gpgv(
            &verifier_snapshot,
            &keyring_snapshot,
            &signature_snapshot,
            &File::open(&manifest).unwrap(),
        )
        .is_none()
    );
    let verify = |key: &File, data: &File| {
        verify_sealed_gpgv(&verifier_snapshot, key, &signature_snapshot, data).is_some()
    };
    assert!(verify(&keyring_snapshot, &manifest_snapshot));
    let policy = fixture.0.join("policy");
    std::fs::write(
        &policy,
        format!("enabled=1\nrelease_keyring={}\n", keyring.display()),
    )
    .unwrap();
    let authority = verify_bundle_from_policy(&bundle, &policy, fixture.owner()).unwrap();
    assert!(authority.revalidate().is_some());
    let previous_policy = fixture.0.join("previous-policy");
    std::fs::write(&previous_policy, std::fs::read(&policy).unwrap()).unwrap();
    let previous_authority =
        verify_bundle_from_policy(&bundle, &previous_policy, fixture.owner()).unwrap();
    let SignatureCustody::Held(inputs) = &authority.custody else {
        panic!("real signature must retain snapshots")
    };
    {
        let mut inputs = inputs.lock().unwrap();
        inputs.verification.keyring = snapshot(&expired_key);
        inputs.verification.signature = snapshot(&expired_signature);
    }
    assert!(authority.revalidate().is_none());
    {
        let mut inputs = inputs.lock().unwrap();
        inputs.verification.keyring = snapshot(&revoked_key);
        inputs.verification.signature = signature_snapshot.try_clone().unwrap();
    }
    assert!(authority.revalidate().is_none());
    {
        let mut inputs = inputs.lock().unwrap();
        inputs.verification.keyring = keyring_snapshot.try_clone().unwrap();
        inputs.verification.signature = signature_snapshot.try_clone().unwrap();
    }
    assert!(authority.revalidate().is_some());
    std::fs::write(&policy, b"release_keyring=/changed\n").unwrap();
    assert!(authority.revalidate().is_none());
    std::fs::write(
        &policy,
        format!("enabled=1\nrelease_keyring={}\n", keyring.display()),
    )
    .unwrap();
    let original_key = std::fs::read(&keyring).unwrap();
    std::fs::write(&keyring, b"revoked").unwrap();
    assert!(authority.revalidate().is_none());
    assert!(
        crate::customer::package_source::ApprovedBundle::open_verified(&bundle, &authority)
            .is_none()
    );
    std::fs::write(&keyring, &original_key).unwrap();
    assert!(authority.revalidate().is_some());
    assert!(
        verify_bundle_from_policy_with(&bundle, &policy, fixture.owner(), || {
            std::fs::write(&keyring, b"revoked").unwrap();
        })
        .is_none()
    );
    std::fs::write(&keyring, &original_key).unwrap();
    assert!(
        verify_bundle_from_policy_with(&bundle, &policy, fixture.owner(), || {
            std::fs::write(&policy, b"release_keyring=/changed\n").unwrap();
        })
        .is_none()
    );
    std::fs::write(
        &policy,
        format!("enabled=1\nrelease_keyring={}\n", keyring.display()),
    )
    .unwrap();
    assert!(
        verify_bundle_from_policy(&bundle, &bundle.join("SHA256SUMS"), fixture.owner()).is_none()
    );
    assert!(
        verify_bundle_for_owner(&bundle, &fixture.0.join("missing"), fixture.owner()).is_none()
    );
    let source =
        crate::customer::package_source::ApprovedBundle::open_verified(&bundle, &authority)
            .unwrap();
    let declared =
        crate::customer::package_source::DeclaredBundle::open_verified(&bundle, &authority)
            .unwrap();
    assert!(declared.stage_roles().is_some());
    if cfg!(target_arch = "x86_64") {
        assert!(declared.verify_all(1024 * 1024).is_some());
        exercise_signed_preparation(
            &fixture.0,
            &declared,
            &authority,
            &policy,
            &previous_authority,
            &previous_policy,
        );
    }
    assert_eq!(
        declared.member("origin.conf.example", 64).unwrap(),
        b"origin.conf.example"
    );
    let member_path = bundle.join("scripts/safety/origin.conf.example");
    std::fs::write(&member_path, b"tampered").unwrap();
    assert!(declared.member("origin.conf.example", 64).is_none());
    if cfg!(target_arch = "x86_64") {
        assert!(declared.verify_all(1024 * 1024).is_none());
    }
    assert!(
        source
            .member("scripts/safety/origin.conf.example", 64)
            .is_none()
    );
    assert!(
        verify_bundle_for_owner(&bundle, &bundle.join("SHA256SUMS"), fixture.owner()).is_none()
    );
    std::fs::write(&manifest, b"changed content\n").unwrap();
    assert!(verify_bundle_for_owner(&bundle, &keyring, fixture.owner()).is_none());
    assert!(verify(&keyring_snapshot, &manifest_snapshot));
    assert!(
        crate::customer::package_source::ApprovedBundle::open_verified(&bundle, &authority)
            .is_none()
    );
    let changed_snapshot = snapshot(&manifest);
    assert!(!verify(&keyring_snapshot, &changed_snapshot));
    assert!(!verify(&changed_snapshot, &manifest_snapshot));
}
