// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/sparse_vram/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use ramshared_vram::enforced_free_floor_from_configured;

/// A policy with an explicit configured minimum and the sealed percent.
///
/// `min_floor_bytes` is the resolved minimum term (DT-1), not a raw
/// "reserve floor". `0` is the DT-9 degenerate case that never raises an
/// allocation because the 20% share still binds.
fn policy(min_floor_bytes: u64) -> ReserveFloorPolicy {
    ReserveFloorPolicy {
        min_floor_bytes,
        sealed_percent: SEALED_RESERVE_PERCENT,
    }
}
use std::cell::Cell;

struct FakeMem(Vec<u8>);

impl VramMemory for FakeMem {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn zero(&mut self) -> Result<(), VramError> {
        self.0.fill(0);
        Ok(())
    }
    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError> {
        let off = off as usize;
        let end = off
            .checked_add(dst.len())
            .filter(|&e| e <= self.0.len())
            .ok_or(VramError::OutOfRange {
                off: off as u64,
                len: dst.len() as u64,
                size: self.0.len() as u64,
            })?;
        dst.copy_from_slice(&self.0[off..end]);
        Ok(())
    }
    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError> {
        let off = off as usize;
        let end = off
            .checked_add(src.len())
            .filter(|&e| e <= self.0.len())
            .ok_or(VramError::OutOfRange {
                off: off as u64,
                len: src.len() as u64,
                size: self.0.len() as u64,
            })?;
        self.0[off..end].copy_from_slice(src);
        Ok(())
    }
}

struct FakeProvider {
    allocs: Cell<usize>,
    fail_next: Cell<bool>,
}

impl FakeProvider {
    fn new() -> Self {
        Self {
            allocs: Cell::new(0),
            fail_next: Cell::new(false),
        }
    }
}

/// Provider whose reported free/total are settable, so the shared free
/// floor can be exercised at its boundary.
struct VarProvider {
    free: Cell<u64>,
    total: u64,
    allocs: Cell<usize>,
}

impl VarProvider {
    fn new(free: u64, total: u64) -> Self {
        Self {
            free: Cell::new(free),
            total,
            allocs: Cell::new(0),
        }
    }
}

impl VramProvider for VarProvider {
    type Mem<'a>
        = FakeMem
    where
        Self: 'a;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.allocs.set(self.allocs.get() + 1);
        Ok(FakeMem(vec![0u8; bytes]))
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((self.free.get(), self.total))
    }
}

impl VramProvider for FakeProvider {
    type Mem<'a>
        = FakeMem
    where
        Self: 'a;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        if self.fail_next.get() {
            self.fail_next.set(false);
            return Err(VramError::Provider("injected fail".into()));
        }
        self.allocs.set(self.allocs.get() + 1);
        Ok(FakeMem(vec![0u8; bytes]))
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((8 << 30, 8 << 30))
    }
}

#[test]
fn zero_block_size_is_rejected_without_panic() {
    let provider = FakeProvider::new();
    assert!(SparseVramBackend::new(&provider, 4096, 4096, 0).is_err());
}

#[test]
fn physical_bounds_refuse_provider_io() {
    let provider = FakeProvider::new();
    let mut backend = SparseVramBackend::new(&provider, 1024 * 1024, 256 * 1024, 4096).unwrap();
    backend.ensure_live(0).unwrap();
    backend.chunks[0].mem.as_mut().unwrap().0.truncate(4096);

    let write_error = backend.write_at(0, &[1u8; 8192]).unwrap_err();
    assert!(write_error.0.contains("sparse physical write oob"));

    let mut read_buffer = [0u8; 8192];
    let read_error = backend.read_at(0, &mut read_buffer).unwrap_err();
    assert!(read_error.0.contains("sparse physical read oob"));
}

#[test]
fn page_table_bounds_guard_enforces_limit() {
    let p = FakeProvider::new();
    let mut be = SparseVramBackend::new(&p, 1024 * 1024, 256 * 1024, 4096).unwrap();
    // Artificially truncate chunks array to simulate a broken page table
    be.chunks.pop();
    let off = 3 * 256 * 1024;
    let err_read = be.read_at(off, &mut [0u8; 4096]).unwrap_err();
    assert!(err_read.0.contains("sparse page table oob idx="));

    let err_write = be.write_at(off, &[0u8; 4096]).unwrap_err();
    assert!(err_write.0.contains("sparse page table oob idx="));
}

#[test]
fn read_empty_is_zeros_without_alloc() {
    let p = FakeProvider::new();
    let mut be = SparseVramBackend::new(&p, 1024 * 1024, 256 * 1024, 4096).unwrap();
    let mut buf = [0xAAu8; 8192];
    be.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, [0u8; 8192]);
    assert_eq!(p.allocs.get(), 0);
    assert_eq!(be.chunks_live(), 0);
}

#[test]
fn write_then_read_roundtrip_one_chunk() {
    let p = FakeProvider::new();
    let mut be = SparseVramBackend::new(&p, 1024 * 1024, 256 * 1024, 4096).unwrap();
    let payload = vec![0x5Au8; 4096];
    be.write_at(4096, &payload).unwrap();
    assert_eq!(p.allocs.get(), 1);
    assert_eq!(be.chunks_live(), 1);
    let mut buf = vec![0u8; 4096];
    be.read_at(4096, &mut buf).unwrap();
    assert_eq!(buf, payload);
}

#[test]
fn cross_chunk_write_two_allocs() {
    let p = FakeProvider::new();
    let chunk = 256 * 1024u64;
    let mut be = SparseVramBackend::new(&p, 2 * chunk, chunk, 4096).unwrap();
    // 8 KiB straddling the boundary
    let mut payload = vec![0x11u8; 8192];
    payload[0] = 0xAA;
    payload[8191] = 0xBB;
    let off = chunk - 4096;
    be.write_at(off, &payload).unwrap();
    assert_eq!(p.allocs.get(), 2);
    let mut buf = vec![0u8; 8192];
    be.read_at(off, &mut buf).unwrap();
    assert_eq!(buf, payload);
}

#[test]
fn reclaim_blocked_when_used_kb_nonzero() {
    let p = FakeProvider::new();
    let mut be = SparseVramBackend::new(&p, 1024 * 1024, 256 * 1024, 4096).unwrap();
    be.write_at(0, &[1u8; 4096]).unwrap();
    assert_eq!(be.chunks_live(), 1);
    let freed = be
        .try_reclaim(100, Some(0), 1 << 30, Duration::from_secs(0))
        .unwrap();
    assert_eq!(freed, 0);
    assert_eq!(be.chunks_live(), 1);
}

#[test]
fn reclaim_frees_when_used_zero_and_below_floor() {
    let p = FakeProvider::new();
    let mut be = SparseVramBackend::new(&p, 1024 * 1024, 256 * 1024, 4096).unwrap();
    be.write_at(0, &[1u8; 4096]).unwrap();
    let freed = be
        .try_reclaim(0, Some(0), 1 << 30, Duration::from_secs(9999))
        .unwrap();
    assert!(freed > 0);
    assert_eq!(be.chunks_live(), 0);
    // reads still zeros
    let mut buf = [0xFFu8; 4096];
    be.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, [0u8; 4096]);
}

#[test]
fn alloc_fail_returns_io_error() {
    let p = FakeProvider::new();
    p.fail_next.set(true);
    let mut be = SparseVramBackend::new(&p, 1024 * 1024, 256 * 1024, 4096).unwrap();
    let err = be.write_at(0, &[1u8; 4096]).unwrap_err();
    assert!(err.0.contains("alloc") || err.0.contains("fail"));
    assert_eq!(be.alloc_fails, 1);
}

#[test]
fn free_floor_refuses_when_headroom_tight() {
    // FakeProvider reports 8GiB free always — use commit_cap instead for hard stop.
    let p = FakeProvider::new();
    let chunk = 256 * 1024u64;
    let mut be = SparseVramBackend::new_with_limits(
        &p,
        2 * chunk,
        chunk,
        4096,
        policy(0),   // no configured floor (fake has lots of free)
        Some(chunk), // only one chunk allowed
    )
    .unwrap();
    be.write_at(0, &[1u8; 4096]).unwrap();
    let err = be.write_at(chunk, &[2u8; 4096]).unwrap_err();
    assert!(err.0.contains("commit_cap"), "{err:?}");
    assert_eq!(be.chunks_live(), 1);
    assert!(be.floor_refuses >= 1);
}

#[test]
fn host_budget_denial_prevents_cuda_allocation() {
    struct Deny;
    impl CommitBudgetGate for Deny {
        fn allow_commit(&self, _committed: u64, _next_chunk: u64) -> Result<(), String> {
            Err("WDDM constrained".into())
        }
    }
    let p = FakeProvider::new();
    let mut be = SparseVramBackend::new_with_limits_and_gate(
        &p,
        1024 * 1024,
        256 * 1024,
        4096,
        policy(0),
        None,
        Some(&Deny),
    )
    .unwrap();
    let error = be.write_at(0, &[1u8; 4096]).unwrap_err();
    assert!(error.0.contains("WDDM constrained"), "{error:?}");
    assert_eq!(p.allocs.get(), 0);
    assert_eq!(be.budget_refuses, 1);
}

#[test]
fn safe_commit_cap_leaves_reserve() {
    let cap = safe_commit_cap(6 << 30, 6 << 30, 512 << 20);
    assert_eq!(cap, (6 << 30) - (512 << 20));
    let cap2 = safe_commit_cap(4 << 30, 6 << 30, 512 << 20);
    assert_eq!(cap2, 4 << 30);
}

#[test]
fn rejects_zero_capacity_and_bad_chunk() {
    let p = FakeProvider::new();
    assert!(SparseVramBackend::new(&p, 0, 256 * 1024, 4096).is_err());
    assert!(SparseVramBackend::new(&p, 1024 * 1024, 0, 4096).is_err());
    assert!(SparseVramBackend::new(&p, 1024 * 1024, 1000, 4096).is_err());
}

#[test]
fn ensure_live_out_of_bounds_returns_io_error() {
    let p = FakeProvider::new();
    let mut be = SparseVramBackend::new(&p, 1024 * 1024, 256 * 1024, 4096).unwrap();
    let err = be.ensure_live(9999).expect_err("should return IoError");
    assert!(
        err.0.contains("sparse page table oob idx=9999")
            || err.0.contains("exceeds physical map len")
    );
}

#[test]
fn empty_read_write_and_flush_and_accessors() {
    let p = FakeProvider::new();
    let mut be = SparseVramBackend::new(&p, 1024 * 1024, 256 * 1024, 4096).unwrap();
    be.read_at(0, &mut []).unwrap();
    be.write_at(0, &[]).unwrap();
    be.flush().unwrap();
    assert_eq!(be.size_bytes(), 1024 * 1024);
    assert_eq!(be.block_size(), 4096);
    assert_eq!(be.capacity_bytes(), 1024 * 1024);
    assert_eq!(be.chunk_bytes(), 256 * 1024);
    assert!(be.chunks_total() >= 1);
    assert_eq!(be.committed_bytes(), 0);
    assert!(be.commit_cap_bytes() > 0);
    assert!(be.reserve_policy().min_floor_bytes > 0);
}

#[test]
fn free_all_live_and_oob_read() {
    let p = FakeProvider::new();
    let mut be = SparseVramBackend::new(&p, 1024 * 1024, 256 * 1024, 4096).unwrap();
    be.write_at(0, &[9u8; 4096]).unwrap();
    assert_eq!(be.chunks_live(), 1);
    let freed = be.free_all_live();
    assert!(freed > 0);
    assert_eq!(be.chunks_live(), 0);
    let mut buf = [0u8; 16];
    assert!(be.read_at(2 * 1024 * 1024, &mut buf).is_err());
    assert!(be.write_at(2 * 1024 * 1024, &[1u8; 16]).is_err());
}

#[test]
fn free_floor_refuses_when_provider_free_is_low() {
    struct TightProvider;
    impl VramProvider for TightProvider {
        type Mem<'a>
            = FakeMem
        where
            Self: 'a;
        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
            Ok(FakeMem(vec![0u8; bytes]))
        }
        fn mem_info(&self) -> Result<(u64, u64), VramError> {
            Ok((64 * 1024, 8 << 30)) // free tiny
        }
    }
    let p = TightProvider;
    let mut be = SparseVramBackend::new_with_limits(
        &p,
        1024 * 1024,
        256 * 1024,
        4096,
        policy(512 * 1024), // reserve 512KiB
        None,
    )
    .unwrap();
    let err = be.write_at(0, &[1u8; 4096]).unwrap_err();
    assert!(
        err.0.contains("free-floor") || err.0.contains("floor"),
        "{err:?}"
    );
}

#[test]
fn mem_info_error_surfaces() {
    struct BadInfo;
    impl VramProvider for BadInfo {
        type Mem<'a>
            = FakeMem
        where
            Self: 'a;
        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
            Ok(FakeMem(vec![0u8; bytes]))
        }
        fn mem_info(&self) -> Result<(u64, u64), VramError> {
            Err(VramError::Provider("no gpu".into()))
        }
    }
    let p = BadInfo;
    let mut be =
        SparseVramBackend::new_with_limits(&p, 1024 * 1024, 256 * 1024, 4096, policy(0), None)
            .unwrap();
    let err = be.write_at(0, &[1u8; 4096]).unwrap_err();
    assert!(
        err.0.contains("mem_info") || err.0.contains("no gpu"),
        "{err:?}"
    );
    assert_eq!(be.alloc_fails, 1);
}

#[test]
fn env_helpers_have_sane_defaults() {
    // Do not clobber user env permanently — only assert defaults when unset
    if std::env::var("RAMSHARED_VRAM_CHUNK_MIB").is_err() {
        let b = chunk_bytes_from_env();
        assert!(b >= 16 * 1024 * 1024);
    }
    if std::env::var("RAMSHARED_VRAM_IDLE_FREE_SEC").is_err() {
        assert_eq!(idle_free_secs_from_env(), 30);
    }
    if std::env::var("RAMSHARED_MIN_VRAM_FREE_MIB").is_err()
        && std::env::var("MIN_VRAM_HEADROOM_MIB").is_err()
    {
        // DT-3: the sealed manifest authority, not an arbitrary default.
        // The old reader returned 512 MiB and clamped overrides to
        // [128, 4096], silently accepting a 128 MiB floor on a shared host.
        let policy = sealed_reserve_policy();
        assert_eq!(policy.min_floor_bytes, SEALED_RESERVE_MIN_MIB * 1024 * 1024);
        assert_eq!(policy.sealed_percent, SEALED_RESERVE_PERCENT);
    }
    if std::env::var("RAMSHARED_VRAM_COMMIT_CAP_MIB").is_err() {
        assert!(commit_cap_bytes_from_env() > 1 << 30);
    }
}
#[test]
fn sealed_reserve_policy_rejects_override_below_sealed() {
    // DT-8: raise-only. A 128 MiB override — the value the old clamp
    // silently accepted — must be refused, not clamped up.
    let base = ReserveFloorPolicy::from_manifest(SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT)
        .expect("sealed literals are valid");
    let too_low = ReserveFloorEnv {
        env_mib: Some(128),
        alias_mib: None,
    };
    assert!(ReserveFloorPolicy::resolve_with_env(&base, &too_low).is_err());
    // A more conservative override is accepted and raises only the minimum.
    let raised = ReserveFloorEnv {
        env_mib: Some(SEALED_RESERVE_MIN_MIB + 1024),
        alias_mib: None,
    };
    let resolved = ReserveFloorPolicy::resolve_with_env(&base, &raised).expect("raise is legal");
    assert_eq!(
        resolved.min_floor_bytes,
        (SEALED_RESERVE_MIN_MIB + 1024) * 1024 * 1024
    );
    // The sealed percentage share is never dropped through the seam.
    assert_eq!(resolved.sealed_percent, SEALED_RESERVE_PERCENT);
}

#[test]
fn sparse_admission_uses_shared_reserve_floor() {
    // invariant check— refusal plus legitimate pass.
    //
    // DT-5: the sparse tier must refuse below the **shared** three-term
    // floor, not below the configured reserve alone. Before this rewire,
    // `reserve_floor_bytes = 0` admitted any allocation and the tier
    // dropped both the 20% capacity share and the 640 MiB runtime buffer.
    let chunk = 4 * 1024 * 1024u64;
    let total = 5 * 1024 * 1024 * 1024u64;
    // Shared floor = max(0, ceil(5 GiB / 5)) + 640 MiB = 1 GiB + 640 MiB.
    let expected_floor = enforced_free_floor_from_configured(0, total, RUNTIME_FREE_BUFFER_BYTES);
    assert_eq!(expected_floor, (1 << 30) + RUNTIME_FREE_BUFFER_BYTES);

    // Refusal: free is above the *configured* floor (0) but below the
    // shared floor. The old surface would have admitted this.
    let just_below = expected_floor - 1;
    let p = VarProvider::new(just_below, total);
    let mut be =
        SparseVramBackend::new_with_limits(&p, 2 * chunk, chunk, 4096, policy(0), None).unwrap();
    let err = be.write_at(0, &[1u8; 4096]).unwrap_err();
    assert!(
        err.0.contains("shared-floor"),
        "must refuse under the shared floor: {err:?}"
    );
    assert_eq!(be.floor_refuses, 1);
    assert_eq!(be.chunks_live(), 0);

    // Legitimate pass: the same request above the shared floor still hits.
    let p = VarProvider::new(expected_floor + chunk, total);
    let mut be =
        SparseVramBackend::new_with_limits(&p, 2 * chunk, chunk, 4096, policy(0), None).unwrap();
    be.write_at(0, &[1u8; 4096]).unwrap();
    assert_eq!(be.chunks_live(), 1);
    assert_eq!(be.floor_refuses, 0);
}

#[test]
fn sparse_probe_floor_matches_shared_reserve() {
    // DT-5: the demotion probe floor is the same value admission uses.
    let total = 6 * 1024 * 1024 * 1024u64;
    let configured = 2048 * 1024 * 1024u64;
    let capacity = ramshared_vram::ReserveFloorPolicy::helper_capacity(Some(total), total);
    let floor =
        enforced_free_floor_from_configured(configured, capacity, RUNTIME_FREE_BUFFER_BYTES);
    // Refusal below the shared floor.
    let p = VarProvider::new(total, total);
    let mut be = SparseVramBackend::new_with_limits(
        &p,
        total,
        4 * 1024 * 1024,
        4096,
        policy(configured),
        None,
    )
    .unwrap();
    be.write_at(0, &[1u8; 4096]).unwrap();
    let freed_tight = be
        .try_reclaim(0, Some(floor - 1), floor, Duration::from_secs(9999))
        .unwrap();
    assert!(freed_tight > 0, "probe must reclaim below the shared floor");
    // A free reading at or above the shared floor must not reclaim on the
    // floor condition alone (only on idle, which is held off here).
    be.write_at(0, &[1u8; 4096]).unwrap();
    let freed_healthy = be
        .try_reclaim(0, Some(floor), floor, Duration::from_secs(9999))
        .unwrap();
    assert_eq!(
        freed_healthy, 0,
        "no floor-triggered reclaim at or above the shared floor"
    );
}
