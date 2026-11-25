// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/control_plane_stop.rs
//! Connected current native host catalog and stop, without daemon or swap activation.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]
use ramshared_wsl2d::control_plane::{ControlPlane, VsockControlPlane};
use ramshared_wsl2d::host_gate::{ControlPlaneState, SealedOrigin, SharedAuthority};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
fn wall() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
#[ignore = "read-only: exact current Windows owned host test and isolated native vsock port"]
fn connected_native_control_stop_revokes_catalog_and_preserves_origin_authority() {
    let port: u32 = std::env::var("RAMSHARED_LIVE_VSOCK_PORT")
        .unwrap()
        .parse()
        .unwrap();
    println!("NATIVE_GUEST_LISTENING port={port}");
    let stream = ramshared_ipc::vsock::listen_vsock(port, Duration::from_secs(25)).unwrap();
    let shared = SharedAuthority::new();
    let mut guest = VsockControlPlane::new(
        stream,
        b"native-control-lifecycle-test-secret".to_vec(),
        "native-stop-test-boot".into(),
        "native-stop-test-distro".into(),
        SealedOrigin {
            logical_capacity_mib: 16,
            partuuid: "test-local-origin".into(),
            origin_vhdx: "test-origin".into(),
        },
        Duration::from_secs(2),
    )
    .with_authority_mirror(shared.begin_session());
    guest.perform_handshake(wall()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        guest.heartbeat(wall()).unwrap();
        if let Some(report) = guest.gpu_catalog() {
            assert!(
                !report.adapters.is_empty(),
                "native GPU query must actually observe an adapter"
            );
            assert!(guest.admit_cache(wall()).is_ok());
            assert!(guest.admit_origin_io().is_ok());
            println!("NATIVE_GUEST_CATALOG authenticated=true observed_adapter=true");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "actual native catalog never became available"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while guest.heartbeat(wall()).is_ok() {
        assert!(Instant::now() < deadline, "native host did not stop");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(guest.state(), ControlPlaneState::SafeMode);
    assert!(guest.gpu_catalog().is_none());
    assert!(guest.admit_cache(wall()).is_err());
    assert!(guest.admit_origin_io().is_ok());
    println!("NATIVE_GUEST_STOP_OK catalog_revoked=true cache_refused=true origin_authority=true");
}
