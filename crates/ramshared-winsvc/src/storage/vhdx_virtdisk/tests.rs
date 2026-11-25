// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/storage/vhdx_virtdisk/tests.rs
#![allow(clippy::unwrap_used)]

use super::*;

fn lease(path: &str, guid: &str) -> VhdxLease {
    VhdxLease {
        schema_version: VHDX_LEASE_SCHEMA,
        vhdx_path: path.to_string(),
        disk_guid: guid.to_string(),
        physical_drive_number: 2,
        boot_id: "boot-1".to_string(),
        attached_at_unix_ms: 1,
    }
}

fn fixture_path(name: &str) -> String {
    std::env::temp_dir()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

#[test]
fn vhdx_virtdisk_attach_idempotent_and_refuses_detach_when_lease_active() {
    let first = lease(&fixture_path("ramshared-origin.vhdx"), "guid-1");
    let mut receipt = VhdxReceipt::new(first.clone()).unwrap();
    assert!(receipt.is_active());
    assert!(receipt.attach_idempotent(&first).is_ok());
    assert!(matches!(
        receipt.attach_idempotent(&lease(&fixture_path("ramshared-other.vhdx"), "guid-2")),
        Err(VhdxError::LeaseActive)
    ));
    assert!(matches!(
        receipt.detach_allowed(1, false),
        Err(VhdxError::DetachRefusedActiveLease)
    ));
    assert!(matches!(
        receipt.detach_allowed(0, true),
        Err(VhdxError::DetachRefusedActiveLease)
    ));
    assert!(receipt.is_active());
    receipt.detach_allowed(0, false).unwrap();
    assert!(!receipt.is_active());

    let root = std::env::temp_dir().join(format!("ramshared-vhdx-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(VHDX_LEASE_FILE);
    write_lease_atomic(&path, &first).unwrap();
    assert_eq!(read_lease(&path).unwrap(), Some(first));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn vhdx_lease_rejects_relative_and_incomplete_receipts() {
    let mut invalid = lease("relative.vhdx", "guid");
    assert!(matches!(invalid.validate(), Err(VhdxError::InvalidLease)));
    invalid.vhdx_path = fixture_path("ramshared-origin.vhdx");
    invalid.schema_version = 2;
    assert!(matches!(invalid.validate(), Err(VhdxError::InvalidLease)));
    let temp = std::env::temp_dir().join(format!("ramshared-vhdx-read-{}", std::process::id()));
    assert_eq!(read_lease(&temp.join("missing.json")).unwrap(), None);
}
