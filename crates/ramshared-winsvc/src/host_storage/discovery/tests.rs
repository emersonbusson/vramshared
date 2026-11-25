// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/discovery/tests.rs
use super::*;

#[test]
fn selectors_require_exact_sid_and_registration_guid() {
    let good = r"{01234567-89ab-cdef-0123-456789abcdef}";
    assert!(registry_key("S-1-5-21-42-1000", good).is_some());
    for bad in ["", "S-1-5-21-x", "S-1-5-21-42\\other", "S-1-5-21-42-"] {
        assert!(registry_key(bad, good).is_none());
    }
    for bad in [
        "ubuntu",
        "{00000000-0000-0000-0000-000000000000}",
        "{01234567-89ab-cdef-0123-456789abcdeg}",
        "{01234567-89ab-cdef-0123-456789abcdef}\\evil",
    ] {
        assert!(registry_key("S-1-5-21-42-1000", bad).is_none());
    }
}

#[test]
fn registration_catalog_rejects_malformed_duplicate_and_oversized_entries() {
    let entry = RegistrationKey {
        guid: "{01234567-89ab-cdef-0123-456789abcdef}".into(),
        last_write: 7,
    };
    assert!(validate_keys(
        "S-1-5-21-42-1000",
        std::slice::from_ref(&entry)
    ));
    assert!(!validate_keys(
        "S-1-5-21-42-1000",
        &[entry.clone(), entry.clone()]
    ));
    let bad = RegistrationKey {
        guid: "ubuntu".into(),
        last_write: 7,
    };
    assert!(!validate_keys("S-1-5-21-42-1000", &[bad]));
    assert!(!validate_keys(
        "S-1-5-21-42-1000",
        &vec![entry; MAX_REGISTRATIONS + 1]
    ));
}

#[test]
fn root_disk_matching_refuses_unobserved_and_duplicate_registrations() {
    fn registration(name: &str) -> WslRegistration {
        WslRegistration {
            user_sid: "S-1-5-21-42-1000".into(),
            registration_guid: name.into(),
            distribution_name: name.into(),
            vhdx_path: name.into(),
        }
    }
    let catalog = UserRegistrationCatalog {
        observed_at_unix_ms: 100,
        registrations: vec![registration("first"), registration("second")],
    };
    let selected = match_registration(&catalog, [7; 16], |path| {
        if path == std::path::Path::new("first") {
            Some([7; 16])
        } else {
            Some([8; 16])
        }
    });
    assert_eq!(selected.unwrap().distribution_name, "first");
    assert!(match_registration(&catalog, [7; 16], |_| Some([7; 16])).is_none());
    assert!(
        match_registration(&catalog, [7; 16], |path| {
            if path == std::path::Path::new("first") {
                Some([7; 16])
            } else {
                None
            }
        })
        .is_none()
    );
    assert!(match_registration(&catalog, [0; 16], |_| Some([7; 16])).is_none());
    assert!(match_registration(&catalog, [9; 16], |_| Some([7; 16])).is_none());
}
