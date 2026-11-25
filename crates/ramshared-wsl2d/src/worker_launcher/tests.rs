// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_launcher/tests.rs
//! Actual launcher EOF and empty-refusal ownership; no GPU driver is loaded.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::time::{Duration, Instant};

fn request() -> WorkerSpawnParams {
    WorkerSpawnParams {
        generation: None,
        target_bytes: 4096,
        chunk_bytes: 4096,
        reserve_floor: 0,
        expected_adapter: None,
        host_catalog: None,
    }
}

#[test]
fn asynchronous_production_launcher_retains_failure_until_scope_is_quiescent() {
    for expected_adapter in [
        None,
        Some(ramshared_vram::GpuAdapterIdentity {
            node_mask: None,
            backend: "cuda".into(),
            key: "00112233445566778899aabbccddeeff".into(),
            luid: None,
        }),
    ] {
        let params = WorkerSpawnParams {
            expected_adapter,
            ..request()
        };
        // current_exe is libtest. It rejects --fd before any driver call;
        // this executes the actual parent spawn and real Unix EOF path.
        let mut startup = begin(&params).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let error = loop {
            match startup.poll() {
                Err(error) => break error,
                Ok(Some(_)) => panic!("failed child advertised a cache"),
                Ok(None) => assert!(
                    Instant::now() < deadline,
                    "actual launcher did not report EOF"
                ),
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(error.contains("worker handshake failed"), "{error}");
        while !startup.is_quiescent() {
            assert!(
                Instant::now() < deadline,
                "actual failure child/creator was retained past exit bound"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(
            worker_registry::registry()
                .reserve(params.expected_adapter.as_ref())
                .unwrap(),
        );
    }
}

#[test]
fn invalid_worker_request_releases_only_empty_launch_custody() {
    let params = WorkerSpawnParams {
        generation: None,
        chunk_bytes: 0,
        ..request()
    };
    let outcome = launch(&params);
    assert!(outcome.result.is_err());
    assert!(outcome.creator_child.is_none());
    drop(worker_registry::registry().reserve(None).unwrap());

    let params = WorkerSpawnParams {
        generation: None,
        expected_adapter: Some(ramshared_vram::GpuAdapterIdentity {
            node_mask: None,
            backend: "cuda".into(),
            key: "invalid".into(),
            luid: None,
        }),
        ..request()
    };
    let outcome = launch(&params);
    assert!(
        outcome
            .result
            .err()
            .unwrap()
            .contains("invalid worker custody identity")
    );
    assert!(outcome.creator_child.is_none());
    drop(worker_registry::registry().reserve(None).unwrap());
}

#[test]
fn launch_with_host_catalog_exercises_catalog_fd_inheritance() {
    let params = WorkerSpawnParams {
        host_catalog: Some(ramshared_wsl2d::host_catalog::SharedHostCatalog::new()),
        ..request()
    };
    let outcome = launch(&params);
    assert!(outcome.result.is_err());
    drop(outcome);
    let deadline = Instant::now() + Duration::from_secs(3);
    while worker_registry::registry().reserve(None).is_err() {
        assert!(
            Instant::now() < deadline,
            "worker registry remained busy after host-catalog launch"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn production_launcher_refuses_revoked_recovery_authority() {
    let mut startup = begin_guarded(&request(), std::sync::Arc::new(|| false)).unwrap();
    assert!(startup.poll().is_err());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !startup.is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(worker_registry::registry().reserve(None).is_ok());
}
