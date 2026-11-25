// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/vhd/tests.rs
use super::*;

#[test]
fn virtual_disk_identity_requires_correct_version_and_complete_nonzero_data() {
    let id = [7u8; 16];
    assert_eq!(extract_disk_identifier(14, 32, &id), Some(id));
    assert!(extract_disk_identifier(2, 32, &id).is_none());
    assert!(extract_disk_identifier(14, 23, &id).is_none());
    assert!(extract_disk_identifier(14, 41, &id).is_none());
    assert!(extract_disk_identifier(14, 32, &[0; 16]).is_none());
}
