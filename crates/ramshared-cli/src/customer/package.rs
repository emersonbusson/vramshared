// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package.rs
//! Release-manifest validation (ITEM-3 / ITEM-7).
//!
//! [`PayloadManifest`] is the covered semantic validator of
//! `packaging/release-manifest.schema.json`. JSON Schema pins the document
//! shape; this type refuses the agreements a schema cannot express — that a
//! declared exclusion is never staged, that a format-scoped member says why,
//! that the pinned binary digests are the digests of the `bin` members, and
//! that every unit and drop-in the control plane manages is actually shipped.
//!
//! The DT-6 quiescence/compatibility admission lives beside this file in
//! [`super::package_admission`] — one surface split by responsibility, not by
//! product concept: this module owns *what the payload is*, that one owns
//! *whether the lifecycle may be touched*. Per-member well-formedness lives in
//! [`super::package_member`], so this module keeps only the document-level
//! agreements.
//!
//! Structure: [`PayloadManifest::parse`] bounds and shapes the document,
//! [`PayloadManifest::validate_semantics`] applies the ordered agreements as
//! one rule per method, and [`PayloadManifest::verify_identity`] compares a
//! parsed document to what was actually shipped. A failure names its own
//! cause; no rule silently repairs.
//!
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::package_member::{bundle_destination, check_member_agreements};
use super::{CustomerError, CustomerReason, CustomerState};

/// Maximum accepted release-manifest document size. Matches the Windows
/// product manifest bound: a manifest is a bounded declaration, not a log.
pub(crate) const MAX_PAYLOAD_MANIFEST_BYTES: usize = 64 * 1024;

/// The one document version a consumer recognises. Any other value is refused
/// rather than guessed: a schema this parser does not know may name members
/// this product would then stage without a rule.
pub(crate) const PAYLOAD_MANIFEST_SCHEMA: &str = "ramshared-release-manifest/v1";

/// The two shipped binaries whose build identity the manifest pins, as
/// `build.cli_sha256` and `build.daemon_sha256` in that order.
///
/// Product identity, the same kind of constant as the Windows
/// `REQUIRED_ARTIFACTS` set: these are the names the payload actually carries,
/// not a magic string scattered through the validator.
const PINNED_BINARIES: [&str; 2] = ["ramshared", "ramsharedd"];

/// The lifecycle unit contract that was retired when the cascade service
/// became the single activator. Its presence beside the cascade unit is a
/// second activation path, which DT-8 and the one-lifecycle-unit contract
/// forbid.
pub(crate) const RETIRED_LIFECYCLE_UNIT: &str = "ramsharedd.service";

/// Every packaging format a member may declare. The shared product set is the
/// whole list; a member declared for a strict subset is a declared packaging
/// difference and must say why.
pub(crate) const ALL_FORMATS: [ManifestFormat; 4] = [
    ManifestFormat::Deb,
    ManifestFormat::Bundle,
    ManifestFormat::Rpm,
    ManifestFormat::Arch,
];

/// Supported payload architecture. The label is what the manifest declares;
/// the shipped ELF `e_machine` must agree before anything is executed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManifestArchitecture {
    Amd64,
    Arm64,
}

impl ManifestArchitecture {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }
}

/// Package format a member ships in. A member's `formats` set is the
/// declaration; a builder that stages outside it is the contract breach.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManifestFormat {
    Deb,
    Bundle,
    Rpm,
    Arch,
}

impl ManifestFormat {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Deb => "deb",
            Self::Bundle => "bundle",
            Self::Rpm => "rpm",
            Self::Arch => "arch",
        }
    }
}

/// Responsibility class of one payload member. Drives each builder's
/// destination mapping; it never names an install path, because those differ
/// per format and DT-8 forbids letting that difference change behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ManifestKind {
    Bin,
    Script,
    Library,
    Config,
    ConfigExample,
    Unit,
    UnitTemplate,
    Dropin,
    Rule,
    Tmpfiles,
    Sysusers,
    Doc,
    Generated,
}

impl ManifestKind {
    pub(crate) fn is_generated(self) -> bool {
        matches!(self, Self::Generated)
    }
}

/// How the built tree stood against its commit at build time.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManifestTreeState {
    Clean,
    Dirty,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestProduct {
    pub(crate) name: String,
    pub(crate) version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestSource {
    pub(crate) commit: String,
    pub(crate) branch: String,
    pub(crate) tree_state: ManifestTreeState,
}

/// The build identity of the two pinned binaries.
///
/// AUD-42: a digest is of the bytes actually shipped. A manifest that records
/// a digest its own `bin` members do not carry is describing a release that
/// does not exist.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestBuild {
    pub(crate) cli_sha256: String,
    pub(crate) daemon_sha256: String,
    pub(crate) source_date_epoch: Option<u64>,
}

/// One member of the runtime payload.
///
/// Identified by `role`, never by a packaging-format install path: the DEB
/// puts a script under `/usr/share/ramshared/scripts` and the sealed bundle
/// puts the same role under `scripts/safety`, and that difference must not
/// become a runtime behavior difference.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestMember {
    pub(crate) role: String,
    pub(crate) kind: ManifestKind,
    pub(crate) mode: String,
    pub(crate) source: String,
    pub(crate) formats: Vec<ManifestFormat>,
    /// Lowercase hex digest of the shipped bytes. `None` only for
    /// `kind=generated`, whose bytes do not exist at declaration time.
    pub(crate) sha256: Option<String>,
    /// Why a member is format-scoped. Required whenever `formats` is not the
    /// full shared set: an undeclared difference is the defect this manifest
    /// exists to prevent.
    pub(crate) reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestDependencies {
    pub(crate) runtime: Vec<String>,
}

/// Unit declarations.
///
/// `lifecycle` units own tier activation; `control_plane` units and `dropins`
/// are the set `manage-control-plane.sh` stages transactionally. A package
/// that ships a control-plane unit without its drop-ins installs half a
/// control plane (customer-installation DT-8).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestUnits {
    pub(crate) lifecycle: Vec<String>,
    pub(crate) control_plane: Vec<String>,
    pub(crate) dropins: Vec<String>,
    /// Named absences. A builder that stages one is the regression.
    pub(crate) excluded: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestConfiguration {
    pub(crate) examples: Vec<String>,
    /// Package-manager conffiles. Empty by default: the sealed origin config
    /// is operator state and must never be a conffile, or a purge could delete
    /// origin data (AUD-41).
    pub(crate) conffiles: Vec<String>,
    /// Paths the product creates at runtime as operator state. Never staged.
    pub(crate) operator_state: Vec<String>,
}

/// The one semantic form of `packaging/release-manifest.schema.json`.
///
/// Unknown fields are refused, not ignored: an ignored field is a second
/// meaning smuggled into a declaration the builders will stage from.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadManifest {
    pub(crate) schema_version: String,
    pub(crate) product: ManifestProduct,
    pub(crate) architecture: ManifestArchitecture,
    pub(crate) source: ManifestSource,
    pub(crate) build: ManifestBuild,
    pub(crate) contents: Vec<ManifestMember>,
    pub(crate) dependencies: ManifestDependencies,
    pub(crate) units: ManifestUnits,
    pub(crate) configuration: ManifestConfiguration,
}

/// What a verifier measured from the bytes a manifest claims to describe.
///
/// Injected, never read from global state inside the validator, so a test
/// supplies a known payload and asserts the agreements. Every field is an
/// observation of shipped bytes or of the target, never a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PayloadIdentity {
    pub(crate) architecture: ManifestArchitecture,
    pub(crate) product_version: String,
    pub(crate) cli_sha256: String,
    pub(crate) daemon_sha256: String,
}

/// Refusal constructors for this surface. The `state` field follows the
/// convention `customer_refusal_exits_preserve_installed_off_state` pins in
/// the parent module: every reason preserves [`CustomerState::InstalledOff`]
/// except the two named non-Off outcomes. `pub(crate)` because
/// [`super::package_admission`] refuses through the same shapes.
pub(crate) fn identity_changed(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::IdentityChanged,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

pub(crate) fn unsupported(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::UnsupportedSurface,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

pub(crate) fn observation_unavailable(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::ObservationUnavailable,
        detail: detail.into(),
        state: CustomerState::UnknownBlocked,
    }
}

pub(crate) fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_full_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_product_version(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_branch(value: &str) -> bool {
    (1..=200).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

pub(crate) fn is_role(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && value.len() <= 200
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        && value
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

pub(crate) fn is_installed_mode(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 4
        && bytes[0] == b'0'
        && bytes[1..].iter().all(|byte| (b'0'..=b'7').contains(byte))
}

fn is_dropin_role(value: &str) -> bool {
    let Some((unit, leaf)) = value.split_once(".service.d/") else {
        return false;
    };
    !unit.is_empty()
        && unit.len() <= 64
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !leaf.is_empty()
        && leaf.len() <= 64
        && leaf
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !leaf.contains('/')
}

/// Lowercase hex digest of exact bytes.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl PayloadManifest {
    /// Bounded, strict parse of one release-manifest document.
    ///
    /// Refuses rather than guesses: oversized input, unknown fields, an
    /// unrecognised schema version and every semantic disagreement below are
    /// rejections, not repairs.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, CustomerError> {
        if bytes.len() > MAX_PAYLOAD_MANIFEST_BYTES {
            return Err(unsupported(format!(
                "release manifest is {} bytes; the bound is {MAX_PAYLOAD_MANIFEST_BYTES}",
                bytes.len()
            )));
        }
        let manifest: Self = serde_json::from_slice(bytes).map_err(|error| {
            unsupported(format!(
                "release manifest is not a strict document: {error}"
            ))
        })?;
        manifest.validate_semantics()?;
        Ok(manifest)
    }

    /// Ordered semantic agreements. Each rule names its own cause; a failure
    /// stops there rather than accumulating a repaired document.
    ///
    /// The order is the contract: identity is proven before contents are
    /// trusted, contents before units, and units before configuration. A
    /// document is never partially repaired.
    fn validate_semantics(&self) -> Result<(), CustomerError> {
        self.rule_schema_version()?;
        self.rule_product_identity()?;
        self.rule_source_identity()?;
        self.rule_build_identity()?;
        let roles = self.rule_members()?;
        self.rule_exclusions_never_staged(&roles)?;
        self.rule_one_lifecycle_unit()?;
        self.rule_control_plane_is_whole(&roles)?;
        self.rule_pinned_binaries()?;
        self.rule_configuration_agrees(&roles)?;
        Ok(())
    }

    /// Rule 1: recognised document version.
    fn rule_schema_version(&self) -> Result<(), CustomerError> {
        if self.schema_version != PAYLOAD_MANIFEST_SCHEMA {
            return Err(unsupported(format!(
                "release manifest schema_version is {}; this consumer recognises only {PAYLOAD_MANIFEST_SCHEMA}",
                self.schema_version
            )));
        }
        Ok(())
    }

    /// Rule 2: product identity.
    fn rule_product_identity(&self) -> Result<(), CustomerError> {
        if self.product.name != "ramshared" {
            return Err(unsupported(format!(
                "release manifest product name is {}; only ramshared is declared",
                self.product.name
            )));
        }
        if !is_product_version(&self.product.version) {
            return Err(unsupported(format!(
                "release manifest product version {:?} is outside the declared alphabet",
                self.product.version
            )));
        }
        Ok(())
    }

    /// Rule 3: source identity. A short or absent commit is not an identity.
    fn rule_source_identity(&self) -> Result<(), CustomerError> {
        if !is_full_commit(&self.source.commit) {
            return Err(unsupported(format!(
                "release manifest source commit {:?} is not a full 40-hex identity",
                self.source.commit
            )));
        }
        if !is_branch(&self.source.branch) {
            return Err(unsupported(format!(
                "release manifest source branch {:?} is outside the declared alphabet",
                self.source.branch
            )));
        }
        Ok(())
    }

    /// Rule 4: build identity of the pinned binaries.
    fn rule_build_identity(&self) -> Result<(), CustomerError> {
        for (label, digest) in [
            ("cli_sha256", &self.build.cli_sha256),
            ("daemon_sha256", &self.build.daemon_sha256),
        ] {
            if !is_lowercase_sha256(digest) {
                return Err(unsupported(format!(
                    "release manifest build.{label} {:?} is not a lowercase sha256",
                    digest
                )));
            }
        }
        Ok(())
    }

    /// Rule 5: members — unique roles, declared modes, sources, formats and
    /// digests, and a reason exactly when the member is format-scoped.
    ///
    /// Returns the role set so the later rules can ask "is this shipped?"
    /// without re-walking the member list.
    fn rule_members(&self) -> Result<BTreeSet<&str>, CustomerError> {
        if self.contents.is_empty() {
            return Err(unsupported(
                "release manifest declares no payload members; a payload is not empty",
            ));
        }
        let mut roles = BTreeSet::new();
        let mut bundle_paths: BTreeSet<String> = BTreeSet::new();
        for member in &self.contents {
            check_member_agreements(member, &mut roles)?;
            if let Some(path) = bundle_destination(member)? {
                let conflicts = bundle_paths.iter().any(|prior| {
                    prior == &path
                        || prior
                            .strip_prefix(path.as_str())
                            .is_some_and(|suffix| suffix.starts_with('/'))
                        || path
                            .strip_prefix(prior.as_str())
                            .is_some_and(|suffix| suffix.starts_with('/'))
                });
                if conflicts {
                    return Err(unsupported(format!(
                        "release manifest bundle destination {path} conflicts with another member"
                    )));
                }
                bundle_paths.insert(path);
            }
        }
        Ok(roles)
    }

    /// Rule 6: declared exclusions are never staged. Naming an absence is
    /// deliberate; staging one is the regression.
    fn rule_exclusions_never_staged(&self, roles: &BTreeSet<&str>) -> Result<(), CustomerError> {
        if let Some(excluded) = &self.units.excluded {
            for name in excluded {
                if roles.contains(name.as_str()) {
                    return Err(unsupported(format!(
                        "release manifest stages {name}, which it declares excluded from the runtime payload"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Rule 7: exactly one lifecycle unit contract. The retired pre-cascade
    /// unit beside the cascade unit is a second activation path.
    fn rule_one_lifecycle_unit(&self) -> Result<(), CustomerError> {
        if self
            .units
            .lifecycle
            .iter()
            .any(|unit| unit == RETIRED_LIFECYCLE_UNIT)
        {
            return Err(unsupported(format!(
                "release manifest lifecycle set carries retired unit {RETIRED_LIFECYCLE_UNIT}; the cascade service is the one activator"
            )));
        }
        Ok(())
    }

    /// Rule 8: every unit and drop-in the control plane manages is shipped,
    /// and every declared drop-in is well formed. A control-plane unit without
    /// its drop-ins is half a control plane. A name declared twice is a
    /// malformed set, not a stronger declaration.
    fn rule_control_plane_is_whole(&self, roles: &BTreeSet<&str>) -> Result<(), CustomerError> {
        if self.units.control_plane.is_empty() {
            return Err(unsupported(
                "release manifest declares no control-plane units; manage-control-plane.sh manages a set",
            ));
        }
        for (label, names) in [
            ("lifecycle", &self.units.lifecycle),
            ("control_plane", &self.units.control_plane),
            ("dropins", &self.units.dropins),
        ] {
            let mut seen = BTreeSet::new();
            for name in names.iter() {
                if !seen.insert(name.as_str()) {
                    return Err(unsupported(format!(
                        "release manifest declares {label} name {name} twice"
                    )));
                }
            }
        }
        for unit in self
            .units
            .lifecycle
            .iter()
            .chain(self.units.control_plane.iter())
        {
            if !is_role(unit) {
                return Err(unsupported(format!(
                    "release manifest declares unit {unit:?} outside the declared alphabet"
                )));
            }
            if !roles.contains(unit.as_str()) {
                return Err(unsupported(format!(
                    "release manifest declares unit {unit} but ships no member for it"
                )));
            }
        }
        for dropin in &self.units.dropins {
            if !is_dropin_role(dropin) {
                return Err(unsupported(format!(
                    "release manifest declares drop-in {dropin:?} outside the declared shape"
                )));
            }
            if !roles.contains(dropin.as_str()) {
                return Err(unsupported(format!(
                    "release manifest declares drop-in {dropin} but ships no member for it"
                )));
            }
        }
        Ok(())
    }

    /// Rule 9: the pinned binary digests are the digests of the `bin` members.
    /// A manifest that disagrees with its own payload describes a release that
    /// does not exist.
    fn rule_pinned_binaries(&self) -> Result<(), CustomerError> {
        let mut pinned = Vec::new();
        for member in &self.contents {
            if member.kind == ManifestKind::Bin {
                pinned.push(member);
            }
        }
        if pinned.len() != PINNED_BINARIES.len() {
            return Err(unsupported(format!(
                "release manifest ships {} bin members; exactly {} are pinned ({:?})",
                pinned.len(),
                PINNED_BINARIES.len(),
                PINNED_BINARIES
            )));
        }
        for (role, expected) in PINNED_BINARIES.iter().zip([
            self.build.cli_sha256.as_str(),
            self.build.daemon_sha256.as_str(),
        ]) {
            let Some(member) = pinned.iter().find(|member| member.role == *role) else {
                return Err(unsupported(format!(
                    "release manifest pins binary {role} but ships no bin member for it"
                )));
            };
            if member.sha256.as_deref() != Some(expected) {
                return Err(unsupported(format!(
                    "release manifest binary {role} digest {:?} does not match the pinned build identity",
                    member.sha256
                )));
            }
        }
        Ok(())
    }

    /// Rule 10: configuration agrees with what is shipped. A conffile is
    /// refused by default: the sealed origin config is operator state and a
    /// purge must not delete origin data (AUD-41).
    fn rule_configuration_agrees(&self, roles: &BTreeSet<&str>) -> Result<(), CustomerError> {
        if !self.configuration.conffiles.is_empty() {
            return Err(unsupported(format!(
                "release manifest declares conffiles {:?}; the sealed origin config is operator state, never a conffile",
                self.configuration.conffiles
            )));
        }
        for example in &self.configuration.examples {
            if !roles.contains(example.as_str()) {
                return Err(unsupported(format!(
                    "release manifest lists example {example} but ships no member for it"
                )));
            }
        }
        Ok(())
    }

    /// Compare a parsed document to what was actually shipped.
    ///
    /// DT-1: exact architecture, binary build identity and hashes must agree
    /// before selection. A tampered payload — one whose bytes digest to
    /// something else — is refused here rather than after extraction.
    pub(crate) fn verify_identity(&self, expected: &PayloadIdentity) -> Result<(), CustomerError> {
        if self.architecture != expected.architecture {
            return Err(identity_changed(format!(
                "release manifest architecture is {}; the payload was built for {}",
                self.architecture.as_str(),
                expected.architecture.as_str()
            )));
        }
        if self.product.version != expected.product_version {
            return Err(identity_changed(format!(
                "release manifest version is {}; the payload is {}",
                self.product.version, expected.product_version
            )));
        }
        for (label, declared, observed) in [
            (
                "cli_sha256",
                self.build.cli_sha256.as_str(),
                expected.cli_sha256.as_str(),
            ),
            (
                "daemon_sha256",
                self.build.daemon_sha256.as_str(),
                expected.daemon_sha256.as_str(),
            ),
        ] {
            if declared != observed {
                return Err(identity_changed(format!(
                    "release manifest build.{label} is {declared}; the shipped binary digests to {observed}"
                )));
            }
        }
        Ok(())
    }

    /// Compare one member's shipped bytes to the digest this manifest declares.
    ///
    /// This is the tampering check: the declaration is authoritative, and
    /// bytes that digest to something else are not the member it names.
    pub(crate) fn verify_member_bytes(
        &self,
        role: &str,
        bytes: &[u8],
    ) -> Result<(), CustomerError> {
        let Some(member) = self.contents.iter().find(|member| member.role == role) else {
            return Err(identity_changed(format!(
                "release manifest declares no member {role}"
            )));
        };
        let Some(declared) = member.sha256.as_deref() else {
            return Err(unsupported(format!(
                "release manifest member {role} is generated and has no digest to verify"
            )));
        };
        let observed = sha256_hex(bytes);
        if declared != observed {
            return Err(identity_changed(format!(
                "release manifest member {role} is declared {declared}; the shipped bytes digest to {observed}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests;
