// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/storage/vhdx_virtdisk/edge_tests.rs
#![allow(clippy::unwrap_used)]

use super::*;

fn lease() -> VhdxLease {
    VhdxLease {
        schema_version: VHDX_LEASE_SCHEMA,
        vhdx_path: std::env::temp_dir()
            .join("ramshared-edge-origin.vhdx")
            .to_string_lossy()
            .into_owned(),
        disk_guid: "guid".to_string(),
        physical_drive_number: 1,
        boot_id: "boot".to_string(),
        attached_at_unix_ms: 1,
    }
}

#[test]
fn vhdx_error_and_receipt_edges_are_total() {
    for error in [
        VhdxError::InvalidLease,
        VhdxError::LeaseActive,
        VhdxError::DetachRefusedActiveLease,
        VhdxError::VirtDisk("x".to_string()),
        VhdxError::NotWindows,
        VhdxError::Io(std::io::Error::other("x")),
    ] {
        assert!(!error.to_string().is_empty());
    }
    let io: VhdxError = std::io::Error::other("x").into();
    assert!(matches!(io, VhdxError::Io(_)));
    assert!(VhdxReceipt::from_lease(None).unwrap().lease().is_none());
    assert!(VhdxReceipt::from_lease(Some(lease())).unwrap().is_active());
    let mut missing_guid = lease();
    missing_guid.disk_guid.clear();
    assert!(matches!(
        missing_guid.validate(),
        Err(VhdxError::InvalidLease)
    ));
    let mut missing_boot = lease();
    missing_boot.boot_id.clear();
    assert!(matches!(
        missing_boot.validate(),
        Err(VhdxError::InvalidLease)
    ));
    let mut receipt = VhdxReceipt::new(lease()).unwrap();
    assert!(matches!(
        receipt.attach_idempotent(&VhdxLease {
            vhdx_path: "relative".to_string(),
            ..lease()
        }),
        Err(VhdxError::InvalidLease)
    ));
    assert!(receipt.detach_allowed(0, false).is_ok());
    assert!(receipt.detach_allowed(0, false).is_ok());

    let root = std::env::temp_dir().join(format!("ramshared-vhdx-edge-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    assert_eq!(
        lease_path(&root),
        root.join("transactions").join(VHDX_LEASE_FILE)
    );
    let invalid = root.join("invalid.json");
    std::fs::write(&invalid, b"not-json").unwrap();
    assert!(matches!(read_lease(&invalid), Err(VhdxError::Io(_))));
    let blocked = root.join("blocked");
    std::fs::write(&blocked, b"x").unwrap();
    assert!(matches!(
        write_lease_atomic(&blocked.join("lease.json"), &lease()),
        Err(VhdxError::Io(_))
    ));
    let _ = std::fs::remove_dir_all(&root);
}
