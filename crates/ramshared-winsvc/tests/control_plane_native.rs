// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/control_plane_native.rs
//! Explicit current Windows/WSL session; no installation or swap mutation.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_winsvc::vsock_lease::{StopState, spawn_configured_listener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[test]
#[ignore = "read-only: current WSL product guest, explicit VM/port and exact built product"]
fn native_owned_control_plane_stops_active_guest() {
    let vm = std::env::var("RAMSHARED_NATIVE_EXPECTED_VM_ID").unwrap();
    ramshared_winsvc::vm_id_resolve::parse_guid(&vm).unwrap();
    let port: u32 = std::env::var("RAMSHARED_NATIVE_EXPECTED_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let ready = PathBuf::from(std::env::var_os("RAMSHARED_NATIVE_GUEST_READY").unwrap());
    assert!(ready.is_absolute() && !ready.exists());
    let product = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("ramshared-winsvc.exe");
    let directory =
        std::env::temp_dir().join(format!("ramshared-native-stop-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    let key = directory.join("test.key");
    std::fs::write(&key, b"native-control-lifecycle-test-secret").unwrap();
    let config = directory.join("test.toml");
    std::fs::write(
        &config,
        format!(
            "[vsock]\nmode = \"connect\"\nport = {port}\nvm_id = \"{vm}\"\nkey_file = {}\n",
            serde_json::to_string(&key.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
    let mut task = spawn_configured_listener(Some(&config), &product)
        .unwrap()
        .unwrap();
    assert!(
        spawn_configured_listener(Some(&config), &product)
            .unwrap_err()
            .to_string()
            .contains("already retained")
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.is_file() {
        assert!(
            Instant::now() < deadline,
            "current guest never proved its authenticated catalog"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut pending = 0;
    loop {
        match task.wait_for_stop(Duration::from_millis(100)).unwrap() {
            StopState::Stopped => break,
            StopState::Pending => pending += 1,
        }
        assert!(
            pending < 100,
            "native task remains owned, stop is not complete"
        );
    }
    assert_eq!(
        task.wait_for_stop(Duration::ZERO).unwrap(),
        StopState::Stopped
    );
    assert!(ramshared_winsvc::gpu_identity::sampler::observation().is_none());
    assert!(ramshared_winsvc::vm_identity::sampler::observation().is_none());
    eprintln!("NATIVE_CONTROL_STOP_OK joined=true samplers_stopped=true duplicate_refused=true");
}
