// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/plan_document.rs
//! Wire form of a [`SetupPlan`] and the name that selects its profile (ITEM-5).
//!
//! `ramshared setup plan --output` writes this document for the supervised
//! native storage-apply reader. There is **one** schema: the
//! envelope carries selection metadata and the plan body is the [`SetupPlan`]
//! type itself, so no second field list can drift away from the arithmetic.
//!
//! Integrity is the plan's own [`SetupPlan::fingerprint`]. On parse it is
//! recomputed from the body and compared with the stored value, so a document
//! edited after it was written is a malformed request on a new surface
//! (`unsupported_surface`, exit 2), never a plan that silently means something
//! else.
//!
//! The profile name is a **selection label**, never a path. Its grammar is the
//! confinement: one path component under the configuration root, so argv can
//! never carry a device path, an executable or a traversal.

use std::path::{Path, PathBuf};

use ramshared_config::resource_profile::MAX_RESOURCE_PROFILE_BYTES;
use serde::{Deserialize, Serialize};

use super::plan::SetupPlan;
use super::{CustomerError, CustomerReason, CustomerState};

/// Document schema version. A reader that does not know this number refuses
/// rather than guessing at the fields it means.
pub(crate) const PLAN_DOCUMENT_SCHEMA_VERSION: u32 = 4;

/// Maximum encoded plan document, in bytes. The same 64 KiB bound the SPEC
/// puts on every setup input, so one size limit covers plan in and plan out.
pub(crate) const MAX_PLAN_DOCUMENT_BYTES: usize = MAX_RESOURCE_PROFILE_BYTES;

/// Maximum profile name, in bytes. SPEC: "maximum profile name 64 bytes".
pub(crate) const MAX_PROFILE_NAME_BYTES: usize = 64;

/// The document `setup plan` writes for the supervised setup-apply reader.
///
/// `plan_fingerprint` is not free-standing authority: it is recomputed from
/// `plan` on parse and must match. A mismatch is a malformed document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetupPlanDocument {
    pub(crate) schema_version: u32,
    pub(crate) profile_name: String,
    pub(crate) plan: SetupPlan,
    pub(crate) plan_fingerprint: String,
}

fn malformed(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::UnsupportedSurface,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

fn observation_unavailable(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::ObservationUnavailable,
        detail: detail.into(),
        state: CustomerState::UnknownBlocked,
    }
}

/// Admit a profile **name**.
///
/// A name is one path component and nothing else: the first byte is
/// alphanumeric and the rest are alphanumeric plus `.` `_` `-`. That excludes
/// path separators, `..`, a leading `.` or `-`, the empty name and any name
/// over [`MAX_PROFILE_NAME_BYTES`]. The grammar is the confinement — there is
/// no path argument to sanitize because no path is ever accepted.
pub(crate) fn validate_profile_name(name: &str) -> Result<(), CustomerError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return Err(malformed("profile name is empty"));
    }
    if bytes.len() > MAX_PROFILE_NAME_BYTES {
        return Err(malformed(format!(
            "profile name is {} bytes; the bound is {MAX_PROFILE_NAME_BYTES}",
            bytes.len()
        )));
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return Err(malformed(
            "profile name must start with an ASCII alphanumeric character",
        ));
    }
    for &byte in &bytes[1..] {
        if byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || byte == b'-' {
            continue;
        }
        return Err(malformed(format!(
            "profile name byte {byte:#04x} is outside [A-Za-z0-9._-]"
        )));
    }
    if name == "." || name == ".." {
        return Err(malformed("profile name must not be a dot segment"));
    }
    Ok(())
}

/// Resolve a validated profile name to its one configuration file.
///
/// The join is `<root>/<name>.toml`. The parent is re-checked against `root`
/// after the join so a future grammar change cannot silently widen the
/// confinement; a name is never a path and never escapes the root.
pub(crate) fn resolve_profile_path(root: &Path, name: &str) -> Result<PathBuf, CustomerError> {
    validate_profile_name(name)?;
    let joined = root.join(format!("{name}.toml"));
    if joined.parent() != Some(root) {
        return Err(malformed(
            "profile name resolved outside the configuration root",
        ));
    }
    Ok(joined)
}

/// Encode a plan as the document `--output` receives.
///
/// The fingerprint is computed here from the plan body, so a document can
/// never be written with a fingerprint its own fields do not produce.
pub(crate) fn encode_plan_document(
    plan: &SetupPlan,
    profile_name: &str,
) -> Result<String, CustomerError> {
    validate_profile_name(profile_name)?;
    plan.validate_operation()
        .map_err(|error| malformed(format!("setup operation is invalid: {error}")))?;
    let document = SetupPlanDocument {
        schema_version: PLAN_DOCUMENT_SCHEMA_VERSION,
        profile_name: profile_name.to_string(),
        plan: plan.clone(),
        plan_fingerprint: plan.fingerprint(),
    };
    let body = serde_json::to_string(&document).map_err(|error| {
        observation_unavailable(format!("cannot encode the setup plan document: {error}"))
    })?;
    if body.len() > MAX_PLAN_DOCUMENT_BYTES {
        return Err(malformed(format!(
            "encoded plan document is {} bytes; the bound is {MAX_PLAN_DOCUMENT_BYTES}",
            body.len()
        )));
    }
    Ok(body)
}

/// Decode a plan document and prove its fingerprint still describes its body.
///
/// Returns the plan and the profile name it was written under. Every refusal
/// here is a malformed request on a new surface: exit 2,
/// [`CustomerReason::UnsupportedSurface`], never a shared errno.
pub(crate) fn parse_plan_document(text: &str) -> Result<(SetupPlan, String), CustomerError> {
    if text.len() > MAX_PLAN_DOCUMENT_BYTES {
        return Err(malformed(format!(
            "plan document is {} bytes; the bound is {MAX_PLAN_DOCUMENT_BYTES}",
            text.len()
        )));
    }
    let document: SetupPlanDocument = serde_json::from_str(text).map_err(|error| {
        malformed(format!(
            "plan document is not a strict plan object: {error}"
        ))
    })?;
    if document.schema_version != PLAN_DOCUMENT_SCHEMA_VERSION {
        return Err(malformed(format!(
            "plan document schema_version is {}; this reader admits {PLAN_DOCUMENT_SCHEMA_VERSION}",
            document.schema_version
        )));
    }
    validate_profile_name(&document.profile_name)?;
    let recomputed = document.plan.fingerprint();
    if recomputed != document.plan_fingerprint {
        return Err(malformed(
            "plan fingerprint does not describe the plan body; the document was altered after it was written",
        ));
    }
    document
        .plan
        .validate_operation()
        .map_err(|error| malformed(format!("setup operation is invalid: {error}")))?;
    Ok((document.plan, document.profile_name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::readiness::Decision;

    pub(super) fn fixture_plan() -> SetupPlan {
        let gpu = crate::customer::gpu_plan::tests::fixture_plan();
        let policy = ramshared_vram::ReserveFloorPolicy::sealed();
        SetupPlan {
            operation: super::super::setup_scope::SetupOperation::GpuCache {
                cache: Box::new(super::super::setup_scope::CachePreparationPlan {
                    schema_version: super::super::setup_scope::CACHE_PLAN_SCHEMA_VERSION,
                    gpus: vec![super::super::gpu_set::CacheAdapterPlan {
                        configured_reserve_bytes: policy
                            .configured_reserve_bytes(gpu.capacity_bytes()),
                        runtime_free_buffer_bytes: ramshared_vram::RUNTIME_FREE_BUFFER_BYTES,
                        gpu: gpu.clone(),
                    }],
                    required_lower_sink_bytes:
                        ramshared_tier::nbd_readiness::minimum_lower_tier_bytes(gpu.target_bytes)
                            .unwrap(),
                    lower_sink_identity: "lab-lower-sink".into(),
                }),
            },
            release_version: "0.15.0".into(),
            profile_schema_version: 1,
            profile_fingerprint: "a".repeat(64),
            report_decision: Decision::Ready,
        }
    }

    /// A name is a selection label, never a path. Every form that could name
    /// a device, an executable or a traversal is refused before any join.
    #[test]
    fn profile_name_is_a_bounded_label_and_never_a_path() {
        for name in [
            "resource-profile",
            "a",
            "A1._-x",
            "profile.toml", // the stem is the name; the suffix is appended
        ] {
            validate_profile_name(name)
                .unwrap_or_else(|error| panic!("{name} must be admitted: {error}"));
        }

        for name in [
            "",
            ".",
            "..",
            "../etc/passwd",
            "a/b",
            "a\\b",
            "/etc/ramshared/resource-profile",
            "-leading",
            "_leading",
            ".leading",
            "with space",
            "with:colon",
            &"x".repeat(MAX_PROFILE_NAME_BYTES + 1),
        ] {
            let error = validate_profile_name(name)
                .expect_err("a name outside the grammar must be refused");
            assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
            assert_eq!(error.exit_code(), 2);
            assert_eq!(error.state, CustomerState::InstalledOff);
        }

        // Exactly 64 bytes is admitted; 65 is not. The bound is bytes, not
        // characters, so a multi-byte name that counts 64 chars is refused.
        let at_bound = "x".repeat(MAX_PROFILE_NAME_BYTES);
        validate_profile_name(&at_bound).expect("64 bytes is inside the bound");
        assert_eq!(MAX_PROFILE_NAME_BYTES, 64, "the SPEC bound drifted");

        let root = Path::new("/etc/ramshared");
        let resolved = resolve_profile_path(root, "resource-profile").expect("resolves");
        assert_eq!(resolved, root.join("resource-profile.toml"));
        for name in ["..", "a/b", ""] {
            resolve_profile_path(root, name).expect_err("traversal never resolves");
        }
    }

    /// One schema, both directions, and a fingerprint that describes the body.
    #[test]
    fn plan_document_round_trips_and_refuses_an_altered_body() {
        let plan = fixture_plan();
        let body = encode_plan_document(&plan, "resource-profile").expect("encodes");
        assert!(
            body.len() <= MAX_PLAN_DOCUMENT_BYTES,
            "an encoded plan must fit the documented bound"
        );

        let (decoded, profile_name) = parse_plan_document(&body).expect("round-trips");
        assert_eq!(decoded, plan);
        assert_eq!(profile_name, "resource-profile");

        // An unknown field is refused: an ignored field is how a second
        // meaning gets smuggled into a plan.
        let with_extra = body.replace(
            "\"profile_name\":\"resource-profile\"",
            "\"profile_name\":\"resource-profile\",\"extra\":true",
        );
        let refused_extra = parse_plan_document(&with_extra).expect_err("unknown field refused");
        assert_eq!(refused_extra.reason, CustomerReason::UnsupportedSurface);

        // A field edited after the fingerprint was computed is refused, not
        // silently reinterpreted as a different plan.
        let altered = body.replace("2147483648", "2147483649");
        assert_ne!(altered, body, "the fixture must expose the mutated field");
        let refused_altered =
            parse_plan_document(&altered).expect_err("an altered body is refused");
        assert_eq!(refused_altered.reason, CustomerReason::UnsupportedSurface);
        assert!(
            refused_altered.detail.contains("fingerprint"),
            "the refusal must name the integrity check: {}",
            refused_altered.detail
        );

        // A wrong or unknown schema version is refused, not guessed at.
        let wrong_version = body.replace(
            &format!("\"schema_version\":{PLAN_DOCUMENT_SCHEMA_VERSION}"),
            "\"schema_version\":999",
        );
        let refused_version =
            parse_plan_document(&wrong_version).expect_err("unknown schema refused");
        assert_eq!(refused_version.reason, CustomerReason::UnsupportedSurface);

        // Malformed and oversized input never parse as a plan.
        parse_plan_document("{not json").expect_err("malformed refused");
        let oversize = "x".repeat(MAX_PLAN_DOCUMENT_BYTES + 1);
        let refused_oversize = parse_plan_document(&oversize).expect_err("oversize refused");
        assert_eq!(refused_oversize.reason, CustomerReason::UnsupportedSurface);

        // A bad name in a structurally valid document is refused on parse, so
        // a document cannot carry a name the writer would not have admitted.
        let bad_name_plan = SetupPlanDocument {
            schema_version: PLAN_DOCUMENT_SCHEMA_VERSION,
            profile_name: "../escape".to_string(),
            plan: fixture_plan(),
            plan_fingerprint: fixture_plan().fingerprint(),
        };
        let bad_name_body = serde_json::to_string(&bad_name_plan).unwrap();
        let refused_name = parse_plan_document(&bad_name_body).expect_err("bad name refused");
        assert_eq!(refused_name.reason, CustomerReason::UnsupportedSurface);

        // The writer refuses the same name before it can reach a document.
        encode_plan_document(&plan, "../escape").expect_err("the writer refuses the same name");
    }
    #[test]
    fn plan_identity_binds_readiness_and_field_boundaries() {
        let plan = fixture_plan();
        let body = encode_plan_document(&plan, "resource-profile").expect("encode");
        let altered = body.replace(
            "\"report_decision\":\"ready\"",
            "\"report_decision\":\"blocked\"",
        );
        assert_ne!(altered, body);
        assert!(
            parse_plan_document(&altered).is_err(),
            "readiness is part of plan identity"
        );
        let mut old: serde_json::Value = serde_json::from_str(&body).expect("document");
        old["schema_version"] = serde_json::json!(1);
        assert!(
            parse_plan_document(&old.to_string()).is_err(),
            "v1 requires re-planning"
        );
    }

    #[test]
    fn plan_identity_refuses_delimiter_collisions() {
        let plan = fixture_plan();
        let mut left = plan.clone();
        let super::super::setup_scope::SetupOperation::GpuCache { cache } = &mut left.operation
        else {
            panic!("cache fixture")
        };
        cache.lower_sink_identity = "sink|part".into();
        left.release_version = "version".into();
        let mut right = plan;
        let super::super::setup_scope::SetupOperation::GpuCache { cache } = &mut right.operation
        else {
            panic!("cache fixture")
        };
        cache.lower_sink_identity = "sink".into();
        right.release_version = "part|version".into();
        assert_ne!(
            left.fingerprint(),
            right.fingerprint(),
            "field boundaries must be unambiguous"
        );
    }
    #[test]
    fn every_plan_body_field_participates_in_the_fingerprint() {
        let body = encode_plan_document(
            &super::node_binding_tests::complete_fixture(),
            "resource-profile",
        )
        .expect("encode");
        let original: serde_json::Value = serde_json::from_str(&body).expect("document");
        fn leaves(value: &serde_json::Value, path: Vec<String>, out: &mut Vec<Vec<String>>) {
            if let Some(object) = value.as_object() {
                for (key, child) in object {
                    let mut next = path.clone();
                    next.push(key.clone());
                    leaves(child, next, out);
                }
            } else if let Some(array) = value.as_array() {
                for (index, child) in array.iter().enumerate() {
                    let mut next = path.clone();
                    next.push(index.to_string());
                    leaves(child, next, out);
                }
            } else {
                out.push(path);
            }
        }
        let mut fields = Vec::new();
        leaves(&original["plan"], Vec::new(), &mut fields);
        for path in fields {
            let mut altered = original.clone();
            let mut value = &mut altered["plan"];
            for key in &path {
                value = if value.is_array() {
                    value
                        .get_mut(key.parse::<usize>().expect("array index"))
                        .expect("array member")
                } else {
                    value.get_mut(key).expect("object field")
                };
            }
            let field = path.last().expect("field");
            *value = if field == "kind" {
                serde_json::json!("storage_only")
            } else if field == "report_decision" {
                serde_json::json!("blocked")
            } else if field == "source" {
                serde_json::json!("provider_local_estimate")
            } else if value.is_null() && field == "luid" {
                serde_json::json!("12345678:00000001")
            } else if value.is_null() {
                serde_json::json!(1)
            } else if let Some(number) = value.as_u64() {
                serde_json::json!(number + 1)
            } else if let Some(text) = value.as_str() {
                serde_json::json!(format!("{text}-changed"))
            } else {
                panic!("define a valid identity mutation for new plan field {path:?}")
            };
            let error = parse_plan_document(&altered.to_string())
                .expect_err("every nested body field must be bound");
            assert!(
                field == "kind" || error.detail.contains("fingerprint"),
                "{path:?}: {}",
                error.detail
            );
        }
    }
    #[test]
    fn plan_document_refuses_old_nested_or_inconsistent_gpu_proof() {
        let body = encode_plan_document(&fixture_plan(), "resource-profile").unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        for version in [1, 2, 3] {
            let mut old = value.clone();
            old["schema_version"] = serde_json::json!(version);
            assert!(
                parse_plan_document(&old.to_string())
                    .unwrap_err()
                    .detail
                    .contains("schema_version")
            );
        }
        for adapter in [false, true] {
            let mut unknown = value.clone();
            let target = if adapter {
                &mut unknown["plan"]["operation"]["cache"]["gpus"][0]["gpu"]["budget"]["adapter"]
            } else {
                &mut unknown["plan"]["operation"]["cache"]["gpus"][0]["gpu"]["budget"]
            };
            target["unknown_authority"] = serde_json::json!(true);
            assert!(
                parse_plan_document(&unknown.to_string())
                    .unwrap_err()
                    .detail
                    .contains("unknown field")
            );
        }
        let mut invalid = fixture_plan();
        let super::super::setup_scope::SetupOperation::GpuCache { cache } = &mut invalid.operation
        else {
            panic!("cache fixture")
        };
        cache.gpus[0].gpu.target_bytes += 1;
        assert!(encode_plan_document(&invalid, "resource-profile").is_err());
        let forged = SetupPlanDocument {
            schema_version: PLAN_DOCUMENT_SCHEMA_VERSION,
            profile_name: "resource-profile".into(),
            plan_fingerprint: invalid.fingerprint(),
            plan: invalid,
        };
        assert!(
            parse_plan_document(&serde_json::to_string(&forged).unwrap())
                .unwrap_err()
                .detail
                .contains("operation")
        );
    }
    #[test]
    fn setup_scope_identity_refuses_cross_scope_replay() {
        use super::super::setup_scope::SetupOperation;
        let cache = fixture_plan();
        let mut storage = cache.clone();
        storage.operation = SetupOperation::StorageOnly {};
        storage.report_decision = Decision::Blocked;
        let text = encode_plan_document(&storage, "resource-profile").unwrap();
        let (readback, _) = parse_plan_document(&text).unwrap();
        assert_eq!(readback, storage);
        assert_ne!(storage.fingerprint(), cache.fingerprint());
        let mut replay: serde_json::Value = serde_json::from_str(&text).unwrap();
        replay["plan_fingerprint"] = serde_json::json!(cache.fingerprint());
        assert!(
            parse_plan_document(&replay.to_string())
                .unwrap_err()
                .detail
                .contains("fingerprint")
        );
        let mut injected: serde_json::Value = serde_json::from_str(&text).unwrap();
        injected["plan"]["operation"]["cache"] = serde_json::json!({});
        assert!(
            parse_plan_document(&injected.to_string())
                .unwrap_err()
                .detail
                .contains("unknown field")
        );
        let mut old: serde_json::Value = serde_json::from_str(&text).unwrap();
        old["schema_version"] = serde_json::json!(3);
        assert!(
            parse_plan_document(&old.to_string())
                .unwrap_err()
                .detail
                .contains("schema_version")
        );
    }
    #[test]
    fn setup_cache_scope_refuses_inconsistent_capture() {
        use super::super::setup_scope::SetupOperation;
        for mutation in 0..6 {
            let mut plan = fixture_plan();
            let SetupOperation::GpuCache { cache } = &mut plan.operation else {
                panic!("cache fixture")
            };
            match mutation {
                0 => cache.gpus[0].configured_reserve_bytes += 1,
                1 => cache.gpus[0].runtime_free_buffer_bytes += 1,
                2 => cache.required_lower_sink_bytes += 1,
                3 => cache.lower_sink_identity.clear(),
                4 => cache.schema_version += 1,
                _ => plan.report_decision = Decision::Blocked,
            }
            assert!(encode_plan_document(&plan, "resource-profile").is_err());
            let forged = SetupPlanDocument {
                schema_version: PLAN_DOCUMENT_SCHEMA_VERSION,
                profile_name: "resource-profile".into(),
                plan_fingerprint: plan.fingerprint(),
                plan,
            };
            assert!(
                parse_plan_document(&serde_json::to_string(&forged).unwrap()).is_err(),
                "recomputing the hash cannot make invalid cache metadata admissible"
            );
        }
    }
    #[test]
    fn reviewed_cache_set_binds_later_members_and_refuses_singular_bodies() {
        use super::super::setup_scope::SetupOperation;
        let mut plan = fixture_plan();
        let SetupOperation::GpuCache { cache } = &mut plan.operation else {
            panic!("cache fixture")
        };
        let mut second = cache.gpus[0].clone();
        second.gpu.budget.adapter.as_mut().unwrap().key = "ffeeddccbbaa99887766554433221100".into();
        cache.gpus.push(second);
        let total = super::super::gpu_set::validate(&cache.gpus).unwrap();
        cache.required_lower_sink_bytes =
            ramshared_tier::nbd_readiness::minimum_lower_tier_bytes(total).unwrap();
        let encoded = encode_plan_document(&plan, "resource-profile").unwrap();
        assert_eq!(parse_plan_document(&encoded).unwrap().0, plan);
        let mut changed = plan.clone();
        let SetupOperation::GpuCache { cache } = &mut changed.operation else {
            panic!("cache fixture")
        };
        cache.gpus[1].gpu.budget.adapter.as_mut().unwrap().luid = Some("12345678:00000001".into());
        assert_ne!(
            plan.fingerprint(),
            changed.fingerprint(),
            "second adapter was omitted from the reviewed identity"
        );
        let mut document: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        document["plan"] = serde_json::to_value(&changed).unwrap();
        assert!(
            parse_plan_document(&document.to_string())
                .unwrap_err()
                .detail
                .contains("fingerprint")
        );
        let mut old: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        let cache = old["plan"]["operation"]["cache"].as_object_mut().unwrap();
        let members = cache.remove("gpus").unwrap();
        cache.remove("schema_version");
        cache.insert("gpu".into(), members[0]["gpu"].clone());
        assert!(
            parse_plan_document(&old.to_string()).is_err(),
            "old singular cache body was silently migrated"
        );
    }

    #[test]
    fn storage_only_plan_retains_its_version_four_identity() {
        let mut plan = fixture_plan();
        plan.operation = super::super::setup_scope::SetupOperation::StorageOnly {};
        plan.report_decision = Decision::Blocked;
        assert_eq!(
            plan.fingerprint(),
            "5a92281e75742d43395156353f5ac588dcd76f8b6169c06424b1f26c084f55f4"
        );
        let encoded = encode_plan_document(&plan, "resource-profile").unwrap();
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["schema_version"], 4);
        assert_eq!(
            value["plan"]["operation"],
            serde_json::json!({"kind":"storage_only"})
        );
        assert_eq!(parse_plan_document(&encoded).unwrap().0, plan);
    }
}

#[cfg(test)]
mod node_binding_tests;
