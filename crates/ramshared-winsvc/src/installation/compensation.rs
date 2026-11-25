// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/compensation.rs
//! Compensate candidate effects an install created before its commit.
//!
//! Publication and root-device registration happen before the SCM mutation so
//! a refusal leaves the previous version untouched. Once either effect exists,
//! a later refusal must take it back. Order matters: the root-enumerated device
//! must be gone before its driver package can be removed, or the package stays
//! in use. Every requested removal is attempted even when an earlier one
//! fails, and the failures are returned in effect order.

/// Candidate effects that must be undone when install has not committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CandidateEffects {
    /// This install created the owned root device; a pre-existing one is not ours.
    pub root_device_created: bool,
    /// The restored version does not run these driver bytes, so they may be retired.
    pub retire_package: bool,
}

/// Attempt the requested compensations in order and return their failures.
///
/// A failed root-device removal does not skip the package removal: the attempt
/// is recorded and the next effect still runs, because a device left behind is
/// a different defect from a package left behind.
pub fn compensate_candidate(
    effects: CandidateEffects,
    mut remove_root_device: impl FnMut() -> Result<(), String>,
    mut remove_package: impl FnMut() -> Result<(), String>,
) -> Vec<String> {
    let mut failures = Vec::new();
    if effects.root_device_created
        && let Err(error) = remove_root_device()
    {
        failures.push(format!("created root device: {error}"));
    }
    if effects.retire_package
        && let Err(error) = remove_package()
    {
        failures.push(format!("candidate driver package: {error}"));
    }
    failures
}

/// Join the primary failure with any compensation failures.
///
/// A clean compensation reports only what failed; the caller never has to
/// claim a rollback that did not happen.
pub fn rollback_message(context: &str, primary: &str, failures: &[String]) -> String {
    if failures.is_empty() {
        format!("{context}: {primary}")
    } else {
        format!(
            "{context}: {primary}; cleanup failed: {}",
            failures.join("; ")
        )
    }
}

/// Add a service-restore failure to a registration rollback message, then the
/// candidate cleanup failures.
///
/// The restore runs first, but it is not a reason to skip taking the candidate
/// back: the candidate was never committed, and leaving its root device or
/// package behind hides one failure behind another. Both are named.
pub fn registration_rollback_message(
    primary: &str,
    restore: Result<(), String>,
    failures: &[String],
) -> String {
    let mut combined = primary.to_string();
    if let Err(error) = restore {
        combined.push_str("; service restore failed: ");
        combined.push_str(&error);
    }
    rollback_message("candidate registration rolled back", &combined, failures)
}

/// A just-published package may be taken back only when the durable receipt is
/// readable and does not already name it.
///
/// An unreadable receipt could still name the bytes the restored version runs,
/// so removal would break that version; keeping the package is the fail-safe
/// choice, and the install error still names the receipt failure.
pub fn may_retire_unrecorded(previous: Result<Option<Vec<String>>, String>, oem_inf: &str) -> bool {
    match previous {
        Ok(Some(names)) => !names.iter().any(|name| name == oem_inf),
        Ok(None) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn root_device_removal_precedes_package_removal() {
        let order = std::cell::RefCell::new(Vec::new());
        let failures = compensate_candidate(
            CandidateEffects {
                root_device_created: true,
                retire_package: true,
            },
            || {
                order.borrow_mut().push("root");
                Ok(())
            },
            || {
                order.borrow_mut().push("package");
                Ok(())
            },
        );
        assert!(failures.is_empty());
        assert_eq!(*order.borrow(), vec!["root", "package"]);
    }

    #[test]
    fn a_failed_root_removal_still_attempts_the_package() {
        let mut package_attempted = false;
        let failures = compensate_candidate(
            CandidateEffects {
                root_device_created: true,
                retire_package: true,
            },
            || Err("removal left 1 owned device(s) behind".into()),
            || {
                package_attempted = true;
                Ok(())
            },
        );
        assert!(package_attempted);
        assert_eq!(
            failures,
            vec!["created root device: removal left 1 owned device(s) behind"]
        );
    }

    #[test]
    fn a_preexisting_root_device_is_not_removed() {
        let mut removed = false;
        let failures = compensate_candidate(
            CandidateEffects {
                root_device_created: false,
                retire_package: true,
            },
            || {
                removed = true;
                Ok(())
            },
            || Ok(()),
        );
        assert!(!removed);
        assert!(failures.is_empty());
    }

    #[test]
    fn a_shared_package_is_not_retired() {
        let mut removed = false;
        let failures = compensate_candidate(
            CandidateEffects {
                root_device_created: false,
                retire_package: false,
            },
            || Ok(()),
            || {
                removed = true;
                Ok(())
            },
        );
        assert!(!removed);
        assert!(failures.is_empty());
    }

    #[test]
    fn both_failures_are_reported_in_effect_order() {
        let failures = compensate_candidate(
            CandidateEffects {
                root_device_created: true,
                retire_package: true,
            },
            || Err("access denied".into()),
            || Err("package is in use".into()),
        );
        assert_eq!(
            failures,
            vec![
                "created root device: access denied",
                "candidate driver package: package is in use",
            ]
        );
    }

    #[test]
    fn a_clean_compensation_reports_only_the_primary_failure() {
        let message = rollback_message("candidate registration rolled back", "scm denied", &[]);
        assert_eq!(message, "candidate registration rolled back: scm denied");
    }

    #[test]
    fn cleanup_failures_are_appended_to_the_primary_failure() {
        let message = rollback_message(
            "candidate registration rolled back",
            "scm denied",
            &[
                "created root device: access denied".to_string(),
                "candidate driver package: in use".to_string(),
            ],
        );
        assert_eq!(
            message,
            "candidate registration rolled back: scm denied; cleanup failed: \
             created root device: access denied; candidate driver package: in use"
        );
    }

    #[test]
    fn a_service_restore_failure_is_named_alongside_candidate_cleanup() {
        let message = registration_rollback_message(
            "service definition mismatch",
            Err("old service registration denied".to_string()),
            &["candidate driver package: access denied".to_string()],
        );
        assert_eq!(
            message,
            "candidate registration rolled back: service definition mismatch; \
             service restore failed: old service registration denied; cleanup failed: \
             candidate driver package: access denied"
        );
    }

    #[test]
    fn a_successful_service_restore_keeps_only_the_primary_failure() {
        let message = registration_rollback_message("scm denied", Ok(()), &[]);
        assert_eq!(message, "candidate registration rolled back: scm denied");
    }

    #[test]
    fn an_absent_receipt_allows_take_back() {
        assert!(may_retire_unrecorded(Ok(None), "oem38.inf"));
    }

    #[test]
    fn a_recorded_package_is_kept() {
        assert!(!may_retire_unrecorded(
            Ok(Some(vec!["oem38.inf".to_string()])),
            "oem38.inf"
        ));
        assert!(may_retire_unrecorded(
            Ok(Some(vec!["oem39.inf".to_string()])),
            "oem38.inf"
        ));
    }

    #[test]
    fn an_unreadable_receipt_never_authorises_removal() {
        assert!(!may_retire_unrecorded(
            Err("schema 2 is not 1".to_string()),
            "oem38.inf"
        ));
    }
}
