// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/native_origin.rs
//! Bridge current admitted Linux volume inventory to shared origin custody.
//!
//! Stable UUID/backing-device and platform admission belong to linux_storage;
//! the block owner verifies the held mount and file descriptors.

use std::fs::File;

use ramshared_block::native_origin::OriginMount;
use ramshared_config::resource_profile::ResourceTarget;

use super::MountInfo;
pub(super) use ramshared_block::native_origin::NativeOriginVolume as OriginCustody;

pub(super) fn open(mount: &MountInfo, owner_uid: u32) -> Result<OriginCustody, String> {
    OriginCustody::open(
        &OriginMount {
            mountpoint: mount.mountpoint.clone(),
            mount_id: mount.mount_id,
            major_minor: mount.major_minor.clone(),
        },
        owner_uid,
    )
}

pub(super) fn verify(target: &ResourceTarget, mount: &MountInfo) -> Result<(), String> {
    open(mount, 0)?.verify(target).map(|_: File| ())
}
