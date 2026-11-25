// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/native_fixture.rs
//! Real filesystem custody with explicitly synthetic native hardware inventory.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_block::native_origin::{NativeOriginVolume, OriginMount};
use ramshared_block::storage_inventory::{BlockDevice, parse_lsblk_json, parse_mountinfo};
use ramshared_config::native_origin::NativeOriginManifest;
use ramshared_config::resource_profile::ResourceTarget;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
pub(super) struct Fixture {
    pub(super) root: PathBuf,
    pub(super) manifest: NativeOriginManifest,
    pub(super) devices: Vec<BlockDevice>,
    pub(super) uid: u32,
}
impl Fixture {
    pub(super) fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ramshared-native-daemon-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let dir = File::open(&root).unwrap();
        let stat = rustix::fs::statx(
            &dir,
            "",
            rustix::fs::AtFlags::EMPTY_PATH,
            rustix::fs::StatxFlags::BASIC_STATS | rustix::fs::StatxFlags::MNT_ID,
        )
        .unwrap();
        let number = format!("{}:{}", stat.stx_dev_major, stat.stx_dev_minor);
        let volume = NativeOriginVolume::open(
            &OriginMount {
                mountpoint: root.to_str().unwrap().into(),
                mount_id: stat.stx_mnt_id,
                major_minor: number.clone(),
            },
            stat.stx_uid,
        )
        .unwrap();
        let request = ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid: "fixture-fs".into(),
            device_identity: "wwn:fixture-disk".into(),
            managed_relative_path: "origin/live.img".into(),
            allocated_bytes: 8192,
        };
        let (mut file, manifest) = volume
            .create_sealed(&request, |target| {
                NativeOriginManifest::new(
                    "a".repeat(64),
                    "native-fixture".into(),
                    "b".repeat(64),
                    "v0.15.0".into(),
                    target,
                )
            })
            .unwrap();
        file.write_all(&[17; 8192]).unwrap();
        file.sync_all().unwrap();
        let raw=serde_json::json!({"blockdevices":[{"name":"fixture","path":"/dev/fixture","type":"disk","wwn":"fixture-disk","uuid":"fixture-fs","maj:min":number,"ro":false,"rm":false,"tran":"virtio"}]}).to_string();
        let mut devices = parse_lsblk_json(&raw).unwrap();
        devices[0].mounts = parse_mountinfo(&format!(
            "{} 1 {} / {} rw - ext4 /dev/fixture rw\n",
            stat.stx_mnt_id,
            number,
            root.display()
        ))
        .unwrap();
        Self {
            root,
            manifest,
            devices,
            uid: stat.stx_uid,
        }
    }
    pub(super) fn origin_path(&self) -> PathBuf {
        self.root.join("var/lib/ramshared/origin/live.img")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
