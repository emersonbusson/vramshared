// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/sealed_identity.rs
//! Sealed selected-distro identity (AUD-40).
//!
//! One identity determines every runtime health/status/origin location. The
//! source is the host-sealed `guardian-config.json`, never an environment
//! override: enumeration is not authorization, and `RAMSHARED_WSL_DISTRO` is
//! not authority for a privileged effect.
//!
//! The identity is the pair `(distro, user_sid)`. A health proof is accepted
//! only when it belongs to exactly that pair, so two users or two distros on
//! the same host cannot be confused, a renamed distro cannot keep reading the
//! previous identity's proof, and malicious identity text is refused before it
//! can become a path or a `wsl` argument.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Host-sealed identity source. Fixed path: the config is what names the
/// distro, so it cannot itself be named after the distro.
pub(crate) const GUARDIAN_CONFIG_FILE: &str = "/mnt/c/ProgramData/RamShared/guardian-config.json";
/// Host directory holding the per-distro health proofs.
pub(crate) const GUARDIAN_STATE_DIR: &str = "/mnt/c/ProgramData/RamShared/guardian-state";
/// The product's registered guardian scheduled task.
///
/// This is the sealed **contract** name the document must carry — the one home
/// for the registration constant. Consumers never retype it: they start the
/// task [`SealedIdentity::task_name`] names, which the schema check has already
/// pinned to this value. A start site that hardcodes it would be a second home.
pub(crate) const GUARDIAN_TASK_NAME: &str = "RamSharedWslGuardian.v1";

/// Longest accepted distro label. Real labels are short (`Ubuntu-24.04`); the
/// bound keeps a hostile config from becoming an unbounded path or argument.
pub(crate) const MAX_DISTRO_BYTES: usize = 64;
/// Longest accepted scheduled-task name. The registered name is
/// `RamSharedWslGuardian.v1`; the bound is the same hostile-input guard.
pub(crate) const MAX_TASK_NAME_BYTES: usize = 64;

/// The sealed `(distro, user_sid)` pair a runtime location is bound to, and the
/// scheduled task name the same document authorizes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SealedIdentity {
    pub(crate) distro: String,
    pub(crate) user_sid: String,
    /// Task name read from the document after the schema check pinned it to
    /// [`GUARDIAN_TASK_NAME`]. Host starts take this value, never a literal.
    pub(crate) task_name: String,
}

impl SealedIdentity {
    /// Health proof path for exactly this identity. The distro label has
    /// already been validated, so it is a single path segment.
    pub(crate) fn health_path(&self, state_dir: &Path) -> PathBuf {
        state_dir.join(format!("{}.health.json", self.distro))
    }
}

/// Why no identity could be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SealedIdentityError {
    /// The sealed config is missing or unreadable.
    Unavailable,
    /// The bytes are not a JSON object of the expected shape.
    Malformed,
    /// Schema version or task name does not match the sealed contract.
    SchemaMismatch,
    /// The distro label is empty, oversized, path-shaped or otherwise hostile.
    InvalidDistro,
    /// The Windows user SID is not a well-formed SID string.
    InvalidUserSid,
}

impl SealedIdentityError {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SealedIdentityError::Unavailable => "guardian_identity_unavailable",
            SealedIdentityError::Malformed => "guardian_identity_malformed",
            SealedIdentityError::SchemaMismatch => "guardian_identity_schema_mismatch",
            SealedIdentityError::InvalidDistro => "guardian_identity_invalid_distro",
            SealedIdentityError::InvalidUserSid => "guardian_identity_invalid_user_sid",
        }
    }
}

/// Read and validate the sealed identity from `guardian-config.json`.
///
/// The document is the file itself, not a redirect to one — the rule
/// [`crate::release_identity`] already states for sealed identity documents,
/// and the one `ramshared-host-gate.sh` applies to the paths it checks
/// (`[[ -f $x && ! -L $x ]]`). This path is on the Windows side of `/mnt/c`,
/// where DrvFs reports the mount owner rather than the Windows ACL, so the
/// owner-matches-parent invariant that guards a `/run/ramshared/` document
/// does not transfer here; refusing a redirect does, and is what the gate
/// already believes.
///
/// `Unavailable` covers a missing document, a redirect, and a document that is
/// not a regular file: this reader never distinguishes why it could not show
/// you the sealed identity, because a caller that acts on the identity must
/// fail closed on all three.
pub(crate) fn read_sealed_identity(path: &Path) -> Result<SealedIdentity, SealedIdentityError> {
    let text =
        crate::release_identity::read_regular_text(path).ok_or(SealedIdentityError::Unavailable)?;
    parse_sealed_identity(&text)
}

/// Validate sealed identity bytes.
///
/// The contract is the one `Watch-RamSharedWsl.ps1` writes and
/// `ramshared-host-gate.sh` already checks: `schema_version` 2, a `distro`, a
/// `user_sid`, `task_name` `RamSharedWslGuardian.v1` and a `guardian_policy`
/// object. Only the identity pair is returned; the policy is the publisher's
/// and is checked by the gate that acts on it.
pub(crate) fn parse_sealed_identity(text: &str) -> Result<SealedIdentity, SealedIdentityError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let value: Value = serde_json::from_str(text).map_err(|_| SealedIdentityError::Malformed)?;
    let object = value.as_object().ok_or(SealedIdentityError::Malformed)?;

    if object.get("schema_version").and_then(Value::as_i64) != Some(2) {
        return Err(SealedIdentityError::SchemaMismatch);
    }
    let task_name = object
        .get("task_name")
        .and_then(Value::as_str)
        .ok_or(SealedIdentityError::SchemaMismatch)?;
    if task_name != GUARDIAN_TASK_NAME {
        return Err(SealedIdentityError::SchemaMismatch);
    }
    if !object.get("guardian_policy").is_some_and(Value::is_object) {
        return Err(SealedIdentityError::SchemaMismatch);
    }

    let distro = object
        .get("distro")
        .and_then(Value::as_str)
        .ok_or(SealedIdentityError::InvalidDistro)?;
    if !is_safe_distro_label(distro) {
        return Err(SealedIdentityError::InvalidDistro);
    }

    let user_sid = object
        .get("user_sid")
        .and_then(Value::as_str)
        .ok_or(SealedIdentityError::InvalidUserSid)?;
    if !is_safe_user_sid(user_sid) {
        return Err(SealedIdentityError::InvalidUserSid);
    }

    Ok(SealedIdentity {
        distro: distro.to_string(),
        user_sid: user_sid.to_string(),
        task_name: task_name.to_string(),
    })
}

/// One path-segment-safe label — a distro name or a scheduled-task name.
///
/// Accepted charset is the one the host gate already seals on
/// (`[A-Za-z0-9._-]`), plus a length bound and explicit refusal of the
/// traversal labels. No path separator, no control character, no leading dash
/// (so it cannot be read as a flag).
fn is_safe_label(label: &str, max_bytes: usize) -> bool {
    if label.is_empty() || label.len() > max_bytes {
        return false;
    }
    if label == "." || label == ".." || label.starts_with('-') {
        return false;
    }
    // No legitimate label contains a traversal token. Refusing the `..` run is
    // belt-and-braces on top of the charset: a label that looks like a
    // traversal is never a selected distro or a registered task.
    if label.contains("..") {
        return false;
    }
    label
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A distro label is a single path segment and a safe `wsl` argument.
pub(crate) fn is_safe_distro_label(label: &str) -> bool {
    is_safe_label(label, MAX_DISTRO_BYTES)
}

/// A scheduled-task name is one identifier and a safe host argument.
pub(crate) fn is_safe_task_name(label: &str) -> bool {
    is_safe_label(label, MAX_TASK_NAME_BYTES)
}

/// Windows user SID shape, matching the host gate's `S-1-[0-9-]+`.
pub(crate) fn is_safe_user_sid(sid: &str) -> bool {
    let Some(rest) = sid.strip_prefix("S-1-") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-')
}

/// Does this health proof belong to exactly this sealed identity?
///
/// Both halves must match. A proof that names another user or another distro
/// is not a weaker proof of this one — it is someone else's proof (AUD-40).
pub(crate) fn health_belongs_to(value: &Value, identity: &SealedIdentity) -> bool {
    value.get("distro").and_then(Value::as_str) == Some(identity.distro.as_str())
        && value.get("user_sid").and_then(Value::as_str) == Some(identity.user_sid.as_str())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn config_json(distro: &str, user_sid: &str) -> String {
        // The fixture carries the registered name from the one contract
        // constant, so a rename cannot leave the fixtures still green.
        format!(
            r#"{{"schema_version":2,"distro":"{distro}","user_sid":"{user_sid}","task_name":"{task}","guardian_policy":{{}}}}"#,
            task = GUARDIAN_TASK_NAME,
        )
    }

    fn identity(distro: &str, user_sid: &str) -> SealedIdentity {
        SealedIdentity {
            distro: distro.to_string(),
            user_sid: user_sid.to_string(),
            task_name: GUARDIAN_TASK_NAME.to_string(),
        }
    }

    /// AUD-40. A non-default distro is a first-class identity. The default
    /// name is not privileged; the sealed config is.
    #[test]
    fn sealed_identity_accepts_non_default_distro() {
        let parsed =
            parse_sealed_identity(&config_json("Debian", "S-1-5-21-1-2-3-1001")).expect("debian");
        assert_eq!(parsed.distro, "Debian");
        assert_eq!(parsed.user_sid, "S-1-5-21-1-2-3-1001");

        let parsed =
            parse_sealed_identity(&config_json("Ubuntu-22.04", "S-1-5-21-1-2-3-1001")).expect("22");
        assert_eq!(parsed.distro, "Ubuntu-22.04");

        // The historical default still parses; it is just not special.
        let parsed =
            parse_sealed_identity(&config_json("Ubuntu-24.04", "S-1-5-21-1-2-3-1001")).expect("24");
        assert_eq!(parsed.distro, "Ubuntu-24.04");
    }

    /// AUD-40. Two users or two distros on one host are two identities. Each
    /// health path is distinct and neither proof is accepted for the other.
    #[test]
    fn sealed_identity_separates_two_users_and_distros() {
        let alice_ubuntu = identity("Ubuntu-24.04", "S-1-5-21-1-2-3-1001");
        let bob_ubuntu = identity("Ubuntu-24.04", "S-1-5-21-1-2-3-1002");
        let alice_debian = identity("Debian", "S-1-5-21-1-2-3-1001");

        let dir = Path::new("/mnt/c/ProgramData/RamShared/guardian-state");
        // The location is per-distro by design. Two users of the same distro
        // share the slot; the user half separates the *proof*, checked by
        // `health_belongs_to` below. Two distros do not share a slot.
        assert_eq!(alice_ubuntu.health_path(dir), bob_ubuntu.health_path(dir));
        assert_ne!(alice_ubuntu.health_path(dir), alice_debian.health_path(dir));

        let alice_proof: Value = serde_json::from_str(&format!(
            r#"{{"distro":"{}","user_sid":"{}","state":"HEALTHY"}}"#,
            alice_ubuntu.distro, alice_ubuntu.user_sid
        ))
        .unwrap();
        assert!(health_belongs_to(&alice_proof, &alice_ubuntu));
        assert!(
            !health_belongs_to(&alice_proof, &bob_ubuntu),
            "a proof for one Windows user is not a proof for another"
        );
        assert!(
            !health_belongs_to(&alice_proof, &alice_debian),
            "a proof for one distro is not a proof for another"
        );
    }

    /// AUD-40. A renamed or never-registered distro does not inherit the old
    /// identity. The label must be well-formed, and a proof naming a different
    /// label is refused rather than coerced.
    #[test]
    fn sealed_identity_refuses_renamed_or_unregistered_distro() {
        // A well-formed but unregistered label is still a distinct identity:
        // it does not silently resolve to the previous one.
        let renamed = parse_sealed_identity(&config_json("Ubuntu-99.99", "S-1-5-21-1-2-3-1001"))
            .expect("well-formed label is admitted as its own identity");
        assert_eq!(renamed.distro, "Ubuntu-99.99");

        let old_proof: Value =
            serde_json::from_str(r#"{"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001"}"#)
                .unwrap();
        assert!(!health_belongs_to(&old_proof, &renamed));

        // Labels that survive JSON parsing but are not safe path segments
        // are refused as invalid distro text.
        for hostile in [
            "",
            ".",
            "..",
            "-Ubuntu",
            "Ubuntu/24.04",
            "Ubuntu..24",
            "Ubuntu 24.04",
            "../etc",
            "/etc/passwd",
        ] {
            assert!(
                !is_safe_distro_label(hostile),
                "label must be refused: {hostile:?}"
            );
            assert_eq!(
                parse_sealed_identity(&config_json(hostile, "S-1-5-21-1-2-3-1001")),
                Err(SealedIdentityError::InvalidDistro),
                "config with label {hostile:?} must not mint an identity"
            );
        }

        // Control characters cannot appear in a JSON string at all, so they
        // fail as malformed JSON before the label is ever examined. Either
        // error is a refusal; neither mints an identity.
        for hostile in ["Ubuntu\t24", "Ubuntu\u{0}24", "Ubuntu\n24"] {
            assert!(!is_safe_distro_label(hostile));
            assert!(
                parse_sealed_identity(&config_json(hostile, "S-1-5-21-1-2-3-1001")).is_err(),
                "config with label {hostile:?} must not mint an identity"
            );
        }

        // Oversized labels are refused by the bound, not truncated.
        let oversized = "a".repeat(MAX_DISTRO_BYTES + 1);
        assert!(!is_safe_distro_label(&oversized));
        assert_eq!(
            parse_sealed_identity(&config_json(&oversized, "S-1-5-21-1-2-3-1001")),
            Err(SealedIdentityError::InvalidDistro)
        );
    }

    /// AUD-40. Identity text that tries to become a path, a shell word or an
    /// option is refused before it reaches either.
    #[test]
    fn sealed_identity_refuses_malicious_identity_text() {
        for (distro, sid) in [
            ("Ubuntu-24.04", ""),
            ("Ubuntu-24.04", "root"),
            ("Ubuntu-24.04", "S-1-5-21-1-2-3-1001; rm -rf /"),
            ("Ubuntu-24.04", "S-1-5-21-1-2-3-1001\u{0}"),
            ("Ubuntu-24.04", "S-1-"),
            ("Ubuntu-24.04", "S-1-x"),
            ("Ubuntu-24.04", "s-1-5-21-1-2-3-1001"),
            ("Ubuntu-24.04; reboot", "S-1-5-21-1-2-3-1001"),
            ("$(reboot)", "S-1-5-21-1-2-3-1001"),
            ("`reboot`", "S-1-5-21-1-2-3-1001"),
        ] {
            let result = parse_sealed_identity(&config_json(distro, sid));
            assert!(
                result.is_err(),
                "hostile pair ({distro:?}, {sid:?}) must not mint an identity"
            );
        }

        // JSON that is not the sealed object is malformed, not a default.
        for text in [
            "",
            "null",
            "[]",
            "\"Ubuntu-24.04\"",
            "{}",
            r#"{"schema_version":3,"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","task_name":"RamSharedWslGuardian.v1","guardian_policy":{}}"#,
            r#"{"schema_version":2,"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","task_name":"OtherTask","guardian_policy":{}}"#,
            r#"{"schema_version":2,"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","task_name":"RamSharedWslGuardian.v1"}"#,
            r#"{"schema_version":2,"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","task_name":"RamSharedWslGuardian.v1","guardian_policy":[]}"#,
        ] {
            let result = parse_sealed_identity(text);
            assert!(result.is_err(), "text {text:?} must not mint an identity");
        }
    }

    /// AUD-40. A proof that names a different pair, or that omits either half
    /// of the pair, is a mismatch. Ambiguity is refused, not guessed.
    #[test]
    fn sealed_identity_refuses_mismatched_health_proof() {
        let expected = identity("Ubuntu-24.04", "S-1-5-21-1-2-3-1001");

        let matching: Value = serde_json::from_str(
            r#"{"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY"}"#,
        )
        .unwrap();
        assert!(health_belongs_to(&matching, &expected));

        for proof in [
            r#"{"distro":"Debian","user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY"}"#,
            r#"{"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1002","state":"HEALTHY"}"#,
            r#"{"distro":"Ubuntu-24.04","state":"HEALTHY"}"#,
            r#"{"user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY"}"#,
            r#"{"distro":null,"user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY"}"#,
            r#"{"distro":"Ubuntu-24.04","user_sid":null,"state":"HEALTHY"}"#,
            r#"{"distro":"Ubuntu-24.04 ","user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY"}"#,
            "{}",
        ] {
            let value: Value = serde_json::from_str(proof).unwrap();
            assert!(
                !health_belongs_to(&value, &expected),
                "proof {proof} must not be accepted for the sealed pair"
            );
        }
    }

    /// A proof bound to a prior boot is a stale object even when every identity
    /// field matches. Boot identity is a separate axis (AUD-40: preserve boot
    /// identity) and must not be collapsed into the distro pair.
    #[test]
    fn sealed_identity_distinguishes_boot_binding_from_distro_pair() {
        let expected = identity("Ubuntu-24.04", "S-1-5-21-1-2-3-1001");
        let prior_boot: Value = serde_json::from_str(
            r#"{"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY","boot_id":"11111111-2222-3333-4444-555555555555"}"#,
        )
        .unwrap();
        let this_boot: Value = serde_json::from_str(
            r#"{"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","state":"HEALTHY","boot_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"}"#,
        )
        .unwrap();

        // Identity membership is about the pair; both proofs belong to it.
        assert!(health_belongs_to(&prior_boot, &expected));
        assert!(health_belongs_to(&this_boot, &expected));

        // Boot identity is its own check and does distinguish them.
        assert_ne!(
            prior_boot.get("boot_id"),
            this_boot.get("boot_id"),
            "a prior-boot proof must not equal a current-boot proof"
        );
    }

    #[test]
    fn sealed_identity_error_labels_are_stable() {
        assert_eq!(
            SealedIdentityError::Unavailable.as_str(),
            "guardian_identity_unavailable"
        );
        assert_eq!(
            SealedIdentityError::Malformed.as_str(),
            "guardian_identity_malformed"
        );
        assert_eq!(
            SealedIdentityError::SchemaMismatch.as_str(),
            "guardian_identity_schema_mismatch"
        );
        assert_eq!(
            SealedIdentityError::InvalidDistro.as_str(),
            "guardian_identity_invalid_distro"
        );
        assert_eq!(
            SealedIdentityError::InvalidUserSid.as_str(),
            "guardian_identity_invalid_user_sid"
        );
    }

    #[test]
    fn sealed_identity_accepts_utf8_bom_and_exact_schema() {
        let text = format!(
            "\u{feff}{}",
            config_json("Ubuntu-24.04", "S-1-5-21-1-2-3-1001")
        );
        let parsed = parse_sealed_identity(&text).expect("BOM is stripped");
        assert_eq!(parsed.distro, "Ubuntu-24.04");
    }

    /// DT-19. The task name the start sites use is the one the document names,
    /// so no caller has to retype the registration constant and a rename
    /// cannot leave a start site pointing at the old name.
    #[test]
    fn sealed_identity_returns_the_task_name_it_validated() {
        let parsed = parse_sealed_identity(&config_json("Ubuntu-24.04", "S-1-5-21-1-2-3-1001"))
            .expect("valid document");
        assert_eq!(parsed.task_name, GUARDIAN_TASK_NAME);
        assert_eq!(parsed.task_name, "RamSharedWslGuardian.v1");
    }

    /// DT-19. A document that names a different task is not a weaker identity
    /// for this one — it is a different registration, and nothing in the guest
    /// may start it.
    #[test]
    fn sealed_identity_refuses_a_wrong_task_name_before_it_is_returned() {
        for wrong in [
            "RamSharedWslGuardian.v2",
            "RamSharedWslGuardian",
            "",
            "RamSharedWslGuardian.v1 ",
            "../RamSharedWslGuardian.v1",
        ] {
            let text = format!(
                r#"{{"schema_version":2,"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","task_name":"{wrong}","guardian_policy":{{}}}}"#
            );
            let err = parse_sealed_identity(&text).expect_err("wrong task name must refuse");
            assert_eq!(err, SealedIdentityError::SchemaMismatch, "wrong={wrong:?}");
        }

        let missing = r#"{"schema_version":2,"distro":"Ubuntu-24.04","user_sid":"S-1-5-21-1-2-3-1001","guardian_policy":{}}"#;
        let err = parse_sealed_identity(missing).expect_err("absent task name must refuse");
        assert_eq!(err, SealedIdentityError::SchemaMismatch);
    }

    /// The reader refuses a redirect to a well-formed identity.
    ///
    /// The parse tests above all feed `parse_sealed_identity` a string, so they
    /// cannot see the rule that lives in the open: the sealed document is the
    /// file itself. A symlink whose target carries a valid guardian identity
    /// parses on its own and is still not the host-sealed document — the same
    /// refusal `ramshared-host-gate.sh` applies to the paths it checks.
    #[test]
    fn read_sealed_identity_refuses_a_redirect_to_a_valid_identity() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "ramshared-sealed-identity-redirect-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let target = dir.join("elsewhere.json");
        std::fs::write(&target, config_json("Ubuntu-24.04", "S-1-5-21-1-2-3-1001")).unwrap();
        assert!(
            read_sealed_identity(&target).is_ok(),
            "the target is a well-formed identity document, so the fixture is not empty"
        );

        let link = dir.join("guardian-config.json");
        symlink(&target, &link).unwrap();
        let err = read_sealed_identity(&link).expect_err("a redirect is not a sealed document");
        assert_eq!(err, SealedIdentityError::Unavailable);

        // And a directory is not a document either.
        let as_dir = dir.join("as-dir");
        std::fs::create_dir_all(&as_dir).unwrap();
        assert_eq!(
            read_sealed_identity(&as_dir).expect_err("a directory is not a document"),
            SealedIdentityError::Unavailable
        );

        // A missing path is `Unavailable`, not a parse error: this reader never
        // tells a caller *why* it could not show the sealed identity.
        assert_eq!(
            read_sealed_identity(&dir.join("absent.json")).expect_err("absent"),
            SealedIdentityError::Unavailable
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
