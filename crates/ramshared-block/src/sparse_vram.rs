// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/sparse_vram.rs
//! Sparse VRAM block backend — capacity without full pre-alloc.
//!
//! Allocates CUDA/provider chunks on first **write**; Empty ranges read as zeros.
//! Free Live chunks only when the NBD swap device is empty (`used_kb == 0`) or on drop/down.

use std::time::{Duration, Instant};

use ramshared_vram::{
    RUNTIME_FREE_BUFFER_BYTES, ReserveFloorEnv, ReserveFloorPolicy, VramError, VramMemory,
    VramProvider,
};

use crate::{BlockBackend, IoError};

/// Default chunk size (MiB) — SPEC `RAMSHARED_VRAM_CHUNK_MIB` default 128.
pub const DEFAULT_CHUNK_MIB: u64 = 128;

/// Host-authoritative admission check invoked before a physical VRAM commit.
pub trait CommitBudgetGate {
    fn allow_commit(&self, committed: u64, next_chunk: u64) -> Result<(), String>;
}

/// Named sparse allocation and safety policy.
pub struct SparseVramConfig<'p> {
    pub capacity: u64,
    pub chunk_bytes: u64,
    pub block_size: u32,
    /// The one configured-reserve authority (DT-1). Resolved once at startup
    /// and passed by value to every admission surface.
    pub reserve_policy: ReserveFloorPolicy,
    pub commit_cap_bytes: Option<u64>,
    pub budget_gate: Option<&'p dyn CommitBudgetGate>,
}

/// One sparse slot.
struct Chunk<'p, P: VramProvider + 'p> {
    mem: Option<P::Mem<'p>>,
    written: bool,
    last_write: Option<Instant>,
}

/// Block device: advertised `capacity`, physical commit in `chunk_bytes` units.
pub struct SparseVramBackend<'p, P: VramProvider + 'p> {
    provider: &'p P,
    capacity: u64,
    chunk_bytes: u64,
    block_size: u32,
    /// Never allocate if `mem_info.free < reserve_floor + chunk` (keep GPU headroom).
    /// The one configured-reserve authority (DT-1).
    reserve_policy: ReserveFloorPolicy,
    /// Hard cap on sum of Live chunks (≤ capacity). Protects 6 GiB cards from full fill.
    commit_cap_bytes: u64,
    budget_gate: Option<&'p dyn CommitBudgetGate>,
    chunks: Vec<Chunk<'p, P>>,
    /// Telemetry counters.
    pub alloc_fails: u64,
    pub reclaim_frees: u64,
    pub floor_refuses: u64,
    pub budget_refuses: u64,
}

impl<'p, P: VramProvider + 'p> SparseVramBackend<'p, P> {
    /// Build empty sparse map (no provider alloc except later canary outside).
    pub fn new(
        provider: &'p P,
        capacity: u64,
        chunk_bytes: u64,
        block_size: u32,
    ) -> Result<Self, IoError> {
        Self::new_with_config(
            provider,
            SparseVramConfig {
                capacity,
                chunk_bytes,
                block_size,
                reserve_policy: sealed_reserve_policy(),
                commit_cap_bytes: None,
                budget_gate: None,
            },
        )
    }

    /// Same as [`new`] with explicit safety limits (tests + daemon).
    pub fn new_with_limits(
        provider: &'p P,
        capacity: u64,
        chunk_bytes: u64,
        block_size: u32,
        reserve_policy: ReserveFloorPolicy,
        commit_cap_bytes: Option<u64>,
    ) -> Result<Self, IoError> {
        Self::new_with_config(
            provider,
            SparseVramConfig {
                capacity,
                chunk_bytes,
                block_size,
                reserve_policy,
                commit_cap_bytes,
                budget_gate: None,
            },
        )
    }

    /// Convenience constructor for callers that supply every safety boundary.
    pub fn new_with_limits_and_gate(
        provider: &'p P,
        capacity: u64,
        chunk_bytes: u64,
        block_size: u32,
        reserve_policy: ReserveFloorPolicy,
        commit_cap_bytes: Option<u64>,
        budget_gate: Option<&'p dyn CommitBudgetGate>,
    ) -> Result<Self, IoError> {
        Self::new_with_config(
            provider,
            SparseVramConfig {
                capacity,
                chunk_bytes,
                block_size,
                reserve_policy,
                commit_cap_bytes,
                budget_gate,
            },
        )
    }

    pub fn new_with_config(provider: &'p P, config: SparseVramConfig<'p>) -> Result<Self, IoError> {
        if config.capacity == 0 {
            return Err(IoError("sparse: capacity 0".into()));
        }
        if config.chunk_bytes == 0
            || !config
                .chunk_bytes
                .is_multiple_of(u64::from(config.block_size))
        {
            return Err(IoError(format!(
                "sparse: chunk_bytes={} must be >0 and multiple of block_size={}",
                config.chunk_bytes, config.block_size
            )));
        }
        let n = config.capacity.div_ceil(config.chunk_bytes);
        if n > 1_000_000 {
            return Err(IoError(format!("sparse: too many chunks ({n})")));
        }
        // Cap commit to capacity; optional env can lower further.
        let commit_cap = config
            .commit_cap_bytes
            .unwrap_or_else(commit_cap_bytes_from_env)
            .min(config.capacity)
            .max(config.chunk_bytes);
        let mut chunks = Vec::with_capacity(n as usize);
        for _ in 0..n {
            chunks.push(Chunk {
                mem: None,
                written: false,
                last_write: None,
            });
        }
        Ok(Self {
            provider,
            capacity: config.capacity,
            chunk_bytes: config.chunk_bytes,
            block_size: config.block_size,
            reserve_policy: config.reserve_policy,
            commit_cap_bytes: commit_cap,
            budget_gate: config.budget_gate,
            chunks,
            alloc_fails: 0,
            reclaim_frees: 0,
            floor_refuses: 0,
            budget_refuses: 0,
        })
    }

    pub fn commit_cap_bytes(&self) -> u64 {
        self.commit_cap_bytes
    }

    pub fn reserve_policy(&self) -> ReserveFloorPolicy {
        self.reserve_policy
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    pub fn chunk_bytes(&self) -> u64 {
        self.chunk_bytes
    }

    pub fn chunks_total(&self) -> usize {
        self.chunks.len()
    }

    pub fn chunks_live(&self) -> usize {
        self.chunks.iter().filter(|c| c.mem.is_some()).count()
    }

    pub fn committed_bytes(&self) -> u64 {
        self.chunks_live() as u64 * self.chunk_bytes
    }

    /// Free all Live chunks (caller must ensure nbd used_kb == 0 or shutdown).
    pub fn free_all_live(&mut self) -> u64 {
        let mut freed = 0u64;
        for c in &mut self.chunks {
            if c.mem.take().is_some() {
                freed = freed.saturating_add(self.chunk_bytes);
                c.written = false;
                c.last_write = None;
                self.reclaim_frees = self.reclaim_frees.saturating_add(1);
            }
        }
        freed
    }

    /// MVP reclaim: only when `nbd_used_kb == 0` and (free below floor or idle).
    ///
    /// `free_floor_bytes` must be the shared three-term floor from
    /// `enforced_free_floor_from_configured` — the same value admission uses
    /// (DT-5). Callers must not pass the configured reserve alone.
    ///
    /// Returns bytes freed. Never frees when `nbd_used_kb > 0` (corruption class).
    pub fn try_reclaim(
        &mut self,
        nbd_used_kb: u64,
        free_vram_bytes: Option<u64>,
        free_floor_bytes: u64,
        idle: Duration,
    ) -> Result<u64, IoError> {
        if nbd_used_kb > 0 {
            return Ok(0);
        }
        let below_floor = free_vram_bytes.is_some_and(|f| f < free_floor_bytes);
        let now = Instant::now();
        // `last_write` is set on write and cleared only by `free_all_live`,
        // which also drops `mem`. So a chunk that is in this filter with no
        // `last_write` was allocated and never written: it holds no dirty
        // data, so there is nothing to write back and it is idle by
        // definition. That is not an unknown write time being read as old.
        let idle_ok = self.chunks.iter().any(|c| c.mem.is_some())
            && self.chunks.iter().filter(|c| c.mem.is_some()).all(|c| {
                c.last_write
                    .map(|t| now.duration_since(t) >= idle)
                    .unwrap_or(true)
            });
        if below_floor || idle_ok {
            return Ok(self.free_all_live());
        }
        Ok(0)
    }

    fn ensure_live(&mut self, idx: usize) -> Result<(), IoError> {
        let Some(chunk) = self.chunks.get(idx) else {
            return Err(IoError(format!(
                "sparse page table oob idx={idx} len={}",
                self.chunks.len()
            )));
        };
        if chunk.mem.is_some() {
            return Ok(());
        }
        // Commit cap: do not fill past safe physical budget (capacity may be 6G on 6G GPU).
        let next_commit = self.committed_bytes().saturating_add(self.chunk_bytes);
        if let Some(gate) = self.budget_gate
            && let Err(message) = gate.allow_commit(self.committed_bytes(), self.chunk_bytes)
        {
            self.budget_refuses = self.budget_refuses.saturating_add(1);
            return Err(IoError(format!(
                "sparse host budget constrained before allocation: {message}"
            )));
        }
        if next_commit > self.commit_cap_bytes {
            self.floor_refuses = self.floor_refuses.saturating_add(1);
            return Err(IoError(format!(
                "sparse commit_cap: committed would be {} MiB > cap {} MiB (capacity {} MiB); \
                 refusing the write because swap fallback is not guaranteed",
                next_commit >> 20,
                self.commit_cap_bytes >> 20,
                self.capacity >> 20
            )));
        }
        // Free-floor: never take the last reserve of GPU (desktop/game headroom).
        //
        // DT-5 of `gpu-reserve-floor-authority`: the threshold is the shared
        // three-term floor `required_free_bytes(configured, RUNTIME_FREE_BUFFER_BYTES)`,
        // not the configured reserve alone. Before this, the sparse tier dropped
        // both the 20% capacity share and the 640 MiB runtime buffer — the most
        // permissive production surface.
        match self.provider.mem_info() {
            Ok((free, total)) => {
                let capacity = ReserveFloorPolicy::helper_capacity(Some(total), total);
                let floor = self
                    .reserve_policy
                    .enforced_free_floor_bytes(capacity, RUNTIME_FREE_BUFFER_BYTES);
                let need = floor.saturating_add(self.chunk_bytes);
                if free < need {
                    self.floor_refuses = self.floor_refuses.saturating_add(1);
                    return Err(IoError(format!(
                        "sparse free-floor: free {} MiB < shared-floor+chunk {} MiB — refuse alloc \
                         (protect GPU)",
                        free >> 20,
                        need >> 20
                    )));
                }
            }
            Err(e) => {
                self.alloc_fails = self.alloc_fails.saturating_add(1);
                return Err(IoError(format!("sparse mem_info: {e}")));
            }
        }
        let len = self.chunk_bytes as usize;
        // Last chunk may be partial capacity — still alloc full chunk_bytes (simpler MVP).
        let mut m = match self.provider.alloc(len) {
            Ok(m) => m,
            Err(e) => {
                self.alloc_fails = self.alloc_fails.saturating_add(1);
                return Err(IoError(format!("sparse alloc chunk {idx}: {e}")));
            }
        };
        m.zero().map_err(|e| IoError(e.to_string()))?;
        let Some(chunk) = self.chunks.get_mut(idx) else {
            return Err(IoError(format!(
                "sparse page table oob idx={idx} len={}",
                self.chunks.len()
            )));
        };
        chunk.mem = Some(m);
        Ok(())
    }

    fn chunk_index(&self, off: u64) -> Result<usize, IoError> {
        if off >= self.capacity {
            return Err(IoError(format!(
                "sparse oob off={off} capacity={}",
                self.capacity
            )));
        }
        Ok((off / self.chunk_bytes) as usize)
    }
}

fn physical_range_fits(physical_len: usize, relative: usize, transfer_len: usize) -> bool {
    relative
        .checked_add(transfer_len)
        .is_some_and(|end| end <= physical_len)
}

impl<'p, P: VramProvider + 'p> BlockBackend for SparseVramBackend<'p, P> {
    fn size_bytes(&self) -> u64 {
        self.capacity
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), IoError> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = off
            .checked_add(buf.len() as u64)
            .filter(|&e| e <= self.capacity)
            .ok_or_else(|| {
                IoError(format!(
                    "sparse read oob off={off} len={} cap={}",
                    buf.len(),
                    self.capacity
                ))
            })?;
        let _ = end;
        let mut done = 0usize;
        while done < buf.len() {
            let abs = off + done as u64;
            let idx = self.chunk_index(abs)?;
            let chunk_base = idx as u64 * self.chunk_bytes;
            let rel = (abs - chunk_base) as usize;
            let room = (self.chunk_bytes as usize).saturating_sub(rel);
            let n = (buf.len() - done).min(room);
            let Some(chunk) = self.chunks.get(idx) else {
                return Err(IoError(format!(
                    "sparse page table oob idx={idx} len={}",
                    self.chunks.len()
                )));
            };
            if let Some(m) = &chunk.mem {
                if !physical_range_fits(m.len(), rel, n) {
                    return Err(IoError(format!(
                        "sparse physical read oob rel={rel} len={n} phys_len={}",
                        m.len()
                    )));
                }
                m.read_at(rel as u64, &mut buf[done..done + n])
                    .map_err(|e: VramError| IoError(e.to_string()))?;
            } else {
                buf[done..done + n].fill(0);
            }
            done += n;
        }
        Ok(())
    }

    fn write_at(&mut self, off: u64, data: &[u8]) -> Result<(), IoError> {
        if data.is_empty() {
            return Ok(());
        }
        let end = off
            .checked_add(data.len() as u64)
            .filter(|&e| e <= self.capacity)
            .ok_or_else(|| {
                IoError(format!(
                    "sparse write oob off={off} len={} cap={}",
                    data.len(),
                    self.capacity
                ))
            })?;
        let _ = end;
        let mut done = 0usize;
        let now = Instant::now();
        while done < data.len() {
            let abs = off + done as u64;
            let idx = self.chunk_index(abs)?;
            self.ensure_live(idx)?;
            let chunk_base = idx as u64 * self.chunk_bytes;
            let rel = (abs - chunk_base) as usize;
            let room = (self.chunk_bytes as usize).saturating_sub(rel);
            let n = (data.len() - done).min(room);
            let Some(chunk) = self.chunks.get_mut(idx) else {
                return Err(IoError(format!(
                    "sparse page table oob idx={idx} len={}",
                    self.chunks.len()
                )));
            };
            let m = chunk
                .mem
                .as_mut()
                .ok_or_else(|| IoError("sparse: mem missing after ensure".into()))?;
            if !physical_range_fits(m.len(), rel, n) {
                return Err(IoError(format!(
                    "sparse physical write oob rel={rel} len={n} phys_len={}",
                    m.len()
                )));
            }
            m.write_at(rel as u64, &data[done..done + n])
                .map_err(|e: VramError| IoError(e.to_string()))?;

            chunk.written = true;
            chunk.last_write = Some(now);
            done += n;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

/// Parse chunk MiB from env (SPEC bounds 16..512).
pub fn chunk_bytes_from_env() -> u64 {
    let mib = std::env::var("RAMSHARED_VRAM_CHUNK_MIB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CHUNK_MIB)
        .clamp(16, 512);
    mib.saturating_mul(1024 * 1024)
}

/// Idle free hysteresis seconds.
pub fn idle_free_secs_from_env() -> u64 {
    std::env::var("RAMSHARED_VRAM_IDLE_FREE_SEC")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(30)
        .clamp(1, 3600)
}

/// Sealed manifest literals for the configured reserve (DT-3).
///
/// Re-exported from `ramshared_vram::reserve_policy` — the single authority.
/// A path that has no manifest yet still enforces the sealed values instead of
/// an arbitrary default; a second literal here would be a drift hazard the
/// seal cannot detect.
pub use ramshared_vram::{SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT};

/// The sealed authority with no environment override applied.
///
/// Delegates to [`ReserveFloorPolicy::sealed`], the single constructor bound to
/// the sealed literals. Re-deriving `from_manifest(SEALED_RESERVE_MIN_MIB, …)`
/// here would be a second hand-rolled path to the same policy — the drift
/// hazard `reserve_policy` exists to prevent.
pub fn sealed_reserve_policy() -> ReserveFloorPolicy {
    ReserveFloorPolicy::sealed()
}

/// Sealed authority raised only by an environment override (DT-8).
///
/// Errors when the override is below the seal or the two documented names
/// disagree. Callers must fail closed — never clamp, never pick one.
pub fn sealed_reserve_policy_from_env()
-> Result<ReserveFloorPolicy, ramshared_vram::ReserveFloorError> {
    let base = sealed_reserve_policy();
    let env = ReserveFloorEnv::from_process_env();
    ReserveFloorPolicy::resolve_with_env(&base, &env)
}

/// Raise-only override input in MiB, or `None` when neither name is set.
///
/// No default and no clamp (DT-8): an operator may be more conservative than
/// the seal, never less, and the resolver is what enforces that.
pub fn reserve_floor_override_mib() -> Option<u64> {
    std::env::var("RAMSHARED_MIN_VRAM_FREE_MIB")
        .or_else(|_| std::env::var("MIN_VRAM_HEADROOM_MIB"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Optional hard commit cap (MiB). Unset → no extra cap beyond capacity (still free-floor).
pub fn commit_cap_bytes_from_env() -> u64 {
    if let Ok(s) = std::env::var("RAMSHARED_VRAM_COMMIT_CAP_MIB")
        && let Ok(mib) = s.trim().parse::<u64>()
    {
        return mib.clamp(256, 64 * 1024).saturating_mul(1024 * 1024);
    }
    // Default: huge (effectively capacity.min later)
    u64::MAX / 4
}

/// Safe commit budget: min(capacity, total_vram − reserve) when total known.
pub fn safe_commit_cap(capacity: u64, total_vram: u64, reserve: u64) -> u64 {
    let by_total = total_vram.saturating_sub(reserve);
    capacity.min(by_total).max(1)
}

#[cfg(test)]
mod tests;
