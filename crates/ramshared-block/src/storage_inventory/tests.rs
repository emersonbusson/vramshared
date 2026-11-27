// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/storage_inventory/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

fn fixture() -> (Vec<BlockDevice>, Vec<MountInfo>) {
    let devices=parse_lsblk_json(r#"{"blockdevices":[{"name":"vda","path":"/dev/vda","type":"disk","uuid":"fs","wwn":"disk","maj:min":"8:0","ro":false,"rm":false,"tran":"virtio"}]}"#).unwrap();
    let mounts = parse_mountinfo("41 1 8:0 / /data rw - ext4 /dev/vda rw\n").unwrap();
    (devices, mounts)
}

#[test]
fn inventory_capacity_probes_only_qualified_mounts() {
    let (mut devices, mut mounts) = fixture();
    mounts.extend(parse_mountinfo("42 1 0:99 / /remote rw - nfs server:/data rw\n").unwrap());
    let mut calls = Vec::new();
    attach_with(&mut devices, mounts, 1000, |path| {
        calls.push(path.to_owned());
        Some((100, 80))
    });
    assert_eq!(calls, vec!["/data"]);
    assert!(devices[0].eligible_for_file_storage);
    for transport in ["nbd", "usb", "unknown"] {
        let (mut devices, mounts) = fixture();
        devices[0].transport = Some(transport.into());
        attach_with(&mut devices, mounts, 1000, |_| {
            panic!("unqualified backing must not be probed")
        });
        assert!(!devices[0].eligible_for_file_storage);
    }
}

#[test]
fn mount_capacity_uses_live_available_blocks_without_writing() {
    let (total, available) = mount_capacity_bytes("/").expect("root filesystem capacity");
    assert!(available > 0);
    assert!(total >= available);
}

#[test]
fn inventory_capacity_failure_revokes_samples_and_ambiguous_devices() {
    let (mut devices, mut mounts) = fixture();
    devices[0].eligible_for_file_storage = true;
    mounts[0].total_bytes = Some(100);
    mounts[0].available_bytes = Some(80);
    mounts[0].capacity_observed_unix_ms = Some(1);
    attach_with(&mut devices, mounts, 1000, |_| None);
    assert!(!devices[0].eligible_for_file_storage);
    assert_eq!(devices[0].mounts[0].capacity_observed_unix_ms, None);
    assert_eq!(devices[0].mounts[0].available_bytes, None);
    devices[0].eligible_for_file_storage = true;
    attach_with(&mut devices, vec![], 1000, |_| panic!("no mount to probe"));
    assert!(!devices[0].eligible_for_file_storage);

    let (mut devices, mounts) = fixture();
    devices.push(devices[0].clone());
    attach_with(&mut devices, mounts, 1000, |_| panic!("ambiguous backing"));
    assert!(
        devices
            .iter()
            .all(|d| d.mounts.is_empty() && !d.eligible_for_file_storage)
    );
    for (time, sample) in [
        (0, Some((100, 80))),
        (1000, Some((100, 101))),
        (1000, Some((0, 0))),
    ] {
        let (mut devices, mounts) = fixture();
        attach_with(&mut devices, mounts, time, |_| sample);
        assert!(!devices[0].eligible_for_file_storage);
        assert_eq!(devices[0].mounts[0].total_bytes, None);
    }
}

#[test]
fn inventory_bounds_and_identity_inheritance_are_fail_closed() {
    use ramshared_config::resource_profile::MAX_IDENTITY_BYTES;
    let text = r#"{"blockdevices":[{"name":"vda","path":"/dev/vda","type":"disk","serial":"disk-serial","ro":false,"rm":true,"tran":"USB","children":[{"name":"vda1","path":"/dev/vda1","type":"part","wwn":"child-id","ro":false,"rm":false}]}]}"#;
    let devices = parse_lsblk_json(text).unwrap();
    assert_eq!(
        devices[1].parent_hardware_identity.as_deref(),
        Some("serial:disk-serial")
    );
    assert_eq!(devices[1].transport.as_deref(), Some("USB"));
    assert_eq!(devices[1].removable, Some(true));
    for invalid in [
        text.replace("disk-serial", &"x".repeat(MAX_IDENTITY_BYTES + 1)),
        text.replace("\"serial\":\"disk-serial\"", "\"serial\":42"),
        r#"{"blockdevices":[{"name":"vda","path":"/dev/vda","type":"disk","children":{}}]}"#.into(),
        "x".repeat(LINUX_INVENTORY_OUTPUT_LIMIT + 1),
    ] {
        assert!(parse_lsblk_json(&invalid).is_err());
    }
    assert!(parse_mountinfo(&"x".repeat(LINUX_INVENTORY_OUTPUT_LIMIT + 1)).is_err());
    for invalid in [
        "41 1 8:0 / /data\\777 rw - ext4 /dev/vda rw\n",
        "41 1 8:0 / /data\\999 rw - ext4 /dev/vda rw\n",
        "41 1 8:0 / /data\\1 rw - ext4 /dev/vda rw\n",
    ] {
        assert!(parse_mountinfo(invalid).is_err());
    }
    let mounts = parse_mountinfo("41 1 8:0 / /data-é\\040disk rw - ext4 /dev/vda rw\n").unwrap();
    assert_eq!(mounts[0].mountpoint, "/data-é disk");
}

#[test]
fn inventory_serialized_shape_preserves_cli_records() {
    let (mut devices, mounts) = fixture();
    attach_with(&mut devices, mounts, 1000, |_| Some((100, 80)));
    let value = serde_json::to_value(&devices[0]).unwrap();
    let mut keys = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "eligibility_reason",
            "eligible_for_file_storage",
            "filesystem",
            "hardware_identity",
            "kind",
            "major_minor",
            "model",
            "mountpoints",
            "mounts",
            "name",
            "parent",
            "parent_hardware_identity",
            "partition_uuid",
            "path",
            "read_only",
            "removable",
            "rotational",
            "size_bytes",
            "transport",
            "uuid"
        ]
    );
    assert_eq!(value["hardware_identity"], "wwn:disk");
    assert_eq!(
        value["mounts"][0],
        serde_json::json!({"mount_id":41,"parent_mount_id":1,"major_minor":"8:0","root":"/","mountpoint":"/data","mount_options":["rw"],"filesystem":"ext4","source":"/dev/vda","super_options":["rw"],"read_only":false,"total_bytes":100,"available_bytes":80,"capacity_observed_unix_ms":1000,"host_backing":null})
    );
}

#[test]
fn hyperv_vmbus_scsi_disk_without_tran_qualifies_while_unknown_null_tran_refuses() {
    let json = r#"{"blockdevices":[
        {"name":"sdc","path":"/dev/sdc","type":"disk","size":4294967296,"fstype":"ext4","uuid":"818f7adf-11ac-476c-b390-5dafd250e56d","mountpoints":["/origin"],"maj:min":"8:32","wwn":"0x60022480ae6f672eb5a12a0030df5092","ro":false,"rm":false,"tran":null,"subsystems":"block:scsi:vmbus:acpi","model":"Virtual Disk"},
        {"name":"sdd","path":"/dev/sdd","type":"disk","size":4294967296,"fstype":"ext4","uuid":"11111111-1111-1111-1111-111111111111","mountpoints":["/mystery"],"maj:min":"8:48","wwn":"0x60022480111111111111111111111111","ro":false,"rm":false,"tran":null,"subsystems":"block:unknown","model":"Unknown"}
    ]}"#;
    let devices = parse_lsblk_json(json).unwrap();
    assert_eq!(devices[0].transport.as_deref(), Some("scsi"));
    assert_eq!(devices[1].transport, None);

    let ok_mount = MountInfo {
        mount_id: 50,
        parent_mount_id: 1,
        major_minor: "8:32".into(),
        root: "/".into(),
        mountpoint: "/origin".into(),
        mount_options: vec!["rw".into()],
        filesystem: "ext4".into(),
        source: "/dev/sdc".into(),
        super_options: vec!["rw".into()],
        read_only: false,
        total_bytes: Some(4_000_000_000),
        available_bytes: Some(3_000_000_000),
        capacity_observed_unix_ms: Some(1000),
        host_backing: None,
    };
    assert!(origin_mount_eligibility(&devices[0], &ok_mount).is_ok());

    let bad_mount = MountInfo {
        major_minor: "8:48".into(),
        mountpoint: "/mystery".into(),
        source: "/dev/sdd".into(),
        ..ok_mount
    };
    assert!(origin_mount_eligibility(&devices[1], &bad_mount).is_err());
}
