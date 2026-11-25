// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/native_cache/worker/tests.rs
//! Actual inherited descriptors/process exit with synthetic configuration records.
#![allow(clippy::unwrap_used)]
use super::*;
use crate::native_cache::tests::{LeaseFixture, policy};
use crate::native_fixture::Fixture;
use std::io::{Read, Write};
use std::process::Stdio;

#[test]
fn configured_worker_lease_survives_parent_and_blocks_replacement() {
    let fixture = LeaseFixture::new();
    let held = ramshared_exec::inherited_lease::acquire(&fixture.path(), fixture.uid()).unwrap();
    let mut command = Command::new("/usr/bin/python3");
    command.args(["-c","import os,sys,fcntl; n=int(sys.argv[-3]); fcntl.flock(n,fcntl.LOCK_EX|fcntl.LOCK_NB); os.write(1,b'R'); sys.stdin.buffer.read(1)"]);
    command.stdin(Stdio::piped()).stdout(Stdio::piped());
    inherit_file(&mut command, &held, 7).unwrap();
    let child = command.spawn().unwrap();
    drop(command);
    struct Cleanup(Option<std::process::Child>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                drop(child.stdin.take());
                let _ = child.wait();
            }
        }
    }
    let mut cleanup = Cleanup(Some(child));
    let mut ready = [0];
    cleanup
        .0
        .as_mut()
        .unwrap()
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(ready, [b'R']);
    drop(held);
    assert!(ramshared_exec::inherited_lease::acquire(&fixture.path(), fixture.uid()).is_err());
    cleanup
        .0
        .as_mut()
        .unwrap()
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"x")
        .unwrap();
    assert!(cleanup.0.take().unwrap().wait().unwrap().success());
    assert!(ramshared_exec::inherited_lease::acquire(&fixture.path(), fixture.uid()).is_ok());
}

#[test]
fn configured_worker_refuses_foreign_generation_before_driver_discovery() {
    let origin = Fixture::new();
    let fixture = LeaseFixture::new();
    let config = policy(&origin.manifest);
    let held = ramshared_exec::inherited_lease::acquire(&fixture.path(), fixture.uid()).unwrap();
    let member = &config.members[0];
    let mut request = WorkerRequest {
        fd: 0,
        generation: Some((held.as_raw_fd(), 7)),
        expected_adapter: Some(member.adapter.clone()),
        config: ramshared_block::GpuWorkerConfig {
            target_bytes: member.target_bytes,
            reserve_floor_bytes: member.reserve_bytes,
            ..Default::default()
        },
        catalog_fd: None,
    };
    let admitted = admit_with(
        held.as_raw_fd(),
        7,
        &request,
        &fixture.path(),
        fixture.uid(),
        || Ok(config.clone()),
    )
    .unwrap();
    assert!(ramshared_exec::inherited_lease::acquire(&fixture.path(), fixture.uid()).is_err());
    drop(admitted);
    assert!(
        admit_with(
            held.as_raw_fd(),
            8,
            &request,
            &fixture.path(),
            fixture.uid(),
            || Ok(config.clone())
        )
        .is_err()
    );
    request.config.target_bytes += 1;
    assert!(
        admit_with(
            held.as_raw_fd(),
            7,
            &request,
            &fixture.path(),
            fixture.uid(),
            || Ok(config.clone())
        )
        .is_err()
    );
    request.config.target_bytes = member.target_bytes;
    request.config.reserve_floor_bytes -= 1;
    assert!(
        admit_with(
            held.as_raw_fd(),
            7,
            &request,
            &fixture.path(),
            fixture.uid(),
            || Ok(config.clone())
        )
        .is_err()
    );
    request.expected_adapter = None;
    assert!(
        admit_with(
            held.as_raw_fd(),
            7,
            &request,
            &fixture.path(),
            fixture.uid(),
            || Ok(config.clone())
        )
        .is_err()
    );
    assert!(
        admit_with(-1, 7, &request, &fixture.path(), fixture.uid(), || panic!(
            "bad descriptor precedes record read"
        ))
        .is_err()
    );
    request.generation = None;
    assert!(admit(&request).unwrap().is_none());
    let retired = crate::native_cache::tests::synthetic_session(config);
    retired.revoke();
    assert!(inherit(&mut Command::new("/bin/false"), &retired).is_err());
}

#[test]
fn inherit_and_admit_scope_enforce_lease_and_origin_custody() {
    use ramshared_block::platform::PlatformKind;
    use std::os::unix::fs::PermissionsExt;

    let origin = Fixture::new();
    let fixture = LeaseFixture::new();
    let config = policy(&origin.manifest);
    let member = &config.members[0];

    let empty_session =
        NativeCacheSession::open_with(&fixture.path(), fixture.uid(), || Ok(None)).unwrap();
    let mut cmd = Command::new("/bin/false");
    assert!(
        inherit_with(&mut cmd, &empty_session, &fixture.path(), fixture.uid())
            .unwrap_err()
            .to_string()
            .contains("absent")
    );
    drop(empty_session);

    let active_config = config.clone();
    let active_session = NativeCacheSession::open_with(&fixture.path(), fixture.uid(), move || {
        Ok(Some(active_config))
    })
    .unwrap();
    assert!(
        inherit_with(
            &mut Command::new("/bin/false"),
            &active_session,
            &fixture.path().with_extension("other"),
            fixture.uid()
        )
        .is_err()
    );
    let mut bound = Command::new("/bin/false");
    inherit_with(&mut bound, &active_session, &fixture.path(), fixture.uid()).unwrap();
    let args = bound
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(args.iter().any(|arg| arg == "--configuration-fd"));
    assert!(args.iter().any(|arg| arg == "--configuration-generation"));
    drop(active_session);
    assert!(ramshared_exec::inherited_lease::acquire(&fixture.path(), fixture.uid()).is_err());
    drop(bound);

    let held = ramshared_exec::inherited_lease::acquire(&fixture.path(), fixture.uid()).unwrap();
    let probe = duplicate(held.as_raw_fd()).unwrap();
    clear_cloexec(probe.as_raw_fd()).unwrap();
    drop(probe);
    assert!(clear_cloexec(-1).is_err());

    let request = WorkerRequest {
        fd: 0,
        generation: Some((held.as_raw_fd(), 7)),
        expected_adapter: Some(member.adapter.clone()),
        config: ramshared_block::GpuWorkerConfig {
            target_bytes: member.target_bytes,
            reserve_floor_bytes: member.reserve_bytes,
            ..Default::default()
        },
        catalog_fd: None,
    };
    if rustix::process::geteuid().as_raw() != 0
        || ramshared_block::platform::probe_platform().platform != Some(PlatformKind::NativeLinux)
    {
        assert!(admit(&request).is_err());
    }
    assert!(
        admit_scoped(
            &WorkerRequest {
                fd: 0,
                generation: None,
                expected_adapter: Some(member.adapter.clone()),
                config: ramshared_block::GpuWorkerConfig {
                    target_bytes: member.target_bytes,
                    reserve_floor_bytes: member.reserve_bytes,
                    ..Default::default()
                },
                catalog_fd: None,
            },
            0,
            Some(PlatformKind::NativeLinux),
            &fixture.path(),
            &fixture.path(),
            fixture.uid(),
            |_| panic!("none skips manifest lookup"),
        )
        .unwrap()
        .is_none()
    );
    assert!(
        admit_scoped(
            &request,
            1000,
            Some(PlatformKind::NativeLinux),
            &fixture.path(),
            &fixture.path(),
            fixture.uid(),
            |_| panic!("non-root refused"),
        )
        .is_err()
    );
    assert!(
        admit_scoped(
            &request,
            0,
            Some(PlatformKind::Wsl2),
            &fixture.path(),
            &fixture.path(),
            fixture.uid(),
            |_| panic!("non-native refused"),
        )
        .is_err()
    );

    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("local/tests")
        .join(format!("worker-admit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let origin_manifest_path = dir.join("native-origin.toml");
    let encoded_origin = origin.manifest.encode().unwrap();
    std::fs::write(&origin_manifest_path, encoded_origin).unwrap();
    std::fs::set_permissions(
        &origin_manifest_path,
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();

    assert!(
        admit_scoped(
            &request,
            0,
            Some(PlatformKind::NativeLinux),
            &std::env::temp_dir().join("unprotected-origin.toml"),
            &fixture.path(),
            fixture.uid(),
            |_| panic!("unprotected path refused"),
        )
        .is_err()
    );
    assert!(
        admit_scoped(
            &request,
            0,
            Some(PlatformKind::NativeLinux),
            &origin_manifest_path,
            &fixture.path(),
            fixture.uid(),
            |_| Ok(None),
        )
        .is_err()
    );
    let admitted = admit_scoped(
        &request,
        0,
        Some(PlatformKind::NativeLinux),
        &origin_manifest_path,
        &fixture.path(),
        fixture.uid(),
        |_| Ok(Some(config)),
    )
    .unwrap()
    .unwrap();
    drop(admitted);
    std::fs::remove_dir_all(dir).unwrap();
}
