// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/critical_devices.rs
//! Admission policy for the origin's critical-device inventory.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use super::parse_strict_proc_swaps;

pub(super) fn linux_device_number(dev: u64) -> String {
    let major = ((dev & 0x0000_0000_000f_ff00) >> 8) | ((dev & 0xffff_f000_0000_0000) >> 32);
    let minor = (dev & 0xff) | ((dev & 0x0000_0fff_fff0_0000) >> 12);
    format!("{major}:{minor}")
}

pub(super) fn validate_stable_critical_device_snapshot(
    before: &[String],
    after: &[String],
) -> Result<(), String> {
    let mut before = before.to_vec();
    let mut after = after.to_vec();
    before.sort();
    before.dedup();
    after.sort();
    after.dedup();
    if before != after {
        return Err("root/swap critical-device identities changed during origin admission".into());
    }
    Ok(())
}

pub(super) fn root_mount_source(mountinfo: &str) -> Option<&str> {
    mountinfo.lines().find_map(|line| {
        let (mount, filesystem) = line.split_once(" - ")?;
        (mount.split_whitespace().nth(4)? == "/")
            .then(|| filesystem.split_whitespace().nth(1))
            .flatten()
            .filter(|source| source.starts_with("/dev/"))
    })
}

pub(super) fn collect_critical_device_numbers() -> Result<Vec<String>, String> {
    collect_critical_device_numbers_at(
        Path::new("/"),
        Path::new("/proc/self/mountinfo"),
        Path::new("/proc/swaps"),
        Path::new("/sys/dev/block"),
        Path::new("/dev"),
    )
}

pub(super) fn collect_critical_device_numbers_at(
    root_path: &Path,
    mountinfo_path: &Path,
    swaps_path: &Path,
    sys_block_root: &Path,
    dev_root: &Path,
) -> Result<Vec<String>, String> {
    let mut devices = Vec::new();
    let root = std::fs::metadata(root_path).map_err(|error| error.to_string())?;
    let root_dev_t = linux_device_number(root.dev());
    devices.push(root_dev_t.clone());
    collect_parent_for_device_number_at(&root_dev_t, &mut devices, sys_block_root, dev_root)?;
    if let Ok(mountinfo) = std::fs::read_to_string(mountinfo_path)
        && let Some(source) = root_mount_source(&mountinfo)
    {
        collect_device_and_parent_at(Path::new(source), &mut devices, sys_block_root, dev_root)?;
    }
    let swaps = std::fs::read_to_string(swaps_path).map_err(|error| error.to_string())?;
    for entry in parse_strict_proc_swaps(&swaps)? {
        collect_device_and_parent_at(
            Path::new(&entry.filename),
            &mut devices,
            sys_block_root,
            dev_root,
        )?;
    }
    devices.sort();
    devices.dedup();
    Ok(devices)
}

#[cfg(test)]
pub(super) fn collect_device_and_parent(
    path: &Path,
    devices: &mut Vec<String>,
) -> Result<(), String> {
    collect_device_and_parent_at(
        path,
        devices,
        Path::new("/sys/dev/block"),
        Path::new("/dev"),
    )
}

pub(super) fn collect_device_and_parent_at(
    path: &Path,
    devices: &mut Vec<String>,
    sys_block_root: &Path,
    dev_root: &Path,
) -> Result<(), String> {
    collect_device_and_parent_with(
        path,
        devices,
        |path| {
            let metadata = std::fs::metadata(path).map_err(|error| {
                format!(
                    "cannot identify critical device {}: {error}",
                    path.display()
                )
            })?;
            if metadata.file_type().is_block_device() {
                Ok(linux_device_number(metadata.rdev()))
            } else if metadata.file_type().is_file() {
                Ok(linux_device_number(metadata.dev()))
            } else {
                Err(format!(
                    "critical device {} is neither a block device nor a regular swap file",
                    path.display()
                ))
            }
        },
        |dev_t| {
            let mut parent = Vec::new();
            collect_parent_for_device_number_at(dev_t, &mut parent, sys_block_root, dev_root)?;
            Ok(parent.into_iter().next())
        },
    )
}

fn collect_parent_for_device_number_at(
    dev_t: &str,
    devices: &mut Vec<String>,
    sys_block_root: &Path,
    dev_root: &Path,
) -> Result<(), String> {
    let sysfs = std::fs::canonicalize(sys_block_root.join(dev_t)).map_err(|error| {
        format!("cannot resolve critical device {dev_t} through sysfs: {error}")
    })?;
    if !partition_marker_present(
        std::fs::symlink_metadata(sysfs.join("partition")).map(|metadata| metadata.is_file()),
    )? {
        return Ok(());
    }
    let parent_name = sysfs
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("critical device {dev_t} has no parent identity"))?;
    let parent = dev_root.join(parent_name);
    let metadata = std::fs::metadata(&parent)
        .map_err(|error| format!("cannot identify parent of critical device {dev_t}: {error}"))?;
    if !metadata.file_type().is_block_device() {
        return Err(format!(
            "parent of critical device {dev_t} is not a block device"
        ));
    }
    devices.push(linux_device_number(metadata.rdev()));
    Ok(())
}

/// Unknown parent identity cannot be recorded as an absent parent. Publish
/// neither identity until both observations have succeeded.
pub(super) fn collect_device_and_parent_with(
    path: &Path,
    devices: &mut Vec<String>,
    identify: impl FnOnce(&Path) -> Result<String, String>,
    parent: impl FnOnce(&str) -> Result<Option<String>, String>,
) -> Result<(), String> {
    let device = identify(path)?;
    let parent = parent(&device)?;
    devices.push(device);
    devices.extend(parent);
    Ok(())
}

/// Sysfs exposes a regular partition attribute for partitions and omits it
/// for whole disks. Errors and unexpected entry types are not absence.
pub(super) fn partition_marker_present(observation: std::io::Result<bool>) -> Result<bool, String> {
    match observation {
        Ok(true) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Ok(false) => Err("critical-device partition marker is not a regular file".into()),
        Err(error) => Err(format!(
            "cannot observe critical-device partition marker: {error}"
        )),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn critical_inventory_keeps_whole_disk_and_partition_identities() {
        for parent in [None, Some("8:16".to_string())] {
            let mut devices = vec!["8:1".into()];
            let expected = parent.clone();
            let result = collect_device_and_parent_with(
                Path::new("/dev/fixture"),
                &mut devices,
                |path| {
                    assert_eq!(path, Path::new("/dev/fixture"));
                    Ok("8:17".into())
                },
                |device| {
                    assert_eq!(device, "8:17");
                    Ok(parent)
                },
            );
            assert!(result.is_ok());
            let mut wanted = vec!["8:1".to_string(), "8:17".to_string()];
            wanted.extend(expected);
            assert_eq!(devices, wanted);
        }
    }

    #[test]
    fn critical_inventory_failure_does_not_publish_partial_identity() {
        let mut devices = vec!["8:1".into()];
        let result = collect_device_and_parent_with(
            Path::new("/missing"),
            &mut devices,
            |_| Err("device observation failed".into()),
            |_| panic!("unknown device must not query a parent"),
        );
        assert_eq!(result, Err("device observation failed".into()));
        assert_eq!(devices, ["8:1"]);
        let result = collect_device_and_parent_with(
            Path::new("/dev/fixture"),
            &mut devices,
            |_| Ok("8:17".into()),
            |_| Err("parent observation failed".into()),
        );
        assert_eq!(result, Err("parent observation failed".into()));
        assert_eq!(devices, ["8:1"]);
    }

    #[test]
    fn critical_partition_marker_distinguishes_missing_from_unknown() {
        assert_eq!(partition_marker_present(Ok(true)), Ok(true));
        assert_eq!(
            partition_marker_present(Err(Error::from(ErrorKind::NotFound))),
            Ok(false)
        );
        assert!(partition_marker_present(Ok(false)).is_err());
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidData,
            ErrorKind::Other,
        ] {
            assert!(partition_marker_present(Err(Error::from(kind))).is_err());
        }
    }

    #[test]
    fn critical_device_snapshot_and_inventory_at_collect_or_refuse() {
        validate_stable_critical_device_snapshot(
            &["8:1".into(), "8:0".into(), "8:1".into()],
            &["8:0".into(), "8:1".into()],
        )
        .expect("reordered duplicate sets still represent same critical devices");
        assert!(
            validate_stable_critical_device_snapshot(
                &["8:1".into()],
                &["8:1".into(), "8:2".into()]
            )
            .is_err()
        );

        let mountinfo = "29 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
                         30 29 0:25 / /proc rw - proc proc rw\n";
        assert_eq!(root_mount_source(mountinfo), Some("/dev/sda1"));
        assert_eq!(root_mount_source("29 1 8:1 / / rw - tmpfs none rw\n"), None);

        let root =
            std::env::temp_dir().join(format!("ramshared-critical-devices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let root_file = root.join("root-target");
        let swap_file = root.join("swapfile");
        let sys_block = root.join("sys/dev/block");
        let dev_dir = root.join("dev");
        std::fs::create_dir_all(&sys_block).expect("create fake sysfs");
        std::fs::create_dir_all(&dev_dir).expect("create fake dev");
        std::fs::write(&root_file, b"root").expect("write root target");
        std::fs::write(&swap_file, b"swap").expect("write swapfile");

        let root_dev = linux_device_number(std::fs::metadata(&root_file).expect("stat root").dev());
        std::fs::create_dir_all(sys_block.join(&root_dev)).expect("create whole-disk sysfs node");
        let mountinfo_path = root.join("mountinfo");
        std::fs::write(&mountinfo_path, "29 1 0:1 / / rw - tmpfs none rw\n")
            .expect("write mountinfo");
        let swaps_path = root.join("swaps");
        std::fs::write(
            &swaps_path,
            format!(
                "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n{}\t\t\t\tfile\t\t1024\t\t0\t\t-2\n",
                swap_file.display()
            ),
        )
        .expect("write swaps");

        let collected = collect_critical_device_numbers_at(
            &root_file,
            &mountinfo_path,
            &swaps_path,
            &sys_block,
            &dev_dir,
        )
        .expect("whole-disk + regular swapfile collection succeeds");
        assert_eq!(collected, vec![root_dev.clone()]);

        // Partition marker pointing to a non-block parent under fake /dev must refuse.
        let disk_dir = root.join("sys/block/sda");
        let part_dir = disk_dir.join("sda1");
        std::fs::create_dir_all(&part_dir).expect("create fake partition sysfs");
        std::fs::write(part_dir.join("partition"), b"1").expect("write partition marker");
        std::os::unix::fs::symlink(&part_dir, sys_block.join("8:1")).expect("symlink partition");
        let disk_dev = linux_device_number(std::fs::metadata(&root_file).expect("stat").dev());
        let _ = std::fs::remove_dir_all(sys_block.join(&disk_dev));
        std::os::unix::fs::symlink(&part_dir, sys_block.join(&disk_dev)).expect("symlink root_dev");
        std::fs::write(dev_dir.join("sda"), b"not-a-block").expect("create regular /dev/sda");
        let err = collect_critical_device_numbers_at(
            &root_file,
            &mountinfo_path,
            &swaps_path,
            &sys_block,
            &dev_dir,
        )
        .expect_err("non-block parent device must refuse");
        assert!(err.contains("not a block device"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
