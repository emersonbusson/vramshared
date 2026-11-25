// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/service/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::broker_tenant::BrokerTenant;
use std::time::Duration;

struct FixedFree(u64);
impl FreeVram for FixedFree {
    fn free_bytes(&self) -> u64 {
        self.0
    }
}

#[derive(Default)]
struct MemDisk {
    created: bool,
    registered: bool,
    destroy_calls: u32,
    unreg_calls: u32,
}
impl DiskControl for MemDisk {
    fn create_disk(&mut self, _: u64, _: u32) -> Result<(), String> {
        self.created = true;
        Ok(())
    }
    fn destroy_disk(&mut self) -> Result<(), String> {
        self.destroy_calls += 1;
        self.created = false;
        Ok(())
    }
    fn register_queue(&mut self) -> Result<(), String> {
        self.registered = true;
        Ok(())
    }
    fn unregister_queue(&mut self) -> Result<(), String> {
        self.unreg_calls += 1;
        self.registered = false;
        Ok(())
    }
}

struct NopWipe;
impl WipeVram for NopWipe {
    fn zero(&mut self) -> Result<(), String> {
        Ok(())
    }
}

struct CountingGates {
    a: Result<Vec<String>, String>,
    b: Result<Vec<String>, String>,
    n: std::cell::Cell<u32>,
    lock_fail: bool,
    locked: bool,
}
impl PagefileGates for CountingGates {
    fn verify_volume_identity(&self, _: char) -> Result<(), String> {
        Ok(())
    }

    fn active_pagefiles(&self) -> Result<Vec<String>, String> {
        let i = self.n.get();
        self.n.set(i + 1);
        (if i == 0 { &self.a } else { &self.b }).clone()
    }
    fn lock_volume(&mut self, _: char) -> Result<(), String> {
        if self.lock_fail {
            return Err("lock denied".into());
        }
        self.locked = true;
        Ok(())
    }
    fn unlock_volume(&mut self) -> Result<(), String> {
        self.locked = false;
        Ok(())
    }
    fn flush_and_dismount(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn volume_locked(&self) -> bool {
        self.locked
    }
}

struct IdentityFailGates;

impl PagefileGates for IdentityFailGates {
    fn verify_volume_identity(&self, _: char) -> Result<(), String> {
        Err("serial mismatch".into())
    }

    fn active_pagefiles(&self) -> Result<Vec<String>, String> {
        Err("Gate A must not run after identity refusal".into())
    }

    fn lock_volume(&mut self, _: char) -> Result<(), String> {
        Err("volume lock must not run after identity refusal".into())
    }

    fn unlock_volume(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn flush_and_dismount(&mut self) -> Result<(), String> {
        Err("dismount must not run after identity refusal".into())
    }

    fn volume_locked(&self) -> bool {
        false
    }
}

struct RawGates {
    queries: std::cell::Cell<u32>,
}

impl PagefileGates for RawGates {
    fn verify_volume_identity(&self, _: char) -> Result<(), String> {
        Ok(())
    }

    fn active_pagefiles(&self) -> Result<Vec<String>, String> {
        self.queries.set(self.queries.get() + 1);
        Ok(vec![])
    }

    fn has_mounted_volume(&self) -> bool {
        false
    }

    fn lock_volume(&mut self, _: char) -> Result<(), String> {
        Err("an exact RAW disk has no volume to lock".into())
    }

    fn unlock_volume(&mut self) -> Result<(), String> {
        Err("an exact RAW disk has no volume to unlock".into())
    }

    fn flush_and_dismount(&mut self) -> Result<(), String> {
        Err("an exact RAW disk has no filesystem to dismount".into())
    }

    fn volume_locked(&self) -> bool {
        false
    }
}

fn cfg() -> WinDriveConfig {
    WinDriveConfig {
        size_bytes: 64 * 1024 * 1024,
        block_size: 4096,
        cuda_device: 0,
        reserve_bytes: 512 * 1024 * 1024,
        queue_depth: 4,
        max_io_bytes: 1024 * 1024,
        evidence_path: std::path::PathBuf::from(r"C:\ProgramData\RamShared\evidence"),
        volume_letter: 'D',
        volume_mount_path: None,
        broker_pipe: crate::config::BrokerPipeV1::NamedPipeV1,
        broker_ready_timeout_secs: 30,
        tenant: "wd".into(),
        heartbeat_secs: 5,
    }
}

fn online_state() -> ServiceState {
    ServiceState {
        lease: Some(LeaseState {
            lease: 1,
            bytes: 64 * 1024 * 1024,
        }),
        disk_created: true,
        registered_queue: true,
        online: true,
    }
}

#[test]
fn coresidence_fail_closed_no_disk() {
    let c = cfg();
    let mut state = ServiceState::default();
    let mut disk = MemDisk::default();
    let mut tenant = BrokerTenant::new("wd", Duration::from_secs(5));
    tenant.force_lease_for_test(1, c.size_bytes);
    let e = provision_after_lease(
        &c,
        &mut state,
        LeaseState {
            lease: 1,
            bytes: c.size_bytes,
        },
        &FixedFree(100),
        &mut disk,
        &mut tenant,
    )
    .unwrap_err();
    assert!(matches!(
        e,
        ProvisionError::Broker(BrokerTenantError::CoresidenceFailClosed { .. })
    ));
    assert!(!disk.created);
    assert!(state.lease.is_none());
}

#[test]
fn provision_ok_when_free_enough() {
    let c = cfg();
    let mut state = ServiceState::default();
    let mut disk = MemDisk::default();
    let mut tenant = BrokerTenant::new("wd", Duration::from_secs(5));
    provision_after_lease(
        &c,
        &mut state,
        LeaseState {
            lease: 2,
            bytes: c.size_bytes,
        },
        &FixedFree(2 << 30),
        &mut disk,
        &mut tenant,
    )
    .unwrap();
    assert!(disk.created && disk.registered);
    assert!(state.disk_created && state.online);
}

#[test]
fn provision_disk_create_failure_leaves_not_created() {
    struct FailCreateDisk;
    impl DiskControl for FailCreateDisk {
        fn create_disk(&mut self, _: u64, _: u32) -> Result<(), String> {
            Err("create failed".into())
        }
        fn destroy_disk(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn register_queue(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn unregister_queue(&mut self) -> Result<(), String> {
            Ok(())
        }
    }
    let c = cfg();
    let mut state = ServiceState::default();
    let mut disk = FailCreateDisk;
    let mut tenant = BrokerTenant::new("wd", Duration::from_secs(5));
    let e = provision_after_lease(
        &c,
        &mut state,
        LeaseState {
            lease: 2,
            bytes: c.size_bytes,
        },
        &FixedFree(2 << 30),
        &mut disk,
        &mut tenant,
    )
    .unwrap_err();
    assert!(matches!(e, ProvisionError::Disk(s) if s == "create failed"));
    assert!(!state.disk_created);
    assert!(!state.online);
}

#[test]
fn provision_disk_register_failure_leaves_disk_created_but_not_online() {
    struct FailRegisterDisk {
        created: bool,
    }
    impl DiskControl for FailRegisterDisk {
        fn create_disk(&mut self, _: u64, _: u32) -> Result<(), String> {
            self.created = true;
            Ok(())
        }
        fn destroy_disk(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn register_queue(&mut self) -> Result<(), String> {
            Err("register failed".into())
        }
        fn unregister_queue(&mut self) -> Result<(), String> {
            Ok(())
        }
    }
    let c = cfg();
    let mut state = ServiceState::default();
    let mut disk = FailRegisterDisk { created: false };
    let mut tenant = BrokerTenant::new("wd", Duration::from_secs(5));
    let e = provision_after_lease(
        &c,
        &mut state,
        LeaseState {
            lease: 2,
            bytes: c.size_bytes,
        },
        &FixedFree(2 << 30),
        &mut disk,
        &mut tenant,
    )
    .unwrap_err();
    assert!(matches!(e, ProvisionError::Disk(s) if s == "register failed"));
    assert!(state.disk_created);
    assert!(!state.online);
}

#[test]
fn pagefile_active_refuses_before_mutation() {
    let c = cfg();
    let mut state = online_state();
    let mut disk = MemDisk {
        created: true,
        registered: true,
        ..Default::default()
    };
    let mut wipe = NopWipe;
    let mut gates = CountingGates {
        a: Ok(vec![r"D:\pagefile.sys".into()]),
        b: Ok(vec![]),
        n: std::cell::Cell::new(0),
        lock_fail: false,
        locked: false,
    };
    let mut phases = Vec::new();
    let e = teardown_storage_only(
        &c,
        &mut state,
        &mut disk,
        &mut wipe,
        &mut gates,
        &mut phases,
    )
    .unwrap_err();
    assert!(matches!(e, ProvisionError::PagefileSafety(s) if s.contains("gate_a")));
    assert_eq!(disk.destroy_calls, 0);
    assert_eq!(disk.unreg_calls, 0);
    assert!(state.online);
    assert!(state.disk_created);
    assert!(phases.contains(&TeardownPhase::ResumeOnline));
}

/// Manufactured active-pagefile campaign (product path decision): a pagefile
/// identity on the product volume letter refuses Gate A before UNREGISTER/DESTROY.
#[test]
fn manufactured_pagefile_on_product_volume_refuses_gate_a() {
    let c = cfg();
    assert_eq!(c.volume_letter, 'D');
    let mut state = online_state();
    let mut disk = MemDisk {
        created: true,
        registered: true,
        ..Default::default()
    };
    let mut wipe = NopWipe;
    // Manufactured: configured/active identity points at product letter D:.
    let mut gates = CountingGates {
        a: Ok(vec![r"D:\pagefile.sys".into()]),
        b: Ok(vec![r"D:\pagefile.sys".into()]),
        n: std::cell::Cell::new(0),
        lock_fail: false,
        locked: false,
    };
    let mut phases = Vec::new();
    let e = teardown_storage_only(
        &c,
        &mut state,
        &mut disk,
        &mut wipe,
        &mut gates,
        &mut phases,
    )
    .unwrap_err();
    match e {
        ProvisionError::PagefileSafety(s) => {
            assert!(
                s.contains("gate_a_active"),
                "expected gate_a_active manufactured refuse, got {s}"
            );
            assert!(s.contains(r"D:\pagefile.sys") || s.contains("D:"), "{s}");
        }
        other => panic!("expected PagefileSafety, got {other}"),
    }
    let rt = pagefile_refusal_to_runtime(&ProvisionError::PagefileSafety(
        "gate_a_active: D:\\pagefile.sys".into(),
    ))
    .expect("maps to runtime code 7");
    assert_eq!(rt.code, 7);
    assert_eq!(disk.destroy_calls, 0);
    assert_eq!(disk.unreg_calls, 0);
    assert!(state.online && state.disk_created && state.registered_queue);
    assert_eq!(
        phases,
        [
            TeardownPhase::Identity,
            TeardownPhase::GateA,
            TeardownPhase::ResumeOnline
        ]
    );
}

#[test]
fn identity_mismatch_refuses_before_gate_a_or_mutation() {
    let c = cfg();
    let mut state = online_state();
    let mut disk = MemDisk {
        created: true,
        registered: true,
        ..Default::default()
    };
    let mut wipe = NopWipe;
    let mut gates = IdentityFailGates;
    let mut phases = Vec::new();
    let error = teardown_storage_only(
        &c,
        &mut state,
        &mut disk,
        &mut wipe,
        &mut gates,
        &mut phases,
    )
    .unwrap_err();
    assert!(
        matches!(error, ProvisionError::PagefileSafety(message) if message.contains("volume_identity"))
    );
    assert_eq!(phases, [TeardownPhase::Identity]);
    assert_eq!(disk.destroy_calls, 0);
    assert_eq!(disk.unreg_calls, 0);
    assert!(state.online);
}

#[test]
fn pagefile_query_error_refuses_before_mutation() {
    let c = cfg();
    let mut state = online_state();
    let mut disk = MemDisk {
        created: true,
        registered: true,
        ..Default::default()
    };
    let mut wipe = NopWipe;
    let mut gates = CountingGates {
        a: Err("WMI timeout".into()),
        b: Ok(vec![]),
        n: std::cell::Cell::new(0),
        lock_fail: false,
        locked: false,
    };
    let mut phases = Vec::new();
    let e = teardown_storage_only(
        &c,
        &mut state,
        &mut disk,
        &mut wipe,
        &mut gates,
        &mut phases,
    )
    .unwrap_err();
    assert!(matches!(e, ProvisionError::PagefileSafety(s) if s.contains("gate_a_query")));
    assert_eq!(disk.destroy_calls, 0);
    assert!(state.online);
}

#[test]
fn gate_b_failure_resumes_online_before_destroy() {
    let c = cfg();
    let mut state = online_state();
    let mut disk = MemDisk {
        created: true,
        registered: true,
        ..Default::default()
    };
    let mut wipe = NopWipe;
    let mut gates = CountingGates {
        a: Ok(vec![]),
        b: Ok(vec![r"D:\pagefile.sys".into()]),
        n: std::cell::Cell::new(0),
        lock_fail: false,
        locked: false,
    };
    let mut phases = Vec::new();
    let e = teardown_storage_only(
        &c,
        &mut state,
        &mut disk,
        &mut wipe,
        &mut gates,
        &mut phases,
    )
    .unwrap_err();
    assert!(matches!(e, ProvisionError::PagefileSafety(s) if s.contains("gate_b")));
    assert_eq!(disk.destroy_calls, 0);
    assert_eq!(disk.unreg_calls, 0);
    assert!(state.online);
    assert!(!gates.locked);
    assert!(phases.contains(&TeardownPhase::ResumeOnline));
}

#[test]
fn pagefile_absent_tears_down_cleanly() {
    let c = cfg();
    let mut state = online_state();
    let mut disk = MemDisk {
        created: true,
        registered: true,
        ..Default::default()
    };
    let mut wipe = NopWipe;
    let mut gates = CountingGates {
        a: Ok(vec![]),
        b: Ok(vec![]),
        n: std::cell::Cell::new(0),
        lock_fail: false,
        locked: false,
    };
    let mut phases = Vec::new();
    teardown_storage_only(
        &c,
        &mut state,
        &mut disk,
        &mut wipe,
        &mut gates,
        &mut phases,
    )
    .unwrap();
    assert!(!state.disk_created);
    assert!(!state.online);
    assert_eq!(disk.destroy_calls, 1);
    assert_eq!(disk.unreg_calls, 1);
    assert!(!gates.locked);
}

#[test]
fn exact_raw_disk_runs_both_gates_without_volume_operations() {
    let c = cfg();
    let mut state = online_state();
    let mut disk = MemDisk {
        created: true,
        registered: true,
        ..Default::default()
    };
    let mut wipe = NopWipe;
    let mut gates = RawGates {
        queries: std::cell::Cell::new(0),
    };
    let mut phases = Vec::new();

    teardown_storage_only(
        &c,
        &mut state,
        &mut disk,
        &mut wipe,
        &mut gates,
        &mut phases,
    )
    .unwrap();

    assert_eq!(gates.queries.get(), 2);
    assert_eq!(disk.unreg_calls, 1);
    assert_eq!(disk.destroy_calls, 1);
    assert!(!state.online);
    assert!(state.lease.is_none());
    assert_eq!(
        phases,
        [
            TeardownPhase::Identity,
            TeardownPhase::GateA,
            TeardownPhase::Drain,
            TeardownPhase::GateB,
            TeardownPhase::Unregister,
            TeardownPhase::Destroy,
            TeardownPhase::Wipe,
            TeardownPhase::Release,
        ]
    );
}

#[test]
fn stop_refusal_preserves_online_state() {
    let c = cfg();
    let mut state = online_state();
    let mut disk = MemDisk {
        created: true,
        registered: true,
        ..Default::default()
    };
    let mut wipe = NopWipe;
    let mut gates = CountingGates {
        a: Ok(vec![]),
        b: Ok(vec![]),
        n: std::cell::Cell::new(0),
        lock_fail: true,
        locked: false,
    };
    let mut phases = Vec::new();
    let e = teardown_storage_only(
        &c,
        &mut state,
        &mut disk,
        &mut wipe,
        &mut gates,
        &mut phases,
    )
    .unwrap_err();
    assert!(matches!(e, ProvisionError::PagefileSafety(ref s) if s.contains("volume_lock")));
    assert!(state.online);
    assert!(state.disk_created);
    assert_eq!(disk.destroy_calls, 0);
    let rt = pagefile_refusal_to_runtime(&e).unwrap();
    assert_eq!(rt.code, 7);
}

#[test]
fn clean_teardown_order_is_drain_lock_recheck_flush_dismount_unregister_destroy_unlock_wipe_release()
 {
    let c = cfg();
    let mut state = online_state();
    let mut disk = MemDisk {
        created: true,
        registered: true,
        ..Default::default()
    };
    let mut wipe = NopWipe;
    let mut gates = CountingGates {
        a: Ok(vec![]),
        b: Ok(vec![]),
        n: std::cell::Cell::new(0),
        lock_fail: false,
        locked: false,
    };
    let mut phases = Vec::new();
    teardown_storage_only(
        &c,
        &mut state,
        &mut disk,
        &mut wipe,
        &mut gates,
        &mut phases,
    )
    .unwrap();
    assert_eq!(
        phases,
        [
            TeardownPhase::Identity,
            TeardownPhase::GateA,
            TeardownPhase::Drain,
            TeardownPhase::VolumeLock,
            TeardownPhase::GateB,
            TeardownPhase::FlushDismount,
            TeardownPhase::Unregister,
            TeardownPhase::Destroy,
            TeardownPhase::Unlock,
            TeardownPhase::Wipe,
            TeardownPhase::Release,
        ]
    );
}

#[test]
fn teardown_target_requires_exact_unique_product_identity() {
    let expected = TeardownTarget::new('S', "A1B2C3D4E5F60708", 64 * 1024 * 1024).unwrap();
    let observed = ObservedVolumeIdentity {
        letter: 'S',
        vendor: "RAMSHARE".into(),
        product: "VRAMDISK".into(),
        serial: "A1B2C3D4E5F60708".into(),
        size_bytes: 64 * 1024 * 1024,
    };
    assert_eq!(expected.verify_unique(&[observed]).unwrap(), 'S');
}

#[test]
fn product_friendly_name_accepts_only_exact_identity_and_standard_suffix() {
    assert_eq!(
        parse_product_friendly_name("RAMSHARE VRAMDISK SCSI Disk Device").unwrap(),
        ("RAMSHARE".into(), "VRAMDISK".into())
    );
    assert_eq!(
        parse_product_friendly_name("RAMSHARE VRAMDISK").unwrap(),
        ("RAMSHARE".into(), "VRAMDISK".into())
    );
    assert_eq!(
        parse_product_friendly_name("ramshare vramdisk scsi disk device").unwrap(),
        ("RAMSHARE".into(), "VRAMDISK".into())
    );
    assert_eq!(
        parse_product_friendly_name("  RAMSHARE   VRAMDISK \t SCSI  Disk  Device \n").unwrap(),
        ("RAMSHARE".into(), "VRAMDISK".into())
    );
    assert!(parse_product_friendly_name("RAMSHARE OTHER SCSI Disk Device").is_err());
    assert!(parse_product_friendly_name("RAMSHARE VRAMDISK USB Device").is_err());
    assert!(parse_product_friendly_name("").is_err());
    assert!(parse_product_friendly_name("   \t\n").is_err());
    assert!(parse_product_friendly_name("RAMSHARE").is_err());
    assert!(parse_product_friendly_name("RAMSHARE VRAMDISK SCSI").is_err());
    assert!(parse_product_friendly_name("RAMSHARE VRAMDISK SCSI Disk").is_err());
    assert!(parse_product_friendly_name("RAMSHARE VRAMDISK SCSI Disk Device Extra").is_err());
    assert!(parse_product_friendly_name("OTHER VRAMDISK").is_err());
    assert!(parse_product_friendly_name("RAMSHARE OTHER").is_err());
    assert!(parse_product_friendly_name("VRAMDISK RAMSHARE").is_err());
    assert!(parse_product_friendly_name("RAMSHARE VRAMDISK USB Disk Device").is_err());
    assert_eq!(
        parse_product_friendly_name("INVALID").unwrap_err(),
        "unexpected product friendly name"
    );
}

#[test]
fn teardown_target_fails_closed_on_missing_mismatch_or_ambiguity() {
    let expected = TeardownTarget::new('S', "A1B2C3D4E5F60708", 64 * 1024 * 1024).unwrap();
    assert!(expected.verify_unique(&[]).is_err());

    let wrong = ObservedVolumeIdentity {
        letter: 'S',
        vendor: "KINGSTON".into(),
        product: "SSD".into(),
        serial: "OTHER".into(),
        size_bytes: 64 * 1024 * 1024,
    };
    assert!(expected.verify_unique(&[wrong]).is_err());

    let matching = ObservedVolumeIdentity {
        letter: 'S',
        vendor: "RAMSHARE".into(),
        product: "VRAMDISK".into(),
        serial: "A1B2C3D4E5F60708".into(),
        size_bytes: 64 * 1024 * 1024,
    };
    assert!(
        expected
            .verify_unique(&[matching.clone(), matching])
            .is_err()
    );
}
