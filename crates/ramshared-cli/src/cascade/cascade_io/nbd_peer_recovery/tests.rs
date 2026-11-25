// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/nbd_peer_recovery/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::super::nbd_netlink::tests::{
    FakeNetlinkTransport, finish_nbd_status_reply, netlink_error_msg, run_fake_nbd_handshake_server,
};
use super::*;
use std::os::unix::net::UnixListener;

#[test]
fn sysfs_block_size_and_live_peer_refusal_before_mutation() {
    let dir = std::env::temp_dir().join(format!("ramshared-nbd-sysfs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("nbd0")).unwrap();
    fs::write(dir.join("nbd0/size"), "8192\n").unwrap();
    assert_eq!(
        read_nbd_block_size_bytes(&dir, "/dev/nbd0").unwrap(),
        4 * 1024 * 1024
    );
    fs::write(dir.join("nbd0/size"), "0\n").unwrap();
    assert!(read_nbd_block_size_bytes(&dir, "/dev/nbd0").is_err());
    fs::write(dir.join("nbd0/size"), "not-a-number\n").unwrap();
    assert!(read_nbd_block_size_bytes(&dir, "/dev/nbd0").is_err());
    assert!(read_nbd_block_size_bytes(&dir, "").is_err());
    assert!(read_nbd_block_size_bytes(&dir, "/dev/nbd99").is_err());

    let mut binding_zram = crate::cascade::cascade_io::tests::lifecycle_binding_fixture(vec![
        crate::cascade::cascade_io::tests::bound_device_fixture(
            "/dev/nbd0",
            ManagedDeviceKind::Nbd,
        ),
        crate::cascade::cascade_io::tests::bound_device_fixture(
            "/dev/zram0",
            ManagedDeviceKind::Zram,
        ),
    ]);
    fs::create_dir_all(dir.join("zram0")).unwrap();
    fs::write(dir.join("zram0/backing_dev"), "/dev/nbd0\n").unwrap();
    assert_eq!(
        zram_backing_for_nbd(&dir, &binding_zram, "/dev/nbd0").as_deref(),
        Some("/dev/zram0")
    );
    fs::write(dir.join("zram0/backing_dev"), "/dev/loop99\n").unwrap();
    assert_eq!(zram_backing_for_nbd(&dir, &binding_zram, "/dev/nbd0"), None);
    binding_zram
        .devices
        .retain(|d| d.kind == ManagedDeviceKind::Nbd);
    assert_eq!(zram_backing_for_nbd(&dir, &binding_zram, "/dev/nbd0"), None);

    let mut binding = crate::cascade::cascade_io::tests::lifecycle_binding_fixture(vec![
        crate::cascade::cascade_io::tests::bound_device_fixture(
            "/dev/nbd0",
            ManagedDeviceKind::Nbd,
        ),
    ]);
    binding.daemon_pid = std::process::id();
    binding.daemon_start_ticks = 100;
    binding.daemon_instance_id = format!("{}-100", binding.daemon_pid);
    let paths = RuntimePaths::under(&dir);
    fs::create_dir_all(&paths.runtime_dir).unwrap();
    write_lifecycle_binding(&paths, &binding).unwrap();
    let err = recover_nbd_peer_from(&paths, || {
        Ok(crate::cascade::cascade_io::tests_suite::up_args_fixture())
    })
    .unwrap_err();
    assert!(!err.to_string().is_empty());

    // Writeback mode keeps NBD out of /proc/swaps while zram backing_dev names it.
    let zram_dev = crate::cascade::cascade_io::tests::bound_device_fixture(
        "/dev/zram0",
        ManagedDeviceKind::Zram,
    );
    let mut wb_binding = crate::cascade::cascade_io::tests::lifecycle_binding_fixture(vec![
        crate::cascade::cascade_io::tests::bound_device_fixture(
            "/dev/nbd0",
            ManagedDeviceKind::Nbd,
        ),
        zram_dev.clone(),
    ]);
    wb_binding.boot_id = current_boot_id().unwrap();
    let origin = read_sealed_origin_config().unwrap();
    wb_binding.origin = BoundOriginIdentity {
        partuuid: origin.partuuid,
        ptuuid: origin.ptuuid,
        partition_dev_t: origin.partition_dev_t,
        parent_dev_t: origin.parent_dev_t,
        expected_swap_uuid: origin.expected_swap_uuid,
        host_manifest_sha256: origin.host_manifest_sha256,
        configuration_sha256: origin.configuration_sha256,
    };
    wb_binding.daemon_pid = i32::MAX as u32;
    wb_binding.daemon_instance_id = format!("{}-100", wb_binding.daemon_pid);
    write_lifecycle_binding(&paths, &wb_binding).unwrap();
    fs::create_dir_all(paths.sys_block.join("nbd0")).unwrap();
    fs::write(paths.sys_block.join("nbd0/size"), "8192\n").unwrap();
    fs::create_dir_all(paths.sys_block.join("zram0")).unwrap();
    fs::write(paths.sys_block.join("zram0/backing_dev"), "/dev/nbd0\n").unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dead_peer_recovery_refuses_released_tier_rolls_back_failed_netlink_and_succeeds() {
    let dir = std::env::temp_dir().join(format!("ramshared-nbd-peer-rec-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let paths = RuntimePaths::under(&dir);
    fs::create_dir_all(&paths.runtime_dir).unwrap();
    fs::create_dir_all(paths.sys_block.join("nbd0")).unwrap();
    fs::write(paths.sys_block.join("nbd0/size"), "8192\n").unwrap();

    let nbd_dev = crate::cascade::cascade_io::tests::bound_device_fixture(
        "/dev/nbd0",
        ManagedDeviceKind::Nbd,
    );
    let mut binding =
        crate::cascade::cascade_io::tests::lifecycle_binding_fixture(vec![nbd_dev.clone()]);
    binding.boot_id = current_boot_id().unwrap();
    let origin = read_sealed_origin_config().unwrap();
    binding.origin = BoundOriginIdentity {
        partuuid: origin.partuuid,
        ptuuid: origin.ptuuid,
        partition_dev_t: origin.partition_dev_t,
        parent_dev_t: origin.parent_dev_t,
        expected_swap_uuid: origin.expected_swap_uuid,
        host_manifest_sha256: origin.host_manifest_sha256,
        configuration_sha256: origin.configuration_sha256,
    };
    binding.daemon_pid = i32::MAX as u32;
    binding.daemon_instance_id = format!("{}-100", binding.daemon_pid);
    assert!(daemon_instance_id_from_pid(binding.daemon_pid).is_none());
    write_lifecycle_binding(&paths, &binding).unwrap();

    let params = PeerRecoveryParams {
        daemon_path: "/bin/sleep".into(),
        vram_mb: 4,
        cache_cap_mib: 64,
        origin_path: "/dev/null".into(),
        accept_wsl2_nbd: true,
        spill_pump_tier: None,
    };

    let mut unused_transport = FakeNetlinkTransport::with_replies(44, vec![]);
    let released_err = recover_dead_nbd_peer_with(
        &paths,
        &params,
        &[],
        std::slice::from_ref(&nbd_dev),
        &mut unused_transport,
        |_, _, _| unreachable!(),
    )
    .unwrap_err();
    assert!(released_err.to_string().contains("recover-released-peer"));

    let active_swaps = vec![SwapEntry {
        filename: "/dev/nbd0".into(),
        size_kb: 4096,
        used_kb: 512,
        priority: 100,
    }];

    let listener = UnixListener::bind(&paths.socket).unwrap();
    let mismatch_err = recover_dead_nbd_peer_with(
        &paths,
        &params,
        &active_swaps,
        std::slice::from_ref(&nbd_dev),
        &mut unused_transport,
        |_, _, _| unreachable!(),
    )
    .unwrap_err();
    assert!(mismatch_err.to_string().contains("socket identity"));
    drop(listener);
    let _ = fs::remove_file(&paths.socket);

    let spawn_with_handshake = |sock: PathBuf| {
        let _ = fs::remove_file(&sock);
        let thread = run_fake_nbd_handshake_server(&sock, 4 * 1024 * 1024);
        let child = std::process::Command::new("/usr/bin/sleep")
            .arg("20")
            .spawn()
            .unwrap();
        (child, thread)
    };

    let mut h1 = None;
    let mut fail_transport = FakeNetlinkTransport::with_replies(
        44,
        vec![
            finish_nbd_status_reply(44, 2, 0, 0),
            netlink_error_msg(3, -libc::EINVAL),
        ],
    );
    let netlink_err = recover_dead_nbd_peer_with(
        &paths,
        &params,
        &active_swaps,
        std::slice::from_ref(&nbd_dev),
        &mut fail_transport,
        |paths, _, _| {
            let (child, thread) = spawn_with_handshake(paths.socket.clone());
            h1 = Some(thread);
            Ok(child)
        },
    )
    .unwrap_err();
    assert!(netlink_err.to_string().contains("errno"), "{netlink_err}");
    h1.unwrap().join().unwrap();
    let _ = fs::remove_file(&paths.socket);

    let mut h2 = None;
    let mut expected_pid = 0u32;
    let mut ok_transport = FakeNetlinkTransport::with_replies(
        44,
        vec![
            finish_nbd_status_reply(44, 2, 0, 1),
            netlink_error_msg(3, 0),
        ],
    );
    let new_pid = recover_dead_nbd_peer_with(
        &paths,
        &params,
        &active_swaps,
        std::slice::from_ref(&nbd_dev),
        &mut ok_transport,
        |paths, _, _| {
            let (child, thread) = spawn_with_handshake(paths.socket.clone());
            expected_pid = child.id();
            h2 = Some(thread);
            Ok(child)
        },
    )
    .unwrap();
    assert_eq!(new_pid, expected_pid);
    h2.unwrap().join().unwrap();
    let updated = read_lifecycle_binding(&paths).unwrap();
    assert_eq!(updated.daemon_pid, expected_pid);
    assert_eq!(
        updated.export_socket,
        socket_identity(&paths.socket).unwrap()
    );
    if let Some(pid) = Pid::from_raw(expected_pid as i32) {
        let _ = kill_process(pid, Signal::KILL);
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty()).ok();
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn recover_up_args_enables_nbd_and_wsl2_acceptance() {
    assert_eq!(recover_up_args(false), &["--transport", "nbd"]);
    assert_eq!(
        recover_up_args(true),
        &["--transport", "nbd", "--accept-wsl2-nbd"]
    );
}
