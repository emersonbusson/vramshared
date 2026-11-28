// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/native_origin/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    root: PathBuf,
    path: PathBuf,
    target: ResourceTarget,
    mount: OriginMount,
    uid: u32,
}

impl Fixture {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ramshared-origin-observer-{}-{stamp}",
            std::process::id()
        ));
        let path = root.join(MANAGED_ROOT).join("origin/test.img");
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path.parent().unwrap())
            .unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(&[17; 8192]).unwrap();
        file.sync_all().unwrap();
        let stat = descriptor_stat(&file).unwrap();
        let mut target = ResourceTarget::LinuxFileOrigin {
            filesystem_uuid: "fixture-fs".into(),
            device_identity: "fixture-device".into(),
            managed_relative_path: "origin/test.img".into(),
            inode: stat.stx_ino,
            allocated_bytes: stat.stx_size,
            identity_field_hash: String::new(),
        };
        seal(&mut target);
        let mount = OriginMount {
            mountpoint: root.to_str().unwrap().into(),
            mount_id: stat.stx_mnt_id,
            major_minor: format!("{}:{}", stat.stx_dev_major, stat.stx_dev_minor),
        };
        Self {
            root,
            path,
            target,
            mount,
            uid: stat.stx_uid,
        }
    }

    fn check(&self) -> Result<(), String> {
        NativeOriginVolume::open(&self.mount, self.uid)?
            .verify(&self.target)
            .map(|_| ())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn seal(target: &mut ResourceTarget) {
    let digest = identity_hash(target).unwrap();
    let ResourceTarget::LinuxFileOrigin {
        identity_field_hash,
        ..
    } = target
    else {
        unreachable!()
    };
    *identity_field_hash = digest;
}

#[test]
fn native_origin_observer_binds_descriptor_and_identity() {
    let fixture = Fixture::new();
    fixture
        .check()
        .expect("actual held file and mount identity");
    // The origin payload is mutable. Identity covers metadata, not bytes.
    let before = identity_hash(&fixture.target).unwrap();
    fs::write(&fixture.path, [29; 8192]).unwrap();
    fixture
        .check()
        .expect("same owned origin after payload writes");
    assert_eq!(identity_hash(&fixture.target).unwrap(), before);
    if fixture.uid != 0 {
        assert!(
            NativeOriginVolume::open(&fixture.mount, 0)
                .and_then(|v| v.verify(&fixture.target))
                .unwrap_err()
                .contains("root owned")
        );
    } else {
        NativeOriginVolume::open(&fixture.mount, 0)
            .and_then(|v| v.verify(&fixture.target))
            .unwrap();
    }
    assert_eq!(fs::metadata(&fixture.path).unwrap().len(), 8192);
}

#[test]
fn native_origin_observer_refuses_path_metadata_and_mount_drift() {
    let mut fixture = Fixture::new();
    let original = fixture.target.clone();
    for path in [
        "../outside",
        "origin/../outside",
        "origin//test.img",
        "swap/test.img",
        "origin/test\\img",
        "origin/test\0img",
    ] {
        if let ResourceTarget::LinuxFileOrigin {
            managed_relative_path,
            ..
        } = &mut fixture.target
        {
            *managed_relative_path = path.into();
        }
        seal(&mut fixture.target);
        assert!(
            fixture.check().unwrap_err().contains("confined"),
            "{path:?}"
        );
    }
    fixture.target = original.clone();
    fixture.mount.mount_id += 1;
    assert!(fixture.check().unwrap_err().contains("mount identity"));
    fixture.mount.mount_id -= 1;
    let device = fixture.mount.major_minor.clone();
    fixture.mount.major_minor = "999:999".into();
    assert!(fixture.check().unwrap_err().contains("mount identity"));
    fixture.mount.major_minor = device;
    if let ResourceTarget::LinuxFileOrigin { inode, .. } = &mut fixture.target {
        *inode += 1;
    }
    seal(&mut fixture.target);
    assert!(fixture.check().unwrap_err().contains("sealed identity"));
    fixture.target = original.clone();
    if let ResourceTarget::LinuxFileOrigin {
        allocated_bytes, ..
    } = &mut fixture.target
    {
        *allocated_bytes += 1;
    }
    seal(&mut fixture.target);
    assert!(fixture.check().unwrap_err().contains("sealed identity"));
    fixture.target = original;
    fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o640)).unwrap();
    assert!(fixture.check().unwrap_err().contains("sealed identity"));
    fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600)).unwrap();
    let alias = fixture.root.join("hardlink");
    fs::hard_link(&fixture.path, &alias).unwrap();
    assert!(fixture.check().unwrap_err().contains("sealed identity"));
    fs::remove_file(alias).unwrap();
    fixture.check().unwrap();
    let directory = fixture.path.parent().unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(fixture.check().unwrap_err().contains("root owned"));
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.check().unwrap();
}

#[test]
fn native_origin_observer_refuses_replacement_links_absence_and_sparse_files() {
    let mut fixture = Fixture::new();
    let old = OpenOptions::new().read(true).open(&fixture.path).unwrap();
    let original = fs::read(&fixture.path).unwrap();
    fs::remove_file(&fixture.path).unwrap();
    assert!(fixture.check().is_err());
    let mut replacement = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&fixture.path)
        .unwrap();
    replacement.write_all(&original).unwrap();
    assert_ne!(
        replacement.metadata().unwrap().ino(),
        old.metadata().unwrap().ino()
    );
    assert!(fixture.check().is_err());
    assert_eq!(fs::read(&fixture.path).unwrap(), original);
    fs::remove_file(&fixture.path).unwrap();
    symlink("/dev/null", &fixture.path).unwrap();
    assert!(fixture.check().is_err());
    fs::remove_file(&fixture.path).unwrap();
    let sparse = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&fixture.path)
        .unwrap();
    sparse.set_len(8192).unwrap();
    if let ResourceTarget::LinuxFileOrigin { inode, .. } = &mut fixture.target {
        *inode = sparse.metadata().unwrap().ino();
    }
    seal(&mut fixture.target);
    assert!(fixture.check().unwrap_err().contains("allocation"));
    fs::remove_file(&fixture.path).unwrap();
    fs::create_dir(&fixture.path).unwrap();
    assert!(fixture.check().is_err());
}

#[test]
fn native_origin_identity_hash_binds_each_field_and_unambiguous_boundaries() {
    let mut fixture = Fixture::new();
    let original = fixture.target.clone();
    let expected = identity_hash(&original).unwrap();
    for field in 0..5 {
        let mut changed = original.clone();
        if let ResourceTarget::LinuxFileOrigin {
            filesystem_uuid,
            device_identity,
            managed_relative_path,
            inode,
            allocated_bytes,
            ..
        } = &mut changed
        {
            match field {
                0 => filesystem_uuid.push('x'),
                1 => device_identity.push('x'),
                2 => managed_relative_path.push('x'),
                3 => *inode += 1,
                _ => *allocated_bytes += 1,
            }
        }
        assert_ne!(identity_hash(&changed).unwrap(), expected);
        assert!(
            NativeOriginVolume::open(&fixture.mount, fixture.uid)
                .and_then(|v| v.verify(&changed))
                .unwrap_err()
                .contains("digest")
        );
    }
    if let ResourceTarget::LinuxFileOrigin {
        filesystem_uuid,
        device_identity,
        ..
    } = &mut fixture.target
    {
        *filesystem_uuid = "left|middle".into();
        *device_identity = "right".into();
    }
    let left = identity_hash(&fixture.target).unwrap();
    if let ResourceTarget::LinuxFileOrigin {
        filesystem_uuid,
        device_identity,
        ..
    } = &mut fixture.target
    {
        *filesystem_uuid = "left".into();
        *device_identity = "middle|right".into();
    }
    assert_ne!(identity_hash(&fixture.target).unwrap(), left);
    let request = ResourceTarget::LinuxFileOriginRequest {
        filesystem_uuid: "fs".into(),
        device_identity: "device".into(),
        managed_relative_path: "origin/new".into(),
        allocated_bytes: 8192,
    };
    assert!(identity_hash(&request).is_err());
    assert!(
        NativeOriginVolume::open(&fixture.mount, fixture.uid)
            .and_then(|v| v.verify(&request))
            .is_err()
    );
}
#[test]
fn native_origin_capacity_observation_is_bound_to_held_mount() {
    let fixture = Fixture::new();
    let custody = NativeOriginVolume::open(&fixture.mount, fixture.uid).unwrap();
    let free = custody.available_bytes().unwrap();
    let stat = rustix::fs::fstatvfs(&custody.root).unwrap();
    assert!(free <= stat.f_blocks * stat.f_frsize);
    let moved = fixture.root.with_extension("moved");
    fs::rename(&fixture.root, &moved).unwrap();
    fs::create_dir(&fixture.root).unwrap();
    assert!(custody.available_bytes().is_err());
    assert!(moved.join(MANAGED_ROOT).join("origin/test.img").exists());
    fs::remove_dir_all(&moved).unwrap();
}

#[test]
fn native_origin_mount_open_refuses_links_and_ambiguous_paths() {
    use std::os::fd::AsRawFd;

    let fixture = Fixture::new();
    let custody = NativeOriginVolume::open(&fixture.mount, fixture.uid).unwrap();
    custody.verify(&fixture.target).unwrap();
    let alias = fixture.root.join("alias");
    symlink(&fixture.root, &alias).unwrap();
    for path in [
        String::new(),
        ".".into(),
        fixture.root.join("unused/..").to_str().unwrap().into(),
        alias.to_str().unwrap().into(),
        alias.join(MANAGED_ROOT).to_str().unwrap().into(),
        format!("/proc/self/fd/{}", custody.root.as_raw_fd()),
    ] {
        let mount = OriginMount {
            mountpoint: path,
            ..fixture.mount.clone()
        };
        assert!(
            NativeOriginVolume::open(&mount, fixture.uid).is_err(),
            "{mount:?}"
        );
    }
    assert_eq!(fs::read(&fixture.path).unwrap(), [17; 8192]);
    custody.verify(&fixture.target).unwrap();
}

#[test]
fn native_origin_created_descriptor_serves_io_without_reopening_path() {
    use crate::{AuthoritativeOriginBackend, DisabledCache, FileOrigin, WriteOptions};

    let fixture = Fixture::new();
    let volume = NativeOriginVolume::open(&fixture.mount, fixture.uid).unwrap();
    let request = ResourceTarget::LinuxFileOriginRequest {
        filesystem_uuid: "fixture-fs".into(),
        device_identity: "fixture-device".into(),
        managed_relative_path: "origin/new.img".into(),
        allocated_bytes: 8192,
    };
    volume.require_absent("origin/new.img").unwrap();
    let (file, manifest) = volume
        .create_sealed(&request, |target| {
            ramshared_config::native_origin::NativeOriginManifest::new(
                "a".repeat(64),
                "test-approval".into(),
                "b".repeat(64),
                "test-release".into(),
                target,
            )
        })
        .unwrap();
    let target = manifest.target;
    let inode = file.metadata().unwrap().ino();
    let backend =
        AuthoritativeOriginBackend::new(FileOrigin::from_file(file), DisabledCache, 8192, 4096)
            .unwrap();
    let mut read = [1; 4096];
    backend.read_at(0, &mut read).unwrap();
    assert_eq!(read, [0; 4096]);
    backend
        .write_at_with_options(0, &[31; 4096], WriteOptions { fua: true })
        .unwrap();
    let path = volume.reviewed_path("origin/new.img").unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    volume.verify(&target).unwrap();

    let retained = fixture.root.join("retained.img");
    fs::rename(&path, &retained).unwrap();
    let mut replacement = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    replacement.write_all(&[97; 8192]).unwrap();
    assert!(volume.verify(&target).is_err());
    backend.read_at(0, &mut read).unwrap();
    assert_eq!(read, [31; 4096]);
    backend
        .write_at_with_options(4096, &[53; 4096], WriteOptions { fua: true })
        .unwrap();
    backend.flush().unwrap();
    assert!(backend.write_at(8192, &[83; 4096]).is_err());
    assert_eq!(fs::metadata(&retained).unwrap().ino(), inode);
    assert_eq!(
        fs::read(&retained).unwrap(),
        [[31; 4096], [53; 4096]].concat()
    );
    assert_eq!(fs::metadata(&retained).unwrap().len(), 8192);
    assert_eq!(fs::read(&path).unwrap(), [97; 8192]);
}

fn creator(
    target: ResourceTarget,
) -> Result<ramshared_config::native_origin::NativeOriginManifest, String> {
    ramshared_config::native_origin::NativeOriginManifest::new(
        "a".repeat(64),
        "fixture-approval".into(),
        "b".repeat(64),
        "fixture-release".into(),
        target,
    )
}

fn creation_request() -> ResourceTarget {
    ResourceTarget::LinuxFileOriginRequest {
        filesystem_uuid: "fixture-fs".into(),
        device_identity: "fixture-device".into(),
        managed_relative_path: "origin/sealed.img".into(),
        allocated_bytes: 8192,
    }
}

#[test]
fn native_creation_proof_precedes_named_publication() {
    let fixture = Fixture::new();
    let volume = NativeOriginVolume::open(&fixture.mount, fixture.uid).unwrap();
    let request = creation_request();
    let path = volume.reviewed_path("origin/sealed.img").unwrap();
    // Closure failure occurs before naming and releases only anonymous storage.
    assert!(
        volume
            .create_sealed(&request, |_| Err("creator unavailable".into()))
            .is_err()
    );
    assert!(!path.exists());
    let (file, manifest) = volume
        .create_sealed(&request, |target| {
            assert!(
                !path.exists(),
                "creator proof must precede named publication"
            );
            creator(target)
        })
        .unwrap();
    let inode = file.metadata().unwrap().ino();
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert!(
        volume.recover_creation(&request).is_err(),
        "creator retains exclusive lease"
    );
    drop(file);
    let before = fs::read(&path).unwrap();
    let (file, recovered) = volume.recover_creation(&request).unwrap();
    assert_eq!(recovered, manifest);
    assert_eq!(file.metadata().unwrap().ino(), inode);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(volume.create_sealed(&request, creator).is_err());
    assert!(volume.create_sealed(&manifest.target, creator).is_err());
    assert!(volume.recover_creation(&manifest.target).is_err());
}

#[test]
fn native_creation_proof_rejects_foreign_or_changed_origins() {
    use rustix::fs::{XattrFlags, fsetxattr};
    let fixture = Fixture::new();
    let volume = NativeOriginVolume::open(&fixture.mount, fixture.uid).unwrap();
    let request = creation_request();
    let path = volume.reviewed_path("origin/sealed.img").unwrap();
    assert!(
        volume
            .create_sealed(&request, |target| {
                let mut foreign = target;
                if let ResourceTarget::LinuxFileOrigin { inode, .. } = &mut foreign {
                    *inode += 1;
                }
                seal(&mut foreign);
                creator(foreign)
            })
            .is_err()
    );
    assert!(!path.exists());
    // An existing same-sized private file is not creation proof.
    let foreign = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    foreign.set_len(8192).unwrap();
    rustix::fs::fallocate(&foreign, rustix::fs::FallocateFlags::empty(), 0, 8192).unwrap();
    assert!(volume.recover_creation(&request).is_err());
    drop(foreign);
    fs::remove_file(&path).unwrap();
    let (file, manifest) = volume.create_sealed(&request, creator).unwrap();
    drop(file);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let original = manifest.encode().unwrap();
    for bytes in [b"broken = [".as_slice(), &[0xff]] {
        fsetxattr(&file, "user.ramshared.creation", bytes, XattrFlags::REPLACE).unwrap();
        assert!(volume.recover_creation(&request).is_err());
    }
    let mut changed = manifest.clone();
    if let ResourceTarget::LinuxFileOrigin { inode, .. } = &mut changed.target {
        *inode += 1;
    }
    seal(&mut changed.target);
    fsetxattr(
        &file,
        "user.ramshared.creation",
        changed.encode().unwrap().as_bytes(),
        XattrFlags::REPLACE,
    )
    .unwrap();
    assert!(volume.recover_creation(&request).is_err());
    fsetxattr(
        &file,
        "user.ramshared.creation",
        original.as_bytes(),
        XattrFlags::REPLACE,
    )
    .unwrap();
    let mut wrong = request.clone();
    if let ResourceTarget::LinuxFileOriginRequest {
        device_identity, ..
    } = &mut wrong
    {
        device_identity.push_str("-foreign");
    }
    assert!(volume.recover_creation(&wrong).is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), 8192);
    let (_, recovered) = volume.recover_creation(&request).unwrap();
    assert_eq!(recovered, manifest);
}

#[test]
fn native_origin_proc_fd_publication_links_held_inode_and_refuses_existing_or_symlink() {
    use rustix::fs::{Mode, OFlags, openat};
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let volume = NativeOriginVolume::open(&fixture.mount, fixture.uid).unwrap();
    let parent_path = fixture.path.parent().unwrap();
    let parent = OpenOptions::new().read(true).open(parent_path).unwrap();

    // Pre-existing regular file and symlink targets must both fail with EEXIST
    // without overwriting or modifying the existing entry.
    let existing_regular = parent_path.join("existing-regular.img");
    fs::write(&existing_regular, b"keep-regular").unwrap();
    let existing_symlink = parent_path.join("existing-symlink.img");
    symlink("/dev/null", &existing_symlink).unwrap();

    let anon = File::from(
        openat(
            &parent,
            ".",
            OFlags::RDWR | OFlags::TMPFILE | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap(),
    );
    let anon_ino = descriptor_stat(&anon).unwrap().stx_ino;
    assert_eq!(
        publication::link_anonymous_via_proc_fd(
            &anon,
            &parent,
            std::ffi::OsStr::new("existing-regular.img")
        ),
        Err(rustix::io::Errno::EXIST)
    );
    assert_eq!(fs::read(&existing_regular).unwrap(), b"keep-regular");
    assert_eq!(
        publication::link_anonymous_via_proc_fd(
            &anon,
            &parent,
            std::ffi::OsStr::new("existing-symlink.img")
        ),
        Err(rustix::io::Errno::EXIST)
    );
    assert!(
        fs::symlink_metadata(&existing_symlink)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // Linking to an absent leaf binds the exact held anonymous inode.
    let mut fallback_used = false;
    publication::link_anonymous_with_fallback(Err(rustix::io::Errno::NOENT), &mut || {
        fallback_used = true;
        publication::link_anonymous_via_proc_fd(
            &anon,
            &parent,
            std::ffi::OsStr::new("published-via-proc-fd.img"),
        )
    })
    .unwrap();
    assert!(fallback_used);
    assert_eq!(
        publication::link_anonymous_with_fallback(Err(rustix::io::Errno::EXIST), &mut || {
            panic!("non-ENOENT primary error must not invoke /proc/self/fd fallback")
        }),
        Err(rustix::io::Errno::EXIST)
    );
    let published = parent_path.join("published-via-proc-fd.img");
    assert_eq!(fs::metadata(&published).unwrap().ino(), anon_ino);

    // High-level create_sealed also refuses pre-existing regular files and symlinks.
    for leaf in ["origin/existing-regular.img", "origin/existing-symlink.img"] {
        let req = ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid: "fixture-fs".into(),
            device_identity: "fixture-device".into(),
            managed_relative_path: leaf.into(),
            allocated_bytes: 8192,
        };
        assert!(volume.create_sealed(&req, creator).is_err());
    }
    assert_eq!(fs::read(&existing_regular).unwrap(), b"keep-regular");
    assert!(
        fs::symlink_metadata(&existing_symlink)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // Concurrent race during creator callback: a file appearing at the final leaf
    // between require_absent() and link_anonymous_inode() must fail EEXIST without
    // overwriting the racing file.
    let race_path = volume.reviewed_path("origin/raced-leaf.img").unwrap();
    let race_req = ResourceTarget::LinuxFileOriginRequest {
        filesystem_uuid: "fixture-fs".into(),
        device_identity: "fixture-device".into(),
        managed_relative_path: "origin/raced-leaf.img".into(),
        allocated_bytes: 8192,
    };
    let race_err = volume
        .create_sealed(&race_req, |target| {
            fs::write(&race_path, b"raced-content").unwrap();
            creator(target)
        })
        .unwrap_err();
    assert!(race_err.contains("publish sealed native origin exclusively"));
    assert_eq!(fs::read(&race_path).unwrap(), b"raced-content");

    // verify_creation rejects non-file targets and mutated xattr proofs.
    let clean_req = ResourceTarget::LinuxFileOriginRequest {
        filesystem_uuid: "fixture-fs".into(),
        device_identity: "fixture-device".into(),
        managed_relative_path: "origin/verify-check.img".into(),
        allocated_bytes: 8192,
    };
    let (sealed_file, mut sealed_manifest) = volume.create_sealed(&clean_req, creator).unwrap();
    let mut bad_target_manifest = sealed_manifest.clone();
    bad_target_manifest.target = clean_req;
    assert!(
        volume
            .verify_creation(&sealed_file, &bad_target_manifest)
            .is_err()
    );
    sealed_manifest.approval_id = "mutated-label".into();
    assert!(
        volume
            .verify_creation(&sealed_file, &sealed_manifest)
            .unwrap_err()
            .contains("changed before publication")
    );
}
